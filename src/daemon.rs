//! The daemon: everything that runs while the tray icon is up.

use std::path::{Path, PathBuf};
use std::sync::mpsc::channel;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::config::Config;
use crate::hotkey::HotkeyMode;
use crate::pipeline::{Pipeline, PipelineConfig};
use crate::{
    capture, capture_log, config, hotkey, ipc, lang, pipeline, platform, polish, server, stt, ui,
    update,
};

/// How long the process stays up after the UI loop ends, so the IPC connection thread can
/// write the `quit` reply its handler already returned.
const QUIT_GRACE: Duration = Duration::from_millis(200);

pub struct Options {
    pub config_path: Option<PathBuf>,
    pub log_to_stderr: bool,
}

pub fn daemon_exe(exe: &Path) -> PathBuf {
    exe.with_file_name(format!("byovox-daemon{}", std::env::consts::EXE_SUFFIX))
}

pub fn cli_exe(exe: &Path) -> PathBuf {
    exe.with_file_name(format!("byovox{}", std::env::consts::EXE_SUFFIX))
}

pub fn preflight(config_path: Option<&Path>) -> Result<Config> {
    let path = config_path.map_or_else(config::default_path, Path::to_path_buf);
    let cfg = config::load(&path)?;
    validate(&cfg)?;
    Ok(cfg)
}

fn validate(cfg: &Config) -> Result<()> {
    if std::env::var("RUST_LOG").is_err() {
        parse_level(&cfg.logging.level)?;
    }
    HotkeyMode::parse(&cfg.hotkey.mode).ok_or_else(|| {
        anyhow::anyhow!("hotkey.mode `{}`: expected hold | toggle", cfg.hotkey.mode)
    })?;
    lang::LanguagePolicy::from_config(&cfg.language, cfg.stt.by_language.keys())?;
    // A selector that names nothing is refused here, where it can be read: the microphone is not
    // touched until the first dictation. Enumerating names opens no stream.
    capture::validate_selector(&cfg.capture.device).map_err(anyhow::Error::msg)?;
    server::validate(&cfg.stt.server).map_err(anyhow::Error::msg)?;
    platform::validate(cfg)?;
    Ok(())
}

/// Owns the appender guard so fatals reach the log file (an autostarted daemon's stderr goes
/// nowhere); `process::exit` runs no destructors, so the guard is dropped by hand.
pub fn run(opts: Options) -> Result<()> {
    let path = opts.config_path.unwrap_or_else(config::default_path);
    let cfg = config::load(&path)?;
    let guard = init_logging(&cfg.logging.level, opts.log_to_stderr)?;
    // Installed only for the daemon, once logging is up. The default hook writes the payload to
    // stderr, which the transcript must never reach.
    std::panic::set_hook(Box::new(|info| {
        tracing::error!(at = panic_location(info.location()), "panicked");
    }));
    if let Err(e) = start(cfg, path) {
        tracing::error!(
            pid = std::process::id(),
            error = format!("{e:#}"),
            "byovox failed to start"
        );
        drop(guard);
        return Err(e);
    }
    tracing::info!("byovox stopped");
    std::thread::sleep(QUIT_GRACE);
    drop(guard);
    std::process::exit(0); // the hook thread blocks in GetMessageW; process exit ends it
}

fn lane_client(
    section: &config::SttConfig,
    lane: &config::SttLane,
) -> (stt::SttClient, Option<String>) {
    let c = section.lane_config(lane);
    let client = stt::SttClient::new(
        &c.base_url,
        &c.model,
        config::resolve_token(&c.api_key_env, &c.api_key_file),
        Duration::from_secs(c.timeout_s),
        Duration::from_secs(c.connect_timeout_s),
        c.no_speech_threshold > 0.0,
    );
    (client, Some(c.prompt).filter(|p| !p.is_empty()))
}

fn start(cfg: Config, path: PathBuf) -> Result<()> {
    tracing::info!(pid = std::process::id(), version = crate::VERSION, config = %path.display(), "byovox starting");

    // The list the spawning CLI already refused on its console, again with the log open: a
    // Run-key daemon has no console, so this is the only place its config errors are recorded.
    validate(&cfg)?;
    let mode = HotkeyMode::parse(&cfg.hotkey.mode).ok_or_else(|| {
        anyhow::anyhow!("hotkey.mode `{}`: expected hold | toggle", cfg.hotkey.mode)
    })?;
    let policy = lang::LanguagePolicy::from_config(&cfg.language, cfg.stt.by_language.keys())?;
    if ipc::daemon_running(&ipc::socket_name()) {
        bail!("already running");
    }
    let mut cleartext: Vec<String> = Vec::new();
    if config::is_cleartext_remote(&cfg.stt.base_url) {
        cleartext.push("stt.base_url".into());
    }
    for (code, lane) in &cfg.stt.by_language {
        if config::is_cleartext_remote(&lane.base_url) {
            cleartext.push(format!("stt.by_language.{code}.base_url"));
        }
    }
    if cfg.polish.enabled && config::is_cleartext_remote(&cfg.polish.base_url) {
        cleartext.push("polish.base_url".into());
    }
    if !cleartext.is_empty() {
        tracing::warn!(keys = %cleartext.join(", "), "{}", config::CLEARTEXT_WARNING);
    }
    server::start(&cfg.stt.server);

    let backends = platform::detect(&cfg)?;
    tracing::info!(hotkey = backends.names.hotkey, layout = backends.names.layout, rungs = ?backends.names.rungs, "backends");

    let stt_token = config::resolve_token(&cfg.stt.api_key_env, &cfg.stt.api_key_file);
    let stt_timeout = Duration::from_secs(cfg.stt.timeout_s);
    let stt_connect_timeout = Duration::from_secs(cfg.stt.connect_timeout_s);
    let stt_scored = cfg.stt.no_speech_threshold > 0.0;
    let mut stt = stt::Routed::new(Box::new(stt::SttClient::new(
        &cfg.stt.base_url,
        &cfg.stt.model,
        stt_token,
        stt_timeout,
        stt_connect_timeout,
        stt_scored,
    )))
    .detecting(stt_scored);
    for (code, lane) in &cfg.stt.by_language {
        let Some(lang) = lang::Lang::parse(code) else {
            continue;
        };
        let (client, prompt) = lane_client(&cfg.stt, lane);
        stt = stt.lane(lang, Box::new(client), prompt);
        // The key, never the URL: a `base_url` can carry `user:pass@`, and the log is kept a week.
        tracing::info!(lang = %lang, key = %format!("stt.by_language.{code}"), "stt lane");
    }
    let polisher: Option<Box<dyn polish::Polisher>> = if cfg.polish.enabled {
        let base = if cfg.polish.prompt_file.is_empty() {
            polish::built_in(cfg.polish.capitalize_first_word)
        } else {
            let file = config::expand_home(&cfg.polish.prompt_file);
            std::fs::read_to_string(&file)
                .with_context(|| format!("polish.prompt_file {}", file.display()))?
        };
        let prompt = polish::prompt_for(&base, &cfg.stt);
        let key = config::resolve_token(&cfg.polish.api_key_env, &cfg.polish.api_key_file);
        if key.is_none() && !cfg.polish.api_key_env.is_empty() {
            tracing::warn!(var = %cfg.polish.api_key_env, "polish token not found; polish requests will be unauthenticated");
        }
        Some(Box::new(polish::PolishClient::new(
            &cfg.polish.base_url,
            &cfg.polish.model,
            key,
            Duration::from_secs(cfg.polish.timeout_s),
            Duration::from_secs(cfg.polish.connect_timeout_s),
            prompt,
        )))
    } else {
        None
    };
    let recorder: Option<Box<dyn pipeline::Recorder>> = if cfg.capture_log.enabled {
        let dir = if cfg.capture_log.dir.is_empty() {
            config::data_dir().join("capture")
        } else {
            config::expand_home(&cfg.capture_log.dir)
        };
        Some(Box::new(capture_log::CaptureLog::new(
            dir,
            cfg.capture_log.keep_days,
        )?))
    } else {
        None
    };

    let (event_loop, proxy) = ui::build_event_loop()?;
    let mut pipe = Pipeline::new(
        PipelineConfig {
            initial_mode: mode,
            min_hold: Duration::from_millis(cfg.hotkey.min_hold_ms),
            polish_min_words: cfg.polish.min_words,
            prompt: Some(cfg.stt.prompt.clone()).filter(|p| !p.is_empty()),
            trailing_space: cfg.inject.trailing_space,
            strip_terminal_period: cfg.inject.strip_terminal_period,
            restore_first_i: polish::restores_first_i(
                cfg.polish.capitalize_first_word,
                &cfg.polish.prompt_file,
            ),
            max_chars: cfg.inject.max_chars,
            polish_model: cfg.polish.model.clone(),
            no_speech_threshold: cfg.stt.no_speech_threshold as f32,
            no_speech_warn: cfg.stt.no_speech_warn as f32,
        },
        backends.capture,
        backends.layout,
        policy,
        Box::new(stt),
        polisher,
        backends.rungs,
        backends.target,
        Box::new(ui::ProxyIndicator(proxy.clone())),
        recorder,
    );
    let shared = pipe.shared();

    let (tx, rx) = channel::<hotkey::HotkeyEvent>();
    let backend_tx = tx.clone();
    let ui_tx = tx.clone();
    let ipc_shared = shared.clone();
    let ipc_proxy = proxy.clone();
    // Bound before the hook goes in: the loser of the single-instance race finds out here, before
    // it installs a keyboard hook or builds a pipeline.
    ipc::serve(&ipc::socket_name(), move |req| {
        use ipc::{Reply, Request};
        match req {
            Request::Toggle => {
                let (forward, reply) = toggle_decision(shared_of(&ipc_shared).enabled);
                if forward {
                    let _ = tx.send(hotkey::HotkeyEvent::Toggle);
                }
                reply
            }
            Request::Quit => {
                let _ = ipc_proxy.send_event(ui::UserEvent::Quit);
                Reply {
                    ok: true,
                    ..Default::default()
                }
            }
            Request::Status => status_reply(&shared_of(&ipc_shared)),
            Request::Last => match shared_of(&ipc_shared).last_transcript.clone() {
                Some(t) => Reply {
                    ok: true,
                    text: Some(t),
                    ..Default::default()
                },
                None => Reply {
                    ok: false,
                    error: Some("nothing held yet".into()),
                    ..Default::default()
                },
            },
        }
    })?;

    std::thread::Builder::new()
        .name("byovox-hotkey".into())
        .spawn(move || backends.hotkey.run(backend_tx))?;
    let pipeline_proxy = proxy.clone();
    std::thread::Builder::new()
        .name("byovox-pipeline".into())
        .spawn(move || {
            let run = std::panic::AssertUnwindSafe(|| pipeline::pump(&mut pipe, &rx));
            if std::panic::catch_unwind(run).is_err() {
                tracing::error!("pipeline thread panicked; stopping byovox");
            }
            // Reached only if the pipeline died or every sender dropped; a tray icon still saying
            // idle is worse than none, so leave through the ordinary Quit path, which flushes the log.
            let _ = pipeline_proxy.send_event(ui::UserEvent::Quit);
        })?;
    let update_proxy = proxy.clone();
    update::spawn(
        cfg.update.check,
        update::LATEST_URL,
        update::stamp_path(),
        move |release| {
            let _ = update_proxy.send_event(ui::UserEvent::UpdateAvailable(release));
        },
    );

    ui::run(
        event_loop,
        ui::UiOptions {
            pill: cfg.indicator.pill,
            cue: cfg.indicator.cue,
            cue_volume: cfg.indicator.cue_volume as f32,
            version: env!("CARGO_PKG_VERSION"),
        },
        shared,
        ui_tx,
        path,
        log_dir(),
    )
}

/// Whether to forward a `toggle` into the pipeline, and the reply to send back. A disabled
/// pipeline drops the event silently, so an unconditional `ok` would report a dictation that
/// never starts.
fn toggle_decision(enabled: bool) -> (bool, ipc::Reply) {
    if enabled {
        (
            true,
            ipc::Reply {
                ok: true,
                ..Default::default()
            },
        )
    } else {
        (
            false,
            ipc::Reply {
                ok: false,
                error: Some("dictation is disabled".into()),
                ..Default::default()
            },
        )
    }
}

fn shared_of(
    lock: &std::sync::Mutex<pipeline::Shared>,
) -> std::sync::MutexGuard<'_, pipeline::Shared> {
    lock.lock().unwrap_or_else(|p| p.into_inner())
}

fn status_reply(s: &pipeline::Shared) -> ipc::Reply {
    ipc::Reply {
        ok: true,
        state: Some(s.state.into()),
        enabled: Some(s.enabled),
        last_error: s.last_error.clone(),
        ..Default::default()
    }
}

fn log_dir() -> PathBuf {
    directories::ProjectDirs::from("", "", "byovox")
        .map(|d| d.data_local_dir().join("logs"))
        .unwrap_or_else(|| PathBuf::from("logs"))
}

/// A directive list into a filter, loudly: `EnvFilter::new` drops what it cannot parse and returns
/// a filter with no directives, so a typo would silently log nothing.
fn parse_directives(source: &str, value: &str) -> Result<tracing_subscriber::EnvFilter> {
    tracing_subscriber::EnvFilter::try_new(value).with_context(|| format!("{source} `{value}`"))
}

fn parse_level(value: &str) -> Result<tracing_subscriber::EnvFilter> {
    const SOURCE: &str = "logging.level";
    let value = value.trim();
    if value.is_empty() {
        bail!("{SOURCE} is empty; expected a level like `info` or a directive list");
    }
    if !value.contains(['=', ','])
        && value
            .parse::<tracing_subscriber::filter::LevelFilter>()
            .is_err()
    {
        bail!("{SOURCE} `{value}`: expected trace | debug | info | warn | error | off");
    }
    parse_directives(SOURCE, value)
}

/// The file and line a panic came from, never its payload — a payload can carry transcript
/// text (any `expect` on a string built from one), and the default hook prints it to stderr.
fn panic_location(loc: Option<&std::panic::Location<'_>>) -> String {
    loc.map_or_else(|| "unknown location".to_string(), |l| l.to_string())
}

fn init_logging(
    level: &str,
    to_stderr: bool,
) -> Result<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt};
    let dir = log_dir();
    std::fs::create_dir_all(&dir)?;
    let file = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("byovox")
        .filename_suffix("log")
        .max_log_files(7)
        .build(&dir)?;
    let (writer, guard) = tracing_appender::non_blocking(file);
    // `RUST_LOG` is read directly: `try_from_default_env`'s one `Err` cannot tell unset from
    // invalid, and an invalid value must fail, not fall through to the config level.
    let filter = match std::env::var("RUST_LOG") {
        Ok(v) => parse_directives("RUST_LOG", &v)?,
        Err(_) => parse_level(level)?,
    };
    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_writer(writer).with_ansi(false))
        .with(to_stderr.then(|| fmt::layer().with_writer(std::io::stderr)))
        .init();
    Ok(guard)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stt::Transcriber;
    use crate::testutil::{MockServer, auth_header};

    #[test]
    fn a_lane_client_sends_the_token_its_lane_asks_for() {
        let dir = std::env::temp_dir().join(format!("byovox-daemon-{}-lane", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let keys = dir.join("keys.env");
        std::fs::write(
            &keys,
            "BYOVOX_DAEMON_TEST_STT=stt-tok\nBYOVOX_DAEMON_TEST_HE_FILE=he-file-tok\n",
        )
        .unwrap();
        let keys = keys.to_str().unwrap();
        // SAFETY: no other test touches this variable, and the environment API is synchronised on Windows.
        unsafe { std::env::set_var("BYOVOX_DAEMON_TEST_HE_ENV", "he-env-tok") };
        let section = config::SttConfig {
            api_key_env: "BYOVOX_DAEMON_TEST_STT".into(),
            api_key_file: keys.into(),
            ..Default::default()
        };
        for (lane, sent) in [
            (config::SttLane::default(), None),
            (
                config::SttLane {
                    inherit_token: true,
                    ..Default::default()
                },
                Some("Bearer stt-tok"),
            ),
            (
                config::SttLane {
                    api_key_env: "BYOVOX_DAEMON_TEST_HE_ENV".into(),
                    ..Default::default()
                },
                Some("Bearer he-env-tok"),
            ),
            (
                config::SttLane {
                    api_key_env: "BYOVOX_DAEMON_TEST_HE_FILE".into(),
                    api_key_file: keys.into(),
                    ..Default::default()
                },
                Some("Bearer he-file-tok"),
            ),
        ] {
            let srv = MockServer::start(200, r#"{"text":"x"}"#);
            let lane = config::SttLane {
                base_url: srv.url().into(),
                ..lane
            };
            let (client, _) = lane_client(&section, &lane);
            let he = lang::SttLanguage::Explicit(lang::Lang::parse("he").unwrap());
            client.transcribe(b"RIFF", &he, None).unwrap();
            let req = srv.requests().remove(0);
            assert_eq!(auth_header(&req).as_deref(), sent, "{lane:?}");
        }
        unsafe { std::env::remove_var("BYOVOX_DAEMON_TEST_HE_ENV") };
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn each_binary_resolves_the_other_beside_itself() {
        let suffix = std::env::consts::EXE_SUFFIX;
        let dir = Path::new("install").join("bin");
        let cli = dir.join(format!("byovox{suffix}"));
        let daemon = dir.join(format!("byovox-daemon{suffix}"));

        assert_eq!(daemon_exe(&cli), daemon);
        assert_eq!(cli_exe(&daemon), cli);
        assert_eq!(cli_exe(&cli), cli);
        assert_eq!(daemon_exe(&daemon), daemon);
    }

    #[test]
    fn toggle_is_refused_while_dictation_is_disabled() {
        let (forward, reply) = toggle_decision(true);
        assert!(forward);
        assert!(reply.ok);
        assert!(reply.error.is_none());

        let (forward, reply) = toggle_decision(false);
        assert!(!forward, "a disabled pipeline must not be sent the event");
        assert!(
            !reply.ok,
            "a dictation that never starts must not report ok"
        );
        assert_eq!(reply.error.as_deref(), Some("dictation is disabled"));
    }

    #[test]
    fn status_reports_enabled_so_idle_and_deaf_are_distinguishable() {
        let mut shared = pipeline::Shared::default();
        let r = status_reply(&shared);
        assert!(r.ok);
        assert_eq!(r.state.as_deref(), Some("idle"));
        assert_eq!(r.enabled, Some(true));
        assert!(r.last_error.is_none());

        shared.enabled = false;
        shared.last_error = Some("stt".into());
        let r = status_reply(&shared);
        assert_eq!(r.state.as_deref(), Some("idle"), "still idle");
        assert_eq!(r.enabled, Some(false), "but deaf, and it has to say so");
        assert_eq!(r.last_error.as_deref(), Some("stt"));
    }

    #[test]
    fn validate_refuses_a_relative_or_missing_server_command() {
        let with = |command: &str| {
            let mut cfg = Config::default();
            cfg.stt.server.command = command.into();
            cfg
        };
        assert!(validate(&with("")).is_ok(), "empty starts nothing");
        let missing = std::env::temp_dir().join("byovox-no-such-server.exe");
        for bad in ["whisper-server.exe", missing.to_str().unwrap()] {
            let e = validate(&with(bad)).expect_err(bad).to_string();
            assert!(e.contains("stt.server.command"), "{bad}: {e}");
        }
    }

    #[test]
    fn a_bad_level_is_rejected_rather_than_silently_disabling_logging() {
        assert!(parse_level("info").is_ok());
        assert!(parse_level("INFO").is_ok());
        assert!(parse_level("byovox=debug,warn").is_ok());

        let err = parse_level("").expect_err("an empty level must be rejected");
        assert!(err.to_string().contains("empty"), "{err}");

        let err = parse_level("inf").expect_err("a typo must be rejected");
        assert!(err.to_string().contains("expected trace"), "{err}");
        assert!(err.to_string().contains("logging.level"), "{err}");
    }

    #[test]
    fn rust_log_keeps_the_whole_directive_grammar() {
        for value in ["byovox", "[my_span]", "", "byovox=debug,warn", "off"] {
            assert!(
                parse_directives("RUST_LOG", value).is_ok(),
                "RUST_LOG={value:?} must still work"
            );
        }
        let err = parse_directives("RUST_LOG", "warn=chatty").expect_err("not a directive list");
        assert!(err.to_string().contains("RUST_LOG"), "{err}");
    }

    #[test]
    #[track_caller]
    fn a_panic_is_logged_by_location_only() {
        let here = panic_location(Some(std::panic::Location::caller()));
        assert!(here.contains("daemon.rs"), "{here}");
        assert!(here.contains(':'), "file:line, {here}");
        assert_eq!(panic_location(None), "unknown location");
    }
}
