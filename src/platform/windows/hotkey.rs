//! WH_KEYBOARD_LL hook mapping virtual keys onto `ChordKey`s; the rules live in `hotkey::ChordTracker`.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Mutex, OnceLock};

use windows::Win32::Foundation::{ERROR_HOTKEY_ALREADY_REGISTERED, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, HOT_KEY_MODIFIERS, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT,
    KEYEVENTF_KEYUP, MOD_ALT, MOD_CONTROL, MOD_SHIFT, MOD_WIN, RegisterHotKey, SendInput,
    UnregisterHotKey, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetMessageW, KBDLLHOOKSTRUCT, MSG, SetWindowsHookExW,
    TranslateMessage, WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP, WM_SYSKEYDOWN, WM_SYSKEYUP,
};

use crate::hotkey::{
    Chord, ChordKey, ChordTracker, Hotkey, HotkeyEvent, Step, parse_chord, validate_key_name,
};

static SENDER: OnceLock<Mutex<Option<Sender<HotkeyEvent>>>> = OnceLock::new();
static CHORD: OnceLock<Mutex<Option<ChordHook>>> = OnceLock::new();
static CANCEL_VK: AtomicU32 = AtomicU32::new(0);
static ARMED: AtomicBool = AtomicBool::new(true);
/// True once a hook is live: a second `run` would chain a hook that doubles every event.
static INSTALLED: AtomicBool = AtomicBool::new(false);

/// Stamped into `dwExtraInfo` of every event `platform::windows::inject` sends, so the hook
/// can tell our own synthetic keys from the user's. The hook cannot filter `LLKHF_INJECTED`
/// instead: its own probe injects the keys it tests with.
pub const INJECT_MARKER: usize = 0x00B7_0B0C;

/// An unassigned virtual key, tapped once when a chord fires. Windows acts on a modifier
/// combination released without another key (Ctrl+Shift flips text direction or switches the
/// layout); the trigger is swallowed, so without this tap the app would see exactly that.
const DEFUSE_VK: u16 = 0xFF;

pub fn set_armed(on: bool) {
    ARMED.store(on, Ordering::Relaxed);
}

fn sender_slot() -> &'static Mutex<Option<Sender<HotkeyEvent>>> {
    SENDER.get_or_init(|| Mutex::new(None))
}

fn chord_slot() -> &'static Mutex<Option<ChordHook>> {
    CHORD.get_or_init(|| Mutex::new(None))
}

pub fn vk_for(name: &str) -> Option<u32> {
    Some(match name {
        "ControlLeft" => 0xA2,
        "ControlRight" => 0xA3,
        "AltLeft" => 0xA4,
        "AltRight" => 0xA5,
        "ShiftLeft" => 0xA0,
        "ShiftRight" => 0xA1,
        "MetaLeft" => 0x5B,
        "MetaRight" => 0x5C,
        "CapsLock" => 0x14,
        "ScrollLock" => 0x91,
        "Pause" => 0x13,
        "Insert" => 0x2D,
        "Escape" => 0x1B,
        // A chord's trigger: ASCII codes for `A`-`Z`/`0`-`9`; before the `F` arm so a bare `F` is the letter.
        c if c.len() == 1 => {
            let b = c.as_bytes()[0];
            if !b.is_ascii_uppercase() && !b.is_ascii_digit() {
                return None;
            }
            u32::from(b)
        }
        f if f.starts_with('F') => {
            let digits = &f[1..];
            if digits.len() != 2 || !digits.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            let n: u32 = digits.parse().ok()?;
            if (13..=24).contains(&n) {
                0x70 + n - 1
            } else {
                return None;
            }
        }
        _ => return None,
    })
}

pub struct HookHotkey {
    chord: Chord,
    vks: Vec<u32>,
    cancel_vk: u32,
}

fn hot_key_modifier(name: &str) -> Option<HOT_KEY_MODIFIERS> {
    Some(match name {
        "ControlLeft" | "ControlRight" => MOD_CONTROL,
        "AltLeft" | "AltRight" => MOD_ALT,
        "ShiftLeft" | "ShiftRight" => MOD_SHIFT,
        "MetaLeft" | "MetaRight" => MOD_WIN,
        _ => return None,
    })
}

const PROBE_ID: i32 = 0x0B70;

/// Advisory both ways: a hook-based claimant (AutoHotkey, Discord, byovox itself) is invisible,
/// and `false` holds only for this moment.
pub fn claimed_elsewhere(chord: &Chord) -> Result<bool, String> {
    if hot_key_modifier(&chord.trigger).is_some() {
        return Err(format!(
            "`{}` is a bare modifier, which Windows does not register as a hotkey at all — \
             nothing can claim it, and nothing can conflict with it",
            chord.trigger
        ));
    }
    let mut flags = HOT_KEY_MODIFIERS(0);
    for m in &chord.modifiers {
        flags |= hot_key_modifier(m)
            .ok_or_else(|| format!("`{m}` is not a modifier Windows can register a hotkey with"))?;
    }
    let vk =
        vk_for(&chord.trigger).ok_or_else(|| format!("no virtual key for `{}`", chord.trigger))?;
    // SAFETY: a plain registration on this thread, undone before returning.
    unsafe {
        match RegisterHotKey(None, PROBE_ID, flags, vk) {
            Ok(()) => {
                let _ = UnregisterHotKey(None, PROBE_ID);
                Ok(false)
            }
            Err(e) if e.code() == ERROR_HOTKEY_ALREADY_REGISTERED.to_hresult() => Ok(true),
            Err(e) => Err(format!("RegisterHotKey: {e}")),
        }
    }
}

impl HookHotkey {
    pub fn new(key: &str, cancel_key: &str) -> Result<HookHotkey, String> {
        let chord = parse_chord(key)?;
        validate_key_name(cancel_key)?;
        let mut vks = Vec::with_capacity(chord.modifiers.len() + 1);
        for name in chord
            .modifiers
            .iter()
            .chain(std::iter::once(&chord.trigger))
        {
            vks.push(vk_for(name).ok_or_else(|| format!("no virtual key for `{name}`"))?);
        }
        let cancel_vk =
            vk_for(cancel_key).ok_or_else(|| format!("no virtual key for `{cancel_key}`"))?;
        if vks.contains(&cancel_vk) {
            return Err(if chord.modifiers.is_empty() {
                format!("hotkey and cancel key are both `{key}`; they must differ")
            } else {
                format!(
                    "cancel key `{cancel_key}` is part of the hotkey `{chord}`; they must differ"
                )
            });
        }
        Ok(HookHotkey {
            chord,
            vks,
            cancel_vk,
        })
    }
}

struct ChordHook {
    vks: Vec<u32>,
    tracker: ChordTracker,
}

impl ChordHook {
    fn key_for(&self, vk: u32) -> Option<ChordKey> {
        let i = self.vks.iter().position(|v| *v == vk)?;
        if i + 1 == self.vks.len() {
            Some(ChordKey::Trigger)
        } else {
            Some(ChordKey::Modifier(i))
        }
    }
}

fn emit(ev: HotkeyEvent) {
    // Recovering from poisoning rather than unwrapping: a panic here would unwind out of
    // an `extern "system"` callback, which aborts the process.
    if let Some(m) = SENDER.get()
        && let Some(tx) = m.lock().unwrap_or_else(|p| p.into_inner()).as_ref()
    {
        let _ = tx.send(ev);
    }
}

fn defuse_inputs() -> [INPUT; 2] {
    [false, true].map(|up| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(DEFUSE_VK),
                wScan: 0,
                dwFlags: if up {
                    KEYEVENTF_KEYUP
                } else {
                    Default::default()
                },
                time: 0,
                dwExtraInfo: INJECT_MARKER,
            },
        },
    })
}

fn defuse_modifiers() {
    let inputs = defuse_inputs();
    // SAFETY: two well-formed INPUTs, size declared as the API requires.
    let _ = unsafe { SendInput(&inputs, size_of::<INPUT>() as i32) };
}

/// Releases the chord lock without emitting: `emit` takes a second lock, and a hook procedure
/// holding two risks deadlock.
fn step_for(vk: u32, down: bool) -> Option<Step> {
    let slot = CHORD.get()?;
    let mut guard = slot.lock().unwrap_or_else(|p| p.into_inner());
    let hook = guard.as_mut()?;
    let key = hook.key_for(vk)?;
    let armed = ARMED.load(Ordering::Relaxed);
    if key == ChordKey::Trigger && down && hook.tracker.would_fire() {
        for i in 0..hook.vks.len() - 1 {
            let vk = hook.vks[i];
            // SAFETY: a plain keyboard-state read, no arguments to get wrong.
            let held = unsafe { GetAsyncKeyState(vk as i32) } as u16 & 0x8000 != 0;
            hook.tracker.feed(ChordKey::Modifier(i), held, armed);
        }
    }
    Some(hook.tracker.feed(key, down, armed))
}

unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        // SAFETY: for WH_KEYBOARD_LL, lparam points at a KBDLLHOOKSTRUCT for the call's duration.
        let kb = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
        if kb.dwExtraInfo == INJECT_MARKER {
            return unsafe { CallNextHookEx(None, code, wparam, lparam) };
        }
        let msg = wparam.0 as u32;
        let is_down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
        let is_up = msg == WM_KEYUP || msg == WM_SYSKEYUP;
        if is_down || is_up {
            match step_for(kb.vkCode, is_down) {
                Some(step) => {
                    if let Some(ev) = step.event {
                        if ev == HotkeyEvent::Pressed && step.swallow {
                            defuse_modifiers();
                        }
                        emit(ev);
                    }
                    if step.swallow {
                        return LRESULT(1);
                    }
                }
                None if is_down && kb.vkCode == CANCEL_VK.load(Ordering::Relaxed) => {
                    emit(HotkeyEvent::Cancel);
                }
                None => {}
            }
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

impl Hotkey for HookHotkey {
    fn run(self: Box<Self>, tx: Sender<HotkeyEvent>) {
        if INSTALLED.load(Ordering::SeqCst) {
            tracing::error!("a keyboard hook is already installed; refusing to install a second");
            return;
        }
        let HookHotkey {
            chord,
            vks,
            cancel_vk,
        } = *self;
        let tracker = ChordTracker::new(&chord);
        *chord_slot().lock().unwrap_or_else(|p| p.into_inner()) = Some(ChordHook { vks, tracker });
        CANCEL_VK.store(cancel_vk, Ordering::Relaxed);
        *sender_slot().lock().unwrap_or_else(|p| p.into_inner()) = Some(tx);
        // SAFETY: standard hook installation; the hook is removed when the process exits.
        let hook = unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), None, 0) };
        if let Err(e) = hook {
            tracing::error!(error = %e, "SetWindowsHookExW failed");
            *sender_slot().lock().unwrap_or_else(|p| p.into_inner()) = None;
            return;
        }
        INSTALLED.store(true, Ordering::SeqCst);
        let mut msg = MSG::default();
        // SAFETY: plain message pump on this thread; GetMessageW returns 0 on WM_QUIT.
        unsafe {
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn w3c_names_map_to_virtual_keys() {
        assert_eq!(vk_for("ControlRight"), Some(0xA3));
        assert_eq!(vk_for("ControlLeft"), Some(0xA2));
        assert_eq!(vk_for("AltRight"), Some(0xA5));
        assert_eq!(vk_for("F13"), Some(0x7C));
        assert_eq!(vk_for("F24"), Some(0x87));
        assert_eq!(vk_for("Escape"), Some(0x1B));
        assert_eq!(vk_for("Nope"), None);
        assert_eq!(vk_for("F+13"), None);
        assert_eq!(vk_for("F0013"), None);
    }

    #[test]
    fn letters_and_digits_map_to_their_own_ascii_codes() {
        assert_eq!(vk_for("A"), Some(0x41));
        assert_eq!(vk_for("Z"), Some(0x5A));
        assert_eq!(vk_for("0"), Some(0x30));
        assert_eq!(vk_for("9"), Some(0x39));
        assert_eq!(vk_for("F"), Some(0x46));
        assert_eq!(vk_for("z"), None);
        assert_eq!(vk_for("+"), None);
        assert_eq!(vk_for(""), None);
    }

    #[test]
    fn every_accepted_key_name_has_a_vk() {
        for name in crate::hotkey::KEY_NAMES {
            assert!(vk_for(name).is_some(), "{name}");
        }
    }

    #[test]
    fn hotkey_and_cancel_key_must_differ() {
        match HookHotkey::new("ControlRight", "ControlRight") {
            Err(e) => assert!(e.contains("must differ"), "{e}"),
            Ok(_) => panic!("the same key for hotkey and cancel must be rejected"),
        }
        assert!(HookHotkey::new("ControlRight", "Escape").is_ok());
    }

    #[test]
    fn a_cancel_key_that_is_part_of_the_chord_is_refused() {
        for cancel in ["ControlLeft", "ShiftLeft", "Insert"] {
            match HookHotkey::new("ControlLeft+ShiftLeft+Insert", cancel) {
                Err(e) => {
                    assert!(e.contains("must differ"), "{cancel}: {e}");
                    assert!(e.contains("ControlLeft+ShiftLeft+Insert"), "{cancel}: {e}");
                }
                Ok(_) => panic!("`{cancel}` is part of the chord and must be refused"),
            }
        }
        assert!(HookHotkey::new("ControlLeft+ShiftLeft+Z", "Escape").is_ok());
    }

    #[test]
    fn the_chords_virtual_keys_map_back_to_its_members() {
        let h = HookHotkey::new("ControlLeft+ShiftLeft+Z", "Escape").unwrap();
        assert_eq!(h.vks, vec![0xA2, 0xA0, 0x5A]);
        let hook = ChordHook {
            tracker: ChordTracker::new(&h.chord),
            vks: h.vks,
        };
        assert_eq!(hook.key_for(0xA2), Some(ChordKey::Modifier(0)));
        assert_eq!(hook.key_for(0xA0), Some(ChordKey::Modifier(1)));
        assert_eq!(hook.key_for(0x5A), Some(ChordKey::Trigger));
        assert_eq!(hook.key_for(0x1B), None);
        assert_eq!(
            hook.key_for(0xA3),
            None,
            "the other Ctrl is not this chord's"
        );
    }

    #[test]
    fn the_defuse_tap_is_an_unassigned_key_stamped_with_the_marker() {
        for (i, input) in defuse_inputs().iter().enumerate() {
            assert_eq!(input.r#type, INPUT_KEYBOARD);
            // SAFETY: `defuse_inputs` builds keyboard inputs, so `ki` is the live union field.
            let ki = unsafe { input.Anonymous.ki };
            assert_eq!(
                ki.wVk,
                VIRTUAL_KEY(0xFF),
                "an assigned key would reach the app"
            );
            assert_eq!(ki.dwExtraInfo, INJECT_MARKER);
            assert_eq!(
                ki.dwFlags,
                if i == 1 {
                    KEYEVENTF_KEYUP
                } else {
                    Default::default()
                },
                "down then up"
            );
        }
    }

    #[test]
    #[ignore]
    fn injected_keys_drive_the_hook() {
        use std::sync::mpsc::{RecvTimeoutError, channel};
        use std::thread::sleep;
        use std::time::{Duration, Instant};
        use windows::Win32::UI::Input::KeyboardAndMouse::{
            INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY,
            KEYEVENTF_KEYUP, SendInput, VIRTUAL_KEY,
        };

        fn key(vk: u16, flags: KEYBD_EVENT_FLAGS) {
            let input = INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: VIRTUAL_KEY(vk),
                        wScan: 0,
                        dwFlags: flags,
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            };
            // SAFETY: one well-formed INPUT, size declared as the API requires.
            let sent = unsafe { SendInput(&[input], size_of::<INPUT>() as i32) };
            assert_eq!(sent, 1, "SendInput refused vk {vk:#04X}");
        }

        const RCONTROL: u16 = 0xA3;
        const ESCAPE: u16 = 0x1B;

        let (tx, rx) = channel();
        std::thread::spawn(move || {
            Box::new(HookHotkey::new("ControlRight", "Escape").unwrap()).run(tx)
        });

        let deadline = Instant::now() + Duration::from_secs(2);
        while !INSTALLED.load(Ordering::SeqCst) {
            assert!(Instant::now() < deadline, "hook not installed within 2 s");
            sleep(Duration::from_millis(10));
        }

        key(RCONTROL, KEYEVENTF_EXTENDEDKEY); // first down -> Pressed
        sleep(Duration::from_secs(2));
        key(RCONTROL, KEYEVENTF_EXTENDEDKEY); // stands in for auto-repeat -> nothing
        key(RCONTROL, KEYEVENTF_EXTENDEDKEY | KEYEVENTF_KEYUP); // up -> Released
        key(ESCAPE, KEYBD_EVENT_FLAGS(0)); // down -> Cancel
        key(ESCAPE, KEYEVENTF_KEYUP); // up -> nothing

        let mut got = Vec::new();
        loop {
            match rx.recv_timeout(Duration::from_secs(3)) {
                Ok(ev) => got.push(ev),
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => panic!("hook thread exited: {got:?}"),
            }
        }
        println!("observed: {got:?}");
        assert_eq!(
            got,
            vec![
                HotkeyEvent::Pressed,
                HotkeyEvent::Released,
                HotkeyEvent::Cancel
            ]
        );
    }
}
