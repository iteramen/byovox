//! Hotkey events, the chord grammar, and the trait every hotkey backend implements.

use std::fmt;
use std::sync::mpsc::Sender;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HotkeyEvent {
    Pressed,
    Released,
    Toggle,
    Cancel,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HotkeyMode {
    Hold,
    Toggle,
}

impl HotkeyMode {
    pub fn parse(s: &str) -> Option<HotkeyMode> {
        match s {
            "hold" => Some(HotkeyMode::Hold),
            "toggle" => Some(HotkeyMode::Toggle),
            _ => None,
        }
    }
}

pub const KEY_NAMES: &[&str] = &[
    "ControlLeft",
    "ControlRight",
    "AltLeft",
    "AltRight",
    "ShiftLeft",
    "ShiftRight",
    "MetaLeft",
    "MetaRight",
    "CapsLock",
    "ScrollLock",
    "Pause",
    "Insert",
    "Escape",
    "F13",
    "F14",
    "F15",
    "F16",
    "F17",
    "F18",
    "F19",
    "F20",
    "F21",
    "F22",
    "F23",
    "F24",
];

pub fn validate_key_name(name: &str) -> Result<(), String> {
    if KEY_NAMES.contains(&name) {
        Ok(())
    } else {
        Err(format!(
            "unknown key `{name}`; accepted: {}",
            KEY_NAMES.join(", ")
        ))
    }
}

fn is_trigger_only(name: &str) -> bool {
    matches!(name.as_bytes(), [b] if b.is_ascii_uppercase() || b.is_ascii_digit())
}

fn unknown_chord_key(name: &str) -> String {
    format!(
        "unknown key `{name}`; accepted: {}, or A-Z / 0-9 as a chord's trigger",
        KEY_NAMES.join(", ")
    )
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chord {
    pub modifiers: Vec<String>,
    pub trigger: String,
}

impl fmt::Display for Chord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for m in &self.modifiers {
            write!(f, "{m}+")?;
        }
        f.write_str(&self.trigger)
    }
}

/// `hotkey.key`: one key name, or `M1+M2+...+T`, the last element being the trigger. Whitespace
/// around an element is ignored.
pub fn parse_chord(s: &str) -> Result<Chord, String> {
    let mut names: Vec<&str> = Vec::new();
    for raw in s.split('+') {
        let name = raw.trim();
        if name.is_empty() {
            return Err(format!(
                "empty element in `{s}`; expected a key name, or Modifier+...+Key"
            ));
        }
        if names.contains(&name) {
            return Err(format!("`{name}` appears twice in `{s}`"));
        }
        names.push(name);
    }
    let trigger = names.pop().expect("split yields at least one element");
    if !KEY_NAMES.contains(&trigger) {
        if !is_trigger_only(trigger) {
            return Err(unknown_chord_key(trigger));
        }
        if names.is_empty() {
            return Err(format!(
                "`{trigger}` needs a modifier: a bare letter or digit as the hotkey would \
                 swallow every one you type (e.g. `ControlLeft+ShiftLeft+{trigger}`)"
            ));
        }
    }
    for m in &names {
        if !KEY_NAMES.contains(m) {
            return Err(unknown_chord_key(m));
        }
    }
    Ok(Chord {
        modifiers: names.into_iter().map(String::from).collect(),
        trigger: trigger.to_string(),
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChordKey {
    Modifier(usize),
    Trigger,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Step {
    pub event: Option<HotkeyEvent>,
    pub swallow: bool,
}

pub struct ChordTracker {
    mods_down: Vec<bool>,
    /// The trigger went down as part of a satisfied chord and has not come up: its repeats and
    /// up are ours even after a modifier is released, or `zzzz` would leak.
    holding: bool,
    latched: bool,
}

impl ChordTracker {
    pub fn new(chord: &Chord) -> ChordTracker {
        ChordTracker {
            mods_down: vec![false; chord.modifiers.len()],
            holding: false,
            latched: false,
        }
    }

    /// A hotkey with no modifiers is passed through; swallowing the trigger is what a chord is for.
    fn swallows(&self) -> bool {
        !self.mods_down.is_empty()
    }

    /// True when the next trigger down would fire the chord on the strength of `mods_down` alone,
    /// so a backend can check its flags against the OS first: an event that never arrives leaves
    /// a flag stuck.
    pub fn would_fire(&self) -> bool {
        !self.holding && !self.mods_down.is_empty() && self.mods_down.iter().all(|d| *d)
    }

    /// `armed` (the daemon's Enable/Disable) gates only the `Pressed` that latches a chord. A
    /// chord already latched still finishes, or the window would get a `Z` up whose down it never
    /// saw. Skipping the tracker while disabled would leave a modifier stuck down.
    pub fn feed(&mut self, key: ChordKey, down: bool, armed: bool) -> Step {
        let pass = Step {
            event: None,
            swallow: false,
        };
        match key {
            ChordKey::Modifier(i) => {
                let Some(slot) = self.mods_down.get_mut(i) else {
                    // Unreachable (the backend maps codes from this chord), but a hook procedure
                    // must not panic: a release build passes the key through.
                    debug_assert!(false, "modifier {i} is not a member of this chord");
                    return pass;
                };
                *slot = down;
                if !down && self.latched {
                    // The combination stopped being held, so the hold is over whichever key
                    // left first. `holding` stays set: the trigger's own up is still ours.
                    self.latched = false;
                    return Step {
                        event: Some(HotkeyEvent::Released),
                        swallow: false,
                    };
                }
                pass
            }
            ChordKey::Trigger if down => {
                if self.holding {
                    return Step {
                        event: None,
                        swallow: self.swallows(),
                    };
                }
                if !armed || !self.mods_down.iter().all(|d| *d) {
                    return pass;
                }
                self.holding = true;
                self.latched = true;
                Step {
                    event: Some(HotkeyEvent::Pressed),
                    swallow: self.swallows(),
                }
            }
            ChordKey::Trigger => {
                if !self.holding {
                    // A key already down at startup, or another keyboard's up, must not end a
                    // dictation that never began.
                    return pass;
                }
                self.holding = false;
                let event = if self.latched {
                    self.latched = false;
                    Some(HotkeyEvent::Released)
                } else {
                    None
                };
                Step {
                    event,
                    swallow: self.swallows(),
                }
            }
        }
    }
}

pub trait Hotkey: Send {
    fn run(self: Box<Self>, tx: Sender<HotkeyEvent>);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::EXAMPLE;

    #[test]
    fn every_accepted_key_name_is_documented_in_the_example() {
        let missing: Vec<&str> = KEY_NAMES
            .iter()
            .copied()
            .filter(|name| !EXAMPLE.contains(name))
            .collect();
        assert!(
            missing.is_empty(),
            "accepted by hotkey::KEY_NAMES, absent from docs/config.example.toml: {missing:?}"
        );

        for range in ["A-Z", "0-9"] {
            assert!(
                EXAMPLE.contains(range),
                "trigger range {range} absent from docs/config.example.toml"
            );
        }
        for c in ('A'..='Z').chain('0'..='9') {
            assert!(
                parse_chord(&format!("ControlLeft+{c}")).is_ok(),
                "`{c}` is documented by a range but not accepted"
            );
        }
        for c in ['a', 'z', '!', 'ß'] {
            assert!(
                parse_chord(&format!("ControlLeft+{c}")).is_err(),
                "`{c}` is outside the documented ranges but accepted"
            );
        }
    }

    #[test]
    fn a_single_name_parses_as_a_chord_with_no_modifiers() {
        let c = parse_chord("ControlRight").unwrap();
        assert!(c.modifiers.is_empty());
        assert_eq!(c.trigger, "ControlRight");
        assert_eq!(c.to_string(), "ControlRight");
    }

    #[test]
    fn a_chord_keeps_its_modifiers_in_order_and_renders_as_written() {
        let c = parse_chord("ControlLeft+ShiftLeft+Z").unwrap();
        assert_eq!(c.modifiers, ["ControlLeft", "ShiftLeft"]);
        assert_eq!(c.trigger, "Z");
        assert_eq!(c.to_string(), "ControlLeft+ShiftLeft+Z");
        assert_eq!(
            parse_chord(" ControlLeft + Z ").unwrap().to_string(),
            "ControlLeft+Z"
        );
        assert_eq!(parse_chord("CapsLock+F13").unwrap().modifiers, ["CapsLock"]);
    }

    #[test]
    fn a_letter_or_digit_alone_is_refused_and_says_what_to_do() {
        for bare in ["Z", "7"] {
            let e = parse_chord(bare).unwrap_err();
            assert!(e.contains("needs a modifier"), "{bare}: {e}");
            assert!(e.contains("ControlLeft+ShiftLeft+"), "{bare}: {e}");
        }
        assert!(parse_chord("ControlLeft+7").is_ok());
    }

    #[test]
    fn an_unknown_name_is_refused_wherever_it_sits() {
        for bad in ["Nope", "Nope+Z", "ControlLeft+nope", "ControlLeft+z"] {
            let e = parse_chord(bad).unwrap_err();
            assert!(e.starts_with("unknown key `"), "{bad}: {e}");
            // The accepted list is what turns the rejection into an answer.
            assert!(e.contains("ControlRight"), "{bad}: {e}");
        }
    }

    #[test]
    fn the_same_key_twice_is_refused() {
        for bad in ["ControlLeft+ControlLeft+Z", "ControlLeft+Z+Z"] {
            let e = parse_chord(bad).unwrap_err();
            assert!(e.contains("appears twice"), "{bad}: {e}");
        }
    }

    #[test]
    fn an_empty_element_is_refused() {
        for bad in ["ControlLeft++Z", "+Z", "ControlLeft+", "", "+"] {
            let e = parse_chord(bad).unwrap_err();
            assert!(e.starts_with("empty element in `"), "{bad:?}: {e}");
        }
    }

    const CTRL: ChordKey = ChordKey::Modifier(0);
    const SHIFT: ChordKey = ChordKey::Modifier(1);
    const TRIGGER: ChordKey = ChordKey::Trigger;

    fn tracker(key: &str) -> ChordTracker {
        ChordTracker::new(&parse_chord(key).unwrap())
    }

    fn feed(t: &mut ChordTracker, key: ChordKey, down: bool) -> (Option<HotkeyEvent>, bool) {
        let s = t.feed(key, down, true);
        (s.event, s.swallow)
    }

    fn feed_disarmed(
        t: &mut ChordTracker,
        key: ChordKey,
        down: bool,
    ) -> (Option<HotkeyEvent>, bool) {
        let s = t.feed(key, down, false);
        (s.event, s.swallow)
    }

    #[test]
    fn the_trigger_fires_on_top_of_the_modifiers_and_is_swallowed() {
        let mut t = tracker("ControlLeft+ShiftLeft+Z");
        assert_eq!(feed(&mut t, CTRL, true), (None, false));
        assert_eq!(feed(&mut t, SHIFT, true), (None, false));
        assert_eq!(
            feed(&mut t, TRIGGER, true),
            (Some(HotkeyEvent::Pressed), true)
        );
        assert_eq!(
            feed(&mut t, TRIGGER, false),
            (Some(HotkeyEvent::Released), true)
        );
        assert_eq!(feed(&mut t, SHIFT, false), (None, false));
        assert_eq!(feed(&mut t, CTRL, false), (None, false));
    }

    #[test]
    fn auto_repeat_of_the_trigger_reports_once_and_types_nothing() {
        let mut t = tracker("ControlLeft+Z");
        feed(&mut t, CTRL, true);
        assert_eq!(
            feed(&mut t, TRIGGER, true),
            (Some(HotkeyEvent::Pressed), true)
        );
        for _ in 0..5 {
            assert_eq!(feed(&mut t, TRIGGER, true), (None, true));
        }
        assert_eq!(
            feed(&mut t, TRIGGER, false),
            (Some(HotkeyEvent::Released), true)
        );
    }

    #[test]
    fn the_trigger_without_its_modifiers_types() {
        let mut t = tracker("ControlLeft+ShiftLeft+Z");
        for _ in 0..3 {
            assert_eq!(feed(&mut t, TRIGGER, true), (None, false));
            assert_eq!(feed(&mut t, TRIGGER, false), (None, false));
        }
        feed(&mut t, CTRL, true);
        assert_eq!(feed(&mut t, TRIGGER, true), (None, false));
        assert_eq!(feed(&mut t, TRIGGER, false), (None, false));
    }

    #[test]
    fn a_modifier_released_first_ends_the_hold_and_the_trigger_stays_swallowed() {
        let mut t = tracker("ControlLeft+ShiftLeft+Z");
        feed(&mut t, CTRL, true);
        feed(&mut t, SHIFT, true);
        assert_eq!(
            feed(&mut t, TRIGGER, true),
            (Some(HotkeyEvent::Pressed), true)
        );
        assert_eq!(
            feed(&mut t, SHIFT, false),
            (Some(HotkeyEvent::Released), false)
        );
        assert_eq!(feed(&mut t, TRIGGER, true), (None, true), "a repeat");
        assert_eq!(feed(&mut t, CTRL, false), (None, false));
        assert_eq!(
            feed(&mut t, TRIGGER, false),
            (None, true),
            "swallowed, and no second Released"
        );
    }

    #[test]
    fn re_pressing_a_modifier_over_a_held_trigger_starts_nothing() {
        let mut t = tracker("ControlLeft+Z");
        feed(&mut t, CTRL, true);
        assert_eq!(
            feed(&mut t, TRIGGER, true),
            (Some(HotkeyEvent::Pressed), true)
        );
        assert_eq!(
            feed(&mut t, CTRL, false),
            (Some(HotkeyEvent::Released), false)
        );
        assert_eq!(feed(&mut t, CTRL, true), (None, false));
        assert_eq!(
            feed(&mut t, TRIGGER, true),
            (None, true),
            "no second Pressed"
        );
        assert_eq!(feed(&mut t, TRIGGER, false), (None, true));
        assert_eq!(
            feed(&mut t, TRIGGER, true),
            (Some(HotkeyEvent::Pressed), true)
        );
    }

    #[test]
    fn the_chord_pressed_twice_is_two_dictations() {
        let mut t = tracker("ControlLeft+ShiftLeft+Z");
        for _ in 0..2 {
            feed(&mut t, CTRL, true);
            feed(&mut t, SHIFT, true);
            assert_eq!(
                feed(&mut t, TRIGGER, true),
                (Some(HotkeyEvent::Pressed), true)
            );
            assert_eq!(
                feed(&mut t, TRIGGER, false),
                (Some(HotkeyEvent::Released), true)
            );
            feed(&mut t, SHIFT, false);
            feed(&mut t, CTRL, false);
        }
    }

    #[test]
    fn a_hotkey_with_no_modifiers_behaves_as_a_bare_key_always_has() {
        let mut t = tracker("ControlRight");
        assert_eq!(
            feed(&mut t, TRIGGER, true),
            (Some(HotkeyEvent::Pressed), false)
        );
        assert_eq!(feed(&mut t, TRIGGER, true), (None, false), "auto-repeat");
        assert_eq!(
            feed(&mut t, TRIGGER, false),
            (Some(HotkeyEvent::Released), false)
        );
        // An up with no press before it must not end a dictation that never began.
        assert_eq!(feed(&mut t, TRIGGER, false), (None, false));
    }

    #[test]
    fn a_resync_clears_stale_modifiers_before_they_can_eat_the_trigger() {
        let mut t = tracker("ControlLeft+ShiftLeft+Z");
        feed(&mut t, CTRL, true);
        assert!(!t.would_fire(), "one modifier short");
        feed(&mut t, SHIFT, true);
        assert!(
            t.would_fire(),
            "the backend checks the OS at exactly this point"
        );

        assert_eq!(feed(&mut t, CTRL, false), (None, false));
        assert_eq!(feed(&mut t, SHIFT, false), (None, false));
        assert!(!t.would_fire());
        assert_eq!(
            feed(&mut t, TRIGGER, true),
            (None, false),
            "a plain z types"
        );
        assert_eq!(feed(&mut t, TRIGGER, false), (None, false));

        feed(&mut t, CTRL, true);
        feed(&mut t, SHIFT, true);
        assert_eq!(feed(&mut t, CTRL, true), (None, false), "still held");
        assert_eq!(feed(&mut t, SHIFT, true), (None, false));
        assert_eq!(
            feed(&mut t, TRIGGER, true),
            (Some(HotkeyEvent::Pressed), true)
        );
    }

    #[test]
    fn a_held_chord_and_a_bare_key_never_ask_for_a_resync() {
        let mut t = tracker("ControlLeft+Z");
        feed(&mut t, CTRL, true);
        feed(&mut t, TRIGGER, true);
        assert!(!t.would_fire(), "already holding");

        let mut bare = tracker("ControlRight");
        assert!(!bare.would_fire());
        feed(&mut bare, TRIGGER, true);
        assert!(!bare.would_fire());
    }

    #[test]
    fn a_disarmed_chord_types_its_trigger_and_starts_nothing() {
        let mut t = tracker("ControlLeft+ShiftLeft+Z");
        assert_eq!(feed_disarmed(&mut t, CTRL, true), (None, false));
        assert_eq!(feed_disarmed(&mut t, SHIFT, true), (None, false));
        assert_eq!(feed_disarmed(&mut t, TRIGGER, true), (None, false));
        assert_eq!(
            feed_disarmed(&mut t, TRIGGER, true),
            (None, false),
            "repeat"
        );
        assert_eq!(feed_disarmed(&mut t, TRIGGER, false), (None, false));
        assert_eq!(feed_disarmed(&mut t, SHIFT, false), (None, false));
        assert_eq!(feed_disarmed(&mut t, CTRL, false), (None, false));
    }

    #[test]
    fn a_chord_latched_before_disable_still_finishes_its_swallow() {
        let mut t = tracker("ControlLeft+ShiftLeft+Z");
        feed(&mut t, CTRL, true);
        feed(&mut t, SHIFT, true);
        assert_eq!(
            feed(&mut t, TRIGGER, true),
            (Some(HotkeyEvent::Pressed), true)
        );
        assert_eq!(feed_disarmed(&mut t, TRIGGER, true), (None, true), "repeat");
        assert_eq!(
            feed_disarmed(&mut t, TRIGGER, false),
            (Some(HotkeyEvent::Released), true),
            "one Released, and the up is still swallowed"
        );
        assert_eq!(feed_disarmed(&mut t, TRIGGER, true), (None, false));
        assert_eq!(feed_disarmed(&mut t, TRIGGER, false), (None, false));
        assert_eq!(
            feed(&mut t, TRIGGER, true),
            (Some(HotkeyEvent::Pressed), true)
        );
        assert_eq!(
            feed(&mut t, TRIGGER, false),
            (Some(HotkeyEvent::Released), true)
        );
    }

    #[test]
    fn a_modifier_released_while_disarmed_still_clears() {
        let mut t = tracker("ControlLeft+Z");
        feed(&mut t, CTRL, true);
        assert_eq!(feed_disarmed(&mut t, CTRL, false), (None, false));
        assert_eq!(feed(&mut t, TRIGGER, true), (None, false));
        assert_eq!(feed(&mut t, TRIGGER, false), (None, false));
    }

    #[test]
    fn a_trigger_up_with_no_press_before_it_reports_nothing() {
        let mut t = tracker("ControlLeft+Z");
        assert_eq!(feed(&mut t, CTRL, true), (None, false));
        assert_eq!(feed(&mut t, TRIGGER, false), (None, false));
        assert_eq!(feed(&mut t, CTRL, false), (None, false));
    }
}
