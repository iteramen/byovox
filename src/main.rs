//! byovox — push-to-talk dictation against a speech-to-text server you run.
//! CLI entry: subcommand dispatch. The daemon itself lives in `byovox::daemon`.

use byovox::{check, config, daemon, hotkey_cmd, ipc, setup};
// Used only by the Windows autostart path.
#[cfg(windows)]
use byovox::platform;

use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "byovox",
    version = byovox::VERSION,
    about = "Push-to-talk dictation against a speech-to-text server you run"
)]
struct Cli {
    /// Config file (default: the platform config dir)
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the daemon in this console, logging to stderr as well as to the file
    Run,
    /// Start or stop recording in the running daemon
    Toggle,
    /// Stop the running daemon
    Quit,
    /// Pipeline state and last error of the running daemon
    Status,
    /// Print the most recent transcript held by the daemon
    Last,
    /// Ask for the endpoints (the speech and clean-up servers probed as named), write the
    /// config file, then run `check`
    Setup,
    /// Exercise every stage and report the backend rungs
    Check {
        /// Wait for Enter before exiting (used when launched from the tray)
        #[arg(long)]
        pause: bool,
    },
    /// Show or change the push-to-talk hotkey
    Hotkey {
        /// Bind this key or chord, e.g. F13 or ControlLeft+ShiftLeft+Z
        #[arg(long, value_name = "KEY")]
        set: Option<String>,
        /// hold = push to talk; toggle = press once to start, once to send
        #[arg(long, value_name = "MODE")]
        mode: Option<String>,
        /// The key that discards a dictation in progress
        #[arg(long, value_name = "KEY")]
        cancel: Option<String>,
        /// Print every key name a binding may use
        #[arg(long, conflicts_with_all = ["set", "mode", "cancel", "force"])]
        list: bool,
        /// Bind the key even though another application has registered it
        #[arg(long, requires = "set")]
        force: bool,
    },
    /// Print the effective configuration, or write the documented default file
    Config {
        /// Write the documented default file instead of printing the effective config
        #[arg(long)]
        init: bool,
    },
    /// Register or unregister per-user autostart
    Autostart {
        /// Start byovox at login, carrying any --config given here
        #[arg(long, conflicts_with = "disable")]
        enable: bool,
        /// Remove the login registration
        #[arg(long)]
        disable: bool,
    },
}

fn main() {
    let cli = Cli::parse();
    let given = cli.config.clone();
    let path = cli.config.unwrap_or_else(config::default_path);
    let result = match cli.cmd {
        None => spawn_daemon(given),
        Some(Cmd::Run) => daemon::run(daemon::Options {
            config_path: given,
            log_to_stderr: true,
        }),
        Some(Cmd::Toggle) => relay(ipc::Request::Toggle),
        Some(Cmd::Quit) => relay(ipc::Request::Quit),
        Some(Cmd::Status) => status(),
        Some(Cmd::Last) => last(),
        Some(Cmd::Setup) => setup_cmd(path, given),
        Some(Cmd::Check { pause }) => check_cmd(path, pause),
        Some(Cmd::Hotkey {
            set,
            mode,
            cancel,
            list,
            force,
        }) => hotkey_cmd_dispatch(path, set, mode, cancel, list, force),
        Some(Cmd::Config { init }) => config_cmd(path, init),
        Some(Cmd::Autostart { enable, disable }) => autostart(enable, disable, given),
    };
    if let Err(e) = result {
        eprintln!("byovox: {e:#}");
        std::process::exit(2);
    }
}

/// Detached so closing the terminal cannot take the tray icon with it; the daemon's stderr is `NUL`, so refusals are reported here.
fn spawn_daemon(config: Option<PathBuf>) -> Result<()> {
    daemon::preflight(config.as_deref())?;
    // Checked here because a second daemon would lose its own check in a process with no stderr.
    if ipc::daemon_running(&ipc::socket_name()) {
        bail!("already running");
    }
    let exe = std::env::current_exe().context("current exe")?;
    let daemon_exe = daemon::daemon_exe(&exe);
    if !daemon_exe.exists() {
        bail!(
            "{} not found — the daemon is installed beside this binary",
            daemon_exe.display()
        );
    }
    let mut cmd = std::process::Command::new(&daemon_exe);
    if let Some(c) = &config {
        cmd.arg("--config").arg(c);
    }
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // DETACHED_PROCESS: no inherited console; CREATE_NO_WINDOW: no new one.
        cmd.creation_flags(0x0000_0008 | 0x0800_0000);
    }
    let mut child = cmd
        .spawn()
        .with_context(|| format!("starting {}", daemon_exe.display()))?;
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        if ipc::daemon_running(&ipc::socket_name()) {
            break;
        }
        if child
            .try_wait()
            .context("waiting for the daemon")?
            .is_some()
        {
            bail!("daemon exited early — run `byovox run` to see why");
        }
        if std::time::Instant::now() >= deadline {
            bail!("daemon did not answer within 2s — run `byovox run` to see why");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    println!("byovox daemon started (pid {})", child.id());
    Ok(())
}

fn relay(req: ipc::Request) -> Result<()> {
    let reply = ipc::send(&ipc::socket_name(), req).context("is the daemon running?")?;
    if !reply.ok {
        bail!("{}", reply.error.unwrap_or_else(|| "refused".into()));
    }
    Ok(())
}

fn status() -> Result<()> {
    let r =
        ipc::send(&ipc::socket_name(), ipc::Request::Status).context("is the daemon running?")?;
    if !r.ok {
        bail!("{}", r.error.unwrap_or_else(|| "refused".into()));
    }
    println!("state: {}", r.state.unwrap_or_else(|| "?".into()));
    if let Some(enabled) = r.enabled {
        println!("enabled: {enabled}");
    }
    if let Some(e) = r.last_error {
        println!("last error: {e}");
    }
    Ok(())
}

fn last() -> Result<()> {
    let r = ipc::send(&ipc::socket_name(), ipc::Request::Last).context("is the daemon running?")?;
    match r.text {
        Some(t) => println!("{t}"),
        None => bail!("{}", r.error.unwrap_or_else(|| "nothing held".into())),
    }
    Ok(())
}

fn check_cmd(path: PathBuf, pause: bool) -> Result<()> {
    let cfg = config::load(&path)?;
    let ok = check::run(&cfg, &path);
    if pause {
        println!("\npress Enter to close");
        let _ = std::io::stdin().read_line(&mut String::new());
    }
    if !ok {
        std::process::exit(1);
    }
    Ok(())
}

/// 1 when the `check` it ends on failed. A failed autostart registration only prints the retry command.
fn setup_cmd(path: PathBuf, config: Option<PathBuf>) -> Result<()> {
    let passed = setup::run(&path)?;
    #[cfg(windows)]
    match platform::windows::autostart::enable(config.as_deref()) {
        Ok(()) => println!(
            "autostart enabled: byovox starts at login (clear Run on startup in the tray to stop it)"
        ),
        Err(e) => {
            eprintln!("byovox: autostart not enabled: {e:#} (retry: byovox autostart --enable)")
        }
    }
    #[cfg(not(windows))]
    let _ = config;
    if !passed {
        std::process::exit(1);
    }
    Ok(())
}

fn hotkey_cmd_dispatch(
    path: PathBuf,
    set: Option<String>,
    mode: Option<String>,
    cancel: Option<String>,
    list: bool,
    force: bool,
) -> Result<()> {
    if list {
        hotkey_cmd::list();
        return Ok(());
    }
    let change = hotkey_cmd::Change {
        key: set,
        mode,
        cancel,
    };
    if change.is_empty() {
        return hotkey_cmd::show(&path);
    }
    hotkey_cmd::set(&path, &change, force)
}

fn config_cmd(path: PathBuf, init: bool) -> Result<()> {
    if init {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut f) => f
                .write_all(config::EXAMPLE.as_bytes())
                .with_context(|| format!("writing {}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => bail!(
                "{} already exists — edit it, or delete it and re-run",
                path.display()
            ),
            Err(e) => return Err(e).with_context(|| format!("writing {}", path.display())),
        }
        println!("wrote {}", path.display());
        return Ok(());
    }
    let cfg = config::load(&path)?;
    println!(
        "# effective configuration ({})\n",
        if path.exists() {
            path.display().to_string()
        } else {
            "no file — all defaults".into()
        }
    );
    print!("{}", toml::to_string_pretty(&cfg)?);
    println!("\n# provenance");
    for (key, source) in config::provenance(&path)? {
        println!("{key:<28} {source}");
    }
    Ok(())
}

fn autostart(enable: bool, disable: bool, config: Option<PathBuf>) -> Result<()> {
    #[cfg(windows)]
    {
        if enable {
            platform::windows::autostart::enable(config.as_deref())?;
            match &config {
                Some(c) => println!("autostart enabled (HKCU Run key, --config {})", c.display()),
                None => println!("autostart enabled (HKCU Run key)"),
            }
        } else if disable {
            platform::windows::autostart::disable()?;
            println!("autostart disabled");
        } else {
            bail!("pass --enable or --disable");
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = (enable, disable, config);
        bail!("autostart is Windows-only for now")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    /// `conflicts_with` names a field at runtime; only `debug_assert` catches a rename.
    #[test]
    fn cli_definition_is_well_formed() {
        Cli::command().debug_assert();
    }

    #[test]
    fn no_subcommand_is_the_daemon_and_config_is_global() {
        let c = Cli::try_parse_from(["byovox"]).expect("a bare invocation starts the daemon");
        assert!(c.cmd.is_none());
        assert!(c.config.is_none());

        // `global = true` is what lets `--config` follow the subcommand.
        let c = Cli::try_parse_from(["byovox", "config", "--config", "x.toml"])
            .expect("--config after the subcommand");
        assert_eq!(c.config.as_deref(), Some(std::path::Path::new("x.toml")));
        assert!(matches!(c.cmd, Some(Cmd::Config { init: false })));

        let c = Cli::try_parse_from(["byovox", "--config", "x.toml", "status"])
            .expect("--config before the subcommand");
        assert_eq!(c.config.as_deref(), Some(std::path::Path::new("x.toml")));
        assert!(matches!(c.cmd, Some(Cmd::Status)));

        assert!(matches!(
            Cli::try_parse_from(["byovox", "check", "--pause"])
                .expect("check --pause")
                .cmd,
            Some(Cmd::Check { pause: true })
        ));
    }

    #[test]
    fn setup_takes_the_config_path_it_will_write() {
        let c = Cli::try_parse_from(["byovox", "setup", "--config", "x.toml"]).expect("setup");
        assert!(matches!(c.cmd, Some(Cmd::Setup)));
        assert_eq!(c.config.as_deref(), Some(std::path::Path::new("x.toml")));
        assert!(Cli::try_parse_from(["byovox", "setup", "--yes"]).is_err());
    }

    #[test]
    fn run_is_the_foreground_daemon() {
        let c = Cli::try_parse_from(["byovox", "run", "--config", "x.toml"]).expect("run");
        assert!(matches!(c.cmd, Some(Cmd::Run)));
        assert_eq!(c.config.as_deref(), Some(std::path::Path::new("x.toml")));
    }

    #[test]
    fn enable_and_disable_cannot_be_given_together() {
        assert!(Cli::try_parse_from(["byovox", "autostart", "--enable", "--disable"]).is_err());
        assert!(matches!(
            Cli::try_parse_from(["byovox", "autostart", "--enable"])
                .expect("--enable alone")
                .cmd,
            Some(Cmd::Autostart {
                enable: true,
                disable: false
            })
        ));
    }
}
