//! Platform backends and the runtime selection between them.

use anyhow::Result;

use crate::capture::Capture;
use crate::config::Config;
use crate::hotkey::{Hotkey, parse_chord, validate_key_name};
use crate::inject::{Inject, InjectMode, Target};
use crate::layout::Layout;

#[cfg(windows)]
pub mod windows;

pub struct BackendNames {
    pub hotkey: &'static str,
    pub layout: &'static str,
    pub rungs: Vec<&'static str>,
}

pub struct Backends {
    pub hotkey: Box<dyn Hotkey>,
    pub capture: Box<dyn Capture>,
    pub layout: Box<dyn Layout>,
    pub rungs: Vec<Box<dyn Inject>>,
    pub target: Box<dyn Target>,
    pub names: BackendNames,
}

pub fn detect(cfg: &Config) -> Result<Backends> {
    let mode = validate(cfg)?;
    platform_detect(cfg, mode)
}

pub enum Availability {
    Free,
    Taken,
    Unknown(String),
}

/// Ask the platform whether `chord` is already somebody else's hotkey.
/// Advisory in both directions: see `windows::hotkey::claimed_elsewhere` for what it can and cannot see.
pub fn hotkey_availability(chord: &crate::hotkey::Chord) -> Availability {
    #[cfg(windows)]
    match windows::hotkey::claimed_elsewhere(chord) {
        Ok(true) => Availability::Taken,
        Ok(false) => Availability::Free,
        Err(why) => Availability::Unknown(why),
    }
    #[cfg(not(windows))]
    {
        let _ = chord;
        Availability::Unknown("this platform has no way to ask".into())
    }
}

/// Every refusal `detect` makes before it opens a device, and the inject mode it settled on.
/// Opens nothing, so the CLI can call it before spawning a daemon whose stderr goes nowhere.
pub fn validate(cfg: &Config) -> Result<InjectMode> {
    let chord = parse_chord(&cfg.hotkey.key).map_err(|e| anyhow::anyhow!("hotkey.key: {e}"))?;
    validate_key_name(&cfg.hotkey.cancel_key)
        .map_err(|e| anyhow::anyhow!("hotkey.cancel_key: {e}"))?;
    // The cancel key must differ from every chord element (while the chord is held its branch
    // wins). A rule about names, so it is checked here for every platform.
    let cancel = &cfg.hotkey.cancel_key;
    if chord.trigger == *cancel || chord.modifiers.iter().any(|m| m == cancel) {
        anyhow::bail!(if chord.modifiers.is_empty() {
            format!(
                "hotkey and cancel key are both `{}`; they must differ",
                cfg.hotkey.key
            )
        } else {
            format!("cancel key `{cancel}` is part of the hotkey `{chord}`; they must differ")
        });
    }
    let mode = InjectMode::parse(&cfg.inject.mode).ok_or_else(|| {
        anyhow::anyhow!(
            "inject.mode `{}`: expected auto | type | paste | clipboard-only",
            cfg.inject.mode
        )
    })?;
    platform_validate(cfg)?;
    Ok(mode)
}

#[cfg(windows)]
fn platform_validate(cfg: &Config) -> Result<()> {
    windows::hotkey::HookHotkey::new(&cfg.hotkey.key, &cfg.hotkey.cancel_key)
        .map(|_| ())
        .map_err(anyhow::Error::msg)
}

#[cfg(not(windows))]
fn platform_validate(_cfg: &Config) -> Result<()> {
    Ok(())
}

#[cfg(windows)]
fn rungs_for(mode: InjectMode) -> Vec<Box<dyn Inject>> {
    use windows::inject::{ClipboardOnlyInject, PasteInject, TypeInject};

    match mode {
        InjectMode::Auto => vec![
            Box::new(TypeInject),
            Box::new(PasteInject),
            Box::new(ClipboardOnlyInject),
        ],
        InjectMode::Type => vec![Box::new(TypeInject)],
        InjectMode::Paste => vec![Box::new(PasteInject)],
        InjectMode::ClipboardOnly => vec![Box::new(ClipboardOnlyInject)],
    }
}

#[cfg(windows)]
fn platform_detect(cfg: &Config, mode: InjectMode) -> Result<Backends> {
    use windows::hotkey::HookHotkey;
    use windows::inject::WinTarget;
    use windows::layout::WinLayout;

    let hotkey =
        HookHotkey::new(&cfg.hotkey.key, &cfg.hotkey.cancel_key).map_err(anyhow::Error::msg)?;
    let capture =
        crate::capture::CpalCapture::open(&cfg.capture.device).map_err(anyhow::Error::msg)?;
    let rungs = rungs_for(mode);
    let names = BackendNames {
        hotkey: "hook",
        layout: "win32",
        rungs: rungs.iter().map(|r| r.name()).collect(),
    };
    Ok(Backends {
        hotkey: Box::new(hotkey),
        capture: Box::new(capture),
        layout: Box::new(WinLayout),
        rungs,
        target: Box::new(WinTarget::default()),
        names,
    })
}

#[cfg(not(windows))]
fn platform_detect(_cfg: &Config, _mode: InjectMode) -> Result<Backends> {
    anyhow::bail!("no backends for this platform yet: byovox runs on Windows only")
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use crate::config::Config;

    fn rejection(cfg: &Config) -> String {
        match detect(cfg) {
            Err(e) => e.to_string(),
            Ok(b) => panic!("expected a rejection, got rungs {:?}", b.names.rungs),
        }
    }

    #[test]
    #[ignore]
    fn default_config_selects_hook_win32_and_all_rungs() {
        let b = detect(&Config::default()).unwrap();
        assert_eq!(b.names.hotkey, "hook");
        assert_eq!(b.names.layout, "win32");
        assert_eq!(b.names.rungs, vec!["type", "paste", "clipboard-only"]);
    }

    #[test]
    #[ignore]
    fn pinned_mode_selects_one_rung_and_bad_mode_errors() {
        let mut c = Config::default();
        c.inject.mode = "paste".into();
        assert_eq!(detect(&c).unwrap().names.rungs, vec!["paste"]);
        c.inject.mode = "telepathy".into();
        assert!(detect(&c).is_err());
        c.inject.mode = "auto".into();
        c.hotkey.key = "Nope".into();
        assert!(rejection(&c).contains("Nope"));
    }

    #[test]
    fn every_mode_selects_its_rungs_in_order() {
        let names = |m: InjectMode| -> Vec<&'static str> {
            rungs_for(m).iter().map(|r| r.name()).collect()
        };
        assert_eq!(
            names(InjectMode::Auto),
            vec!["type", "paste", "clipboard-only"]
        );
        assert_eq!(names(InjectMode::Type), vec!["type"]);
        assert_eq!(names(InjectMode::Paste), vec!["paste"]);
        assert_eq!(names(InjectMode::ClipboardOnly), vec!["clipboard-only"]);
    }

    #[test]
    fn bad_mode_or_key_is_rejected_before_any_device_is_opened() {
        let mut c = Config::default();
        c.inject.mode = "telepathy".into();
        let e = rejection(&c);
        assert!(e.starts_with("inject.mode `telepathy`"), "{e}");

        let mut c = Config::default();
        c.hotkey.key = "Nope".into();
        let e = rejection(&c);
        assert!(e.starts_with("hotkey.key: "), "{e}");
        assert!(e.contains("Nope"), "{e}");

        let mut c = Config::default();
        c.hotkey.cancel_key = "Nope".into();
        let e = rejection(&c);
        assert!(e.starts_with("hotkey.cancel_key: "), "{e}");
        assert!(e.contains("Nope"), "{e}");
    }

    #[test]
    fn validate_accepts_the_default_config_without_opening_a_device() {
        assert_eq!(
            validate(&Config::default()).unwrap(),
            InjectMode::Auto,
            "the default inject.mode"
        );
    }

    #[test]
    fn same_key_for_hotkey_and_cancel_is_rejected() {
        let mut c = Config::default();
        c.hotkey.key = "Escape".into();
        c.hotkey.cancel_key = "Escape".into();
        let e = rejection(&c);
        assert!(e.contains("Escape") && e.contains("differ"), "{e}");
    }

    #[test]
    fn a_chord_is_validated_before_any_device_is_opened() {
        let mut c = Config::default();
        c.hotkey.key = "ControlLeft+ShiftLeft+Z".into();
        assert_eq!(validate(&c).unwrap(), InjectMode::Auto);

        c.hotkey.cancel_key = "ShiftLeft".into();
        let e = rejection(&c);
        assert!(e.contains("ShiftLeft") && e.contains("differ"), "{e}");

        let mut c = Config::default();
        c.hotkey.key = "Z".into();
        let e = rejection(&c);
        assert!(e.starts_with("hotkey.key: "), "{e}");
        assert!(e.contains("needs a modifier"), "{e}");
    }
}
