//! Per-user autostart via HKCU\...\Run — no elevation, no installer.

use std::path::Path;

use anyhow::{Context, Result};
use winreg::RegKey;
use winreg::enums::{HKEY_CURRENT_USER, KEY_WRITE};

const RUN: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

/// The command line to register. `config` is absolutised: a `Run` value runs with an unrelated
/// working directory, so a relative path would resolve elsewhere at login.
fn command_line(exe: &Path, config: Option<&Path>) -> Result<String> {
    let Some(c) = config else {
        return Ok(format!("\"{}\"", exe.display()));
    };
    let absolute =
        std::path::absolute(c).with_context(|| format!("resolving --config {}", c.display()))?;
    Ok(format!(
        "\"{}\" --config \"{}\"",
        exe.display(),
        absolute.display()
    ))
}

/// What gets registered is `byovox-daemon`, never the CLI doing the registering: the CLI is
/// console-subsystem, so the Run key would flash a console window at every logon.
pub fn enable(config: Option<&Path>) -> Result<()> {
    register(&std::env::current_exe().context("current exe")?, config)
}

fn register(exe: &Path, config: Option<&Path>) -> Result<()> {
    let daemon = crate::daemon::daemon_exe(exe);
    // A wrong path fails silently at every logon, so refuse while someone is reading the output.
    if !daemon.exists() {
        anyhow::bail!(
            "{} not found — autostart would fail at every logon",
            daemon.display()
        );
    }
    let command = command_line(&daemon, config)?;
    // Registry errors don't name the key, so each call carries it.
    let (key, _) = RegKey::predef(HKEY_CURRENT_USER)
        .create_subkey(RUN)
        .with_context(|| format!(r"HKCU\{RUN}"))?;
    key.set_value("byovox", &command)
        .with_context(|| format!(r"HKCU\{RUN}\byovox"))?;
    Ok(())
}

pub fn is_enabled() -> Result<bool> {
    let key = match RegKey::predef(HKEY_CURRENT_USER).open_subkey(RUN) {
        Ok(key) => key,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e).with_context(|| format!(r"HKCU\{RUN}")),
    };
    match key.get_raw_value("byovox") {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).with_context(|| format!(r"HKCU\{RUN}\byovox")),
    }
}

pub fn disable() -> Result<()> {
    let key = match RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags(RUN, KEY_WRITE) {
        Ok(key) => key,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!(r"HKCU\{RUN}")),
    };
    match key.delete_value("byovox") {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!(r"HKCU\{RUN}\byovox")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_relative_config_is_absolutised_before_it_is_registered() {
        let exe = crate::daemon::daemon_exe(Path::new(r"C:\bin\byovox.exe"));
        assert_eq!(
            command_line(&exe, None).unwrap(),
            r#""C:\bin\byovox-daemon.exe""#
        );

        let relative = command_line(&exe, Some(Path::new("byovox.toml"))).unwrap();
        assert!(
            !relative.contains(r#"--config "byovox.toml""#),
            "still relative: {relative}"
        );
        let resolved = std::path::absolute("byovox.toml").unwrap();
        assert!(
            relative.ends_with(&format!("--config \"{}\"", resolved.display())),
            "{relative}"
        );

        let absolute = command_line(&exe, Some(Path::new(r"C:\somewhere\byovox.toml"))).unwrap();
        assert_eq!(
            absolute,
            r#""C:\bin\byovox-daemon.exe" --config "C:\somewhere\byovox.toml""#
        );
    }

    #[test]
    #[ignore]
    fn autostart_round_trips_the_run_key() {
        let dir = std::env::temp_dir().join(format!("byovox-autostart-{}", std::process::id()));
        std::fs::create_dir(&dir).expect("test dir");
        let cli = dir.join("byovox.exe");
        let exe = crate::daemon::daemon_exe(&cli);
        std::fs::write(&exe, b"").expect("stand-in daemon");

        let (key, _) = RegKey::predef(HKEY_CURRENT_USER)
            .create_subkey(RUN)
            .expect("open Run");
        let original: Option<String> = key.get_value("byovox").ok();
        println!("original Run\\byovox: {original:?}");

        register(&cli, None).expect("enable");
        let value: String = key.get_value("byovox").expect("byovox value");
        println!("after enable: {value}");
        assert_eq!(value, format!("\"{}\"", exe.display()));
        assert!(
            is_enabled().expect("is_enabled"),
            "enabled, but read as off"
        );

        let cfg = Path::new(r"C:\somewhere\byovox.toml");
        register(&cli, Some(cfg)).expect("enable with a config");
        let value: String = key.get_value("byovox").expect("byovox value");
        println!("after enable --config: {value}");
        assert_eq!(
            value,
            format!("\"{}\" --config \"{}\"", exe.display(), cfg.display())
        );

        disable().expect("disable");
        let after = key.get_value::<String, _>("byovox");
        println!("after disable: {after:?}");
        assert!(after.is_err(), "the value survived disable(): {after:?}");
        assert!(
            !is_enabled().expect("is_enabled"),
            "disabled, but read as on"
        );

        disable().expect("disable on a missing value is a no-op");
        println!("second disable: ok");
        std::fs::remove_file(&exe).expect("remove stand-in");
        std::fs::remove_dir(&dir).expect("remove test dir");

        if let Some(original) = original {
            key.set_value("byovox", &original).expect("restore");
            let back: String = key.get_value("byovox").expect("restored value");
            assert_eq!(back, original);
            println!("restored: {back}");
        }
    }
}
