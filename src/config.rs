//! Configuration: one TOML file, every key defaulted, unknown keys rejected.
//! `docs/config.example.toml` documents every key; a test pins it equal to `Config::default()`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

pub const EXAMPLE: &str = include_str!("../docs/config.example.toml");

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub stt: SttConfig,
    pub language: LanguageConfig,
    pub polish: PolishConfig,
    pub hotkey: HotkeyConfig,
    pub inject: InjectConfig,
    pub indicator: IndicatorConfig,
    pub capture: CaptureConfig,
    pub capture_log: CaptureLogConfig,
    pub logging: LoggingConfig,
    pub update: UpdateConfig,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SttConfig {
    pub base_url: String,
    pub model: String,
    pub api_key_env: String,
    pub api_key_file: String,
    pub prompt: String,
    pub timeout_s: u64,
    pub connect_timeout_s: u64,
    /// Discard a transcript whose duration-weighted mean `no_speech_prob` exceeds this; `0.0` keeps every one.
    pub no_speech_threshold: f64,
    /// Play the warning cue when any segment of a kept transcript exceeds this; `0.0` never warns.
    pub no_speech_warn: f64,
    /// Marks `base_url` as somebody else's service; changes no request, `byovox check` reads it for its `note hosted` row.
    pub hosted: bool,
    pub server: SttServer,
    /// Per-language endpoints keyed by ISO 639-1 code (`stt::Routed`); the token does not inherit (`SttConfig::lane_config`).
    /// Last because TOML writes scalars before tables.
    pub by_language: std::collections::BTreeMap<String, SttLane>,
}
impl Default for SttConfig {
    fn default() -> Self {
        Self {
            // Empty on purpose: a placeholder host would fail as a DNS error instead of naming the key.
            base_url: String::new(),
            model: "whisper-1".into(),
            api_key_env: String::new(),
            api_key_file: String::new(),
            prompt: String::new(),
            timeout_s: 30,
            connect_timeout_s: 5,
            no_speech_threshold: 0.3,
            no_speech_warn: 0.08,
            hosted: false,
            server: Default::default(),
            by_language: Default::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct SttServer {
    pub command: String,
    /// Each item is one argument as written: no shell, no splitting on spaces.
    pub args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct SttLane {
    pub base_url: String,
    pub model: String,
    pub prompt: String,
    pub api_key_env: String,
    pub api_key_file: String,
    /// Send `[stt]`'s token to this lane. `validate` refuses it beside a lane `api_key_env`.
    pub inherit_token: bool,
}

impl SttConfig {
    /// The effective `[stt]` for one lane; `model` and `prompt` fall back to this section's when empty, the token only per `inherit_token`.
    pub fn lane_config(&self, lane: &SttLane) -> SttConfig {
        let (api_key_env, api_key_file) = if !lane.api_key_env.is_empty() {
            (lane.api_key_env.clone(), lane.api_key_file.clone())
        } else if lane.inherit_token {
            (self.api_key_env.clone(), self.api_key_file.clone())
        } else {
            (String::new(), String::new())
        };
        SttConfig {
            api_key_env,
            api_key_file,
            base_url: lane.base_url.clone(),
            model: if lane.model.is_empty() {
                self.model.clone()
            } else {
                lane.model.clone()
            },
            prompt: if lane.prompt.is_empty() {
                self.prompt.clone()
            } else {
                lane.prompt.clone()
            },
            by_language: Default::default(),
            // `hosted` is copied but never read off a lane config; `check` reads `cfg.stt.hosted`.
            ..self.clone()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LanguageConfig {
    /// "auto" or an ISO 639-1 code used when the layout is unmapped or unreadable.
    pub default: String,
    pub candidates: Vec<String>,
    /// Layout language (ISO 639-1) -> STT language: forced under an explicit `default`, the prior under `auto`.
    pub by_layout: std::collections::BTreeMap<String, String>,
}
impl Default for LanguageConfig {
    fn default() -> Self {
        Self {
            default: "auto".into(),
            candidates: Vec::new(),
            by_layout: Default::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PolishConfig {
    pub enabled: bool,
    pub base_url: String,
    pub model: String,
    pub api_key_env: String,
    pub api_key_file: String,
    pub min_words: usize,
    pub prompt_file: String,
    /// `false` swaps rule 1 of the built-in prompt for one that lower-cases; a `prompt_file` is never rewritten.
    pub capitalize_first_word: bool,
    pub timeout_s: u64,
    pub connect_timeout_s: u64,
    pub hosted: bool,
}
impl Default for PolishConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            // Empty like `stt.base_url`; `validate` names the missing key while polish is enabled.
            base_url: String::new(),
            model: String::new(),
            api_key_env: String::new(),
            api_key_file: String::new(),
            min_words: 0,
            prompt_file: String::new(),
            capitalize_first_word: true,
            timeout_s: 20,
            connect_timeout_s: 5,
            hosted: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HotkeyConfig {
    /// W3C UI Events `code` name (F13) or a chord (ControlLeft+ShiftLeft+Z).
    pub key: String,
    pub mode: String,
    pub min_hold_ms: u64,
    pub cancel_key: String,
}
impl Default for HotkeyConfig {
    fn default() -> Self {
        Self {
            key: "ControlRight".into(),
            mode: "hold".into(),
            min_hold_ms: 250,
            cancel_key: "Escape".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct InjectConfig {
    /// auto | type | paste | clipboard-only
    pub mode: String,
    pub trailing_space: bool,
    /// Pop exactly one terminal `.` before typing; `false` types the endpoint's period as it came.
    pub strip_terminal_period: bool,
    /// Longest transcript, in characters, that may be typed; `0` lifts the limit. Over it is held for `byovox last`.
    pub max_chars: usize,
}
impl Default for InjectConfig {
    fn default() -> Self {
        Self {
            mode: "auto".into(),
            trailing_space: false,
            strip_terminal_period: true,
            max_chars: 20_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct IndicatorConfig {
    pub pill: bool,
    pub cue: bool,
    pub cue_volume: f64,
}
impl Default for IndicatorConfig {
    fn default() -> Self {
        Self {
            pill: true,
            cue: true,
            cue_volume: 0.05,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct CaptureConfig {
    /// Case-insensitive substring of an input device name; empty for the system default (see `capture::select`).
    pub device: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CaptureLogConfig {
    pub enabled: bool,
    /// Empty = `data_dir()` + `capture`.
    pub dir: String,
    pub keep_days: u32,
}
impl Default for CaptureLogConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            dir: String::new(),
            keep_days: 30,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LoggingConfig {
    pub level: String,
}
impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: "info".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UpdateConfig {
    /// One anonymous request a day for the latest release; `false` makes none (see `update`).
    pub check: bool,
}
impl Default for UpdateConfig {
    fn default() -> Self {
        Self { check: true }
    }
}

pub fn default_path() -> PathBuf {
    directories::ProjectDirs::from("", "", "byovox")
        .map(|d| d.config_dir().join("config.toml"))
        .unwrap_or_else(|| PathBuf::from("config.toml"))
}

pub fn data_dir() -> PathBuf {
    directories::ProjectDirs::from("", "", "byovox")
        .map(|d| d.data_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Bearer token: the named env var, else `NAME=VALUE` in `file`. Empty `env_name` or a blank value is `None`; never logged.
pub fn resolve_token(env_name: &str, file: &str) -> Option<String> {
    if env_name.is_empty() {
        return None;
    }
    if let Ok(v) = std::env::var(env_name)
        && !v.trim().is_empty()
    {
        return Some(v.trim().to_string());
    }
    if file.is_empty() {
        return None;
    }
    let path = expand_home(file);
    let text = std::fs::read_to_string(&path).ok()?;
    let raw = text.lines().find_map(|line| {
        let (k, v) = line.split_once('=')?;
        (k.trim() == env_name).then_some(v)
    })?;
    let value = raw.trim().trim_matches('"').trim_matches('\'').trim();
    (!value.is_empty()).then(|| value.to_string())
}

/// One wording for the plain-HTTP warning, shared by `byovox check` and the daemon's startup log.
pub const CLEARTEXT_WARNING: &str = "plain HTTP: voice, transcript and the bearer token cross the network in clear — \
     use https or a private network";

/// True only for `http://` to a non-loopback host; a hostname other than `localhost` is assumed remote, as this runs before DNS.
pub fn is_cleartext_remote(base_url: &str) -> bool {
    let url = base_url.trim();
    let Some(rest) = url
        .get(..7)
        .filter(|p| p.eq_ignore_ascii_case("http://"))
        .map(|_| &url[7..])
    else {
        return false;
    };
    let authority = rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default()
        // Credentials in a URL must not be mistaken for the host.
        .rsplit('@')
        .next()
        .unwrap_or_default();
    let host = match authority.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or_default(),
        None => authority.split(':').next().unwrap_or_default(),
    };
    if host.eq_ignore_ascii_case("localhost") {
        return false;
    }
    match host.parse::<std::net::IpAddr>() {
        Ok(ip) => !ip.is_loopback(),
        Err(_) => true,
    }
}

/// `user:pass@` replaced by `***@`, because `check`'s rows get pasted into bug reports.
pub fn redact_userinfo(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_string();
    };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(end);
    match authority.rsplit_once('@') {
        Some((_, host)) => format!("{scheme}://***@{host}{tail}"),
        None => url.to_string(),
    }
}

/// `https://user:pw@api.example.com:8443/v1` becomes `api.example.com:8443`; a non-URL yields the empty string.
pub fn host_of(base_url: &str) -> &str {
    let url = base_url.trim();
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    authority.rsplit('@').next().unwrap_or_default()
}

/// `~/x` → home-relative; anything else unchanged.
pub fn expand_home(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/").or_else(|| p.strip_prefix("~\\"))
        && let Some(home) = directories::BaseDirs::new().map(|b| b.home_dir().to_path_buf())
    {
        return home.join(rest);
    }
    PathBuf::from(p)
}

/// Missing file = all defaults, which fail `validate` pointing at `byovox setup`; any other read failure is reported.
pub fn load(path: &Path) -> Result<Config> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let cfg = Config::default();
            validate(&cfg).with_context(|| {
                format!(
                    "no config at {} — run `byovox setup`, or `byovox config --init` and set \
                     the endpoints",
                    path.display()
                )
            })?;
            return Ok(cfg);
        }
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let cfg: Config =
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    validate(&cfg).with_context(|| format!("in {}", path.display()))?;
    Ok(cfg)
}

/// Runs on the one path every command loads through, so a bad value fails the daemon and `byovox check` up front.
fn validate(cfg: &Config) -> Result<()> {
    if cfg.stt.base_url.trim().is_empty() {
        bail!(
            "stt.base_url is empty: set it to your speech-to-text server, e.g. http://127.0.0.1:8770/v1"
        );
    }
    for (code, lane) in &cfg.stt.by_language {
        if crate::lang::Lang::parse(code).is_none() {
            bail!("stt.by_language.{code}: expected an ISO 639-1 code such as `he`");
        }
        if lane.base_url.trim().is_empty() {
            bail!(
                "stt.by_language.{code}.base_url is empty: set it to that language's speech-to-text server"
            );
        }
        if !lane.api_key_env.is_empty() && lane.inherit_token {
            bail!("stt.by_language.{code}: set api_key_env or inherit_token = true, not both");
        }
    }
    if cfg.polish.enabled {
        if cfg.polish.base_url.trim().is_empty() {
            bail!(
                "polish.base_url is empty: set it to your chat-completions gateway, or set polish.enabled = false"
            );
        }
        if cfg.polish.model.trim().is_empty() {
            bail!(
                "polish.model is empty: name the model your gateway serves, or set polish.enabled = false"
            );
        }
    }
    // Padding only: `capture.device` is trimmed before matching, so it would silently record from the default.
    if !cfg.capture.device.is_empty() && cfg.capture.device.trim().is_empty() {
        bail!(
            "capture.device is only whitespace: name a microphone, e.g. \"Microphone Array\", or leave it empty for the system default"
        );
    }
    let t = cfg.stt.no_speech_threshold;
    // `contains` is false for NaN too, right for a threshold nothing could compare against.
    if !(0.0..=1.0).contains(&t) {
        bail!("stt.no_speech_threshold is {t}: expected 0.0 to 1.0 (0.0 disables the gate)");
    }
    let w = cfg.stt.no_speech_warn;
    if !(0.0..=1.0).contains(&w) {
        bail!("stt.no_speech_warn is {w}: expected 0.0 to 1.0 (0.0 disables the warning cue)");
    }
    let v = cfg.indicator.cue_volume;
    if !(0.0..=1.0).contains(&v) {
        bail!(
            "indicator.cue_volume is {v}: expected 0.0 to 1.0 (a fraction of full scale; indicator.cue = false turns the cues off)"
        );
    }
    // Threshold 0.0 turns scoring off, so a non-zero warn beside it would be silently inert.
    if w > 0.0 && t == 0.0 {
        bail!(
            "stt.no_speech_warn is {w} but no_speech_threshold is 0.0, which turns whisper scoring off: set no_speech_warn = 0.0 with it, or raise no_speech_threshold"
        );
    }
    Ok(())
}

pub fn provenance(path: &Path) -> Result<Vec<(String, &'static str)>> {
    let file_table: toml::Table = match std::fs::read_to_string(path) {
        Ok(text) => toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => toml::Table::new(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let defaults: toml::Table = toml::Table::try_from(Config::default())?;
    let mut out = Vec::new();
    walk(&defaults, &file_table, "", &mut out);
    Ok(out)
}

fn walk(
    defaults: &toml::Table,
    file: &toml::Table,
    prefix: &str,
    out: &mut Vec<(String, &'static str)>,
) {
    for (k, v) in defaults {
        let dotted = if prefix.is_empty() {
            k.clone()
        } else {
            format!("{prefix}.{k}")
        };
        // An open-ended map has no default leaves; with none from the file it gets one row for itself.
        if matches!(v, toml::Value::Table(t) if t.is_empty()) {
            match file.get(k) {
                Some(toml::Value::Table(ft)) if !ft.is_empty() => {
                    out.extend(ft.keys().map(|fk| (format!("{dotted}.{fk}"), "file")));
                }
                _ => out.push((dotted, "default")),
            }
            continue;
        }
        match (v, file.get(k)) {
            (toml::Value::Table(dt), Some(toml::Value::Table(ft))) => walk(dt, ft, &dotted, out),
            (toml::Value::Table(dt), _) => walk(dt, &toml::Table::new(), &dotted, out),
            (_, Some(_)) => out.push((dotted, "file")),
            (_, None) => out.push((dotted, "default")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_file_keeps_defaults_for_everything_else() {
        let cfg: Config = toml::from_str("[stt]\nbase_url = \"http://x:1/v1\"\n").unwrap();
        assert_eq!(cfg.stt.base_url, "http://x:1/v1");
        assert_eq!(cfg.stt.timeout_s, 30);
        assert_eq!(cfg.stt.connect_timeout_s, 5);
        assert_eq!(cfg.polish.connect_timeout_s, 5);
        assert_eq!(cfg.hotkey.key, "ControlRight");
        assert!(cfg.polish.enabled);
    }

    #[test]
    fn no_server_table_names_no_server() {
        let cfg: Config = toml::from_str("[stt]\nbase_url = \"http://x:1/v1\"\n").unwrap();
        assert_eq!(cfg.stt.server.command, "", "command");
        assert!(cfg.stt.server.args.is_empty(), "args");
    }

    #[test]
    fn unknown_key_is_an_error() {
        let err = toml::from_str::<Config>("[stt]\nbase_ur = \"typo\"\n").unwrap_err();
        assert!(err.to_string().contains("base_ur"), "{err}");
    }

    #[test]
    fn a_misspelled_hosted_key_is_refused_in_both_sections() {
        for (text, typo) in [
            ("[stt]\nhoste = true\n", "hoste"),
            ("[stt]\nHosted = true\n", "Hosted"),
            ("[polish]\nhosted_ = true\n", "hosted_"),
        ] {
            let err = toml::from_str::<Config>(text).unwrap_err().to_string();
            assert!(err.contains(&format!("unknown field `{typo}`")), "{err}");
        }
        let cfg: Config =
            toml::from_str("[stt]\nhosted = true\n[polish]\nhosted = true\n").unwrap();
        assert!(cfg.stt.hosted && cfg.polish.hosted);
        assert!(!Config::default().stt.hosted && !Config::default().polish.hosted);
    }

    #[test]
    fn a_lane_config_inherits_what_it_leaves_empty() {
        let stt = SttConfig {
            base_url: "http://x/v1".into(),
            model: "whisper-1".into(),
            api_key_env: "STT_TOKEN".into(),
            api_key_file: "~/.config/byovox/keys.env".into(),
            prompt: "Glossary: Acme".into(),
            timeout_s: 45,
            connect_timeout_s: 12,
            no_speech_threshold: 0.2,
            no_speech_warn: 0.05,
            by_language: [(
                "he".to_string(),
                SttLane {
                    base_url: "http://x/he/v1".into(),
                    ..Default::default()
                },
            )]
            .into_iter()
            .collect(),
            ..Default::default()
        };
        let bare = stt.lane_config(&stt.by_language["he"]);
        assert_eq!(bare.base_url, "http://x/he/v1");
        assert_eq!(bare.model, "whisper-1");
        assert_eq!(bare.prompt, "Glossary: Acme");
        assert_eq!(
            (bare.api_key_env.as_str(), bare.api_key_file.as_str()),
            ("", ""),
            "a bare lane is sent no token"
        );
        assert_eq!(bare.timeout_s, 45);
        assert_eq!(bare.connect_timeout_s, 12);
        assert_eq!(bare.no_speech_threshold, 0.2);
        assert_eq!(bare.no_speech_warn, 0.05);
        assert!(bare.by_language.is_empty());

        let full = stt.lane_config(&SttLane {
            base_url: "http://y/he/v1".into(),
            model: "ivrit-large-v3".into(),
            prompt: "Glossary: he".into(),
            ..Default::default()
        });
        assert_eq!(
            (full.model.as_str(), full.prompt.as_str()),
            ("ivrit-large-v3", "Glossary: he")
        );
        assert_eq!(full.timeout_s, 45);
    }

    #[test]
    fn a_lane_is_sent_the_token_it_asks_for() {
        let stt = SttConfig {
            api_key_env: "STT_TOKEN".into(),
            api_key_file: "~/stt.env".into(),
            ..Default::default()
        };
        let pair = |lane: SttLane| {
            let c = stt.lane_config(&lane);
            (c.api_key_env, c.api_key_file)
        };
        assert_eq!(
            pair(SttLane {
                inherit_token: true,
                ..Default::default()
            }),
            ("STT_TOKEN".into(), "~/stt.env".into()),
            "inherit_token"
        );
        assert_eq!(
            pair(SttLane {
                api_key_env: "HE_TOKEN".into(),
                api_key_file: "~/he.env".into(),
                ..Default::default()
            }),
            ("HE_TOKEN".into(), "~/he.env".into()),
            "the lane's own"
        );
    }

    #[test]
    fn a_lane_with_every_token_key_parses() {
        let cfg: Config = toml::from_str(
            "[stt.by_language.he]\nbase_url = \"http://y/v1\"\napi_key_env = \"HE_TOKEN\"\n\
             api_key_file = \"~/he.env\"\ninherit_token = true\n",
        )
        .unwrap();
        let he = &cfg.stt.by_language["he"];
        assert_eq!(
            (
                he.api_key_env.as_str(),
                he.api_key_file.as_str(),
                he.inherit_token
            ),
            ("HE_TOKEN", "~/he.env", true)
        );
    }

    #[test]
    fn an_explicit_connect_timeout_round_trips() {
        let cfg: Config =
            toml::from_str("[stt]\nconnect_timeout_s = 15\n[polish]\nconnect_timeout_s = 9\n")
                .unwrap();
        assert_eq!(cfg.stt.connect_timeout_s, 15);
        assert_eq!(cfg.polish.connect_timeout_s, 9);
        assert_eq!((cfg.stt.timeout_s, cfg.polish.timeout_s), (30, 20));
        let back: Config = toml::from_str(&toml::to_string(&cfg).unwrap()).unwrap();
        assert_eq!(back, cfg);
    }

    #[test]
    fn example_file_is_exactly_the_defaults() {
        let from_example: Config = toml::from_str(EXAMPLE).expect("example parses");
        assert_eq!(from_example, Config::default());
        // Table-level too: a key left out of the example deserialises to its default and slips past the struct comparison.
        assert_eq!(
            toml::from_str::<toml::Table>(EXAMPLE).unwrap(),
            toml::Table::try_from(Config::default()).unwrap()
        );
    }

    #[test]
    fn provenance_marks_file_keys() {
        let dir = tempfile_dir("marks_file_keys");
        let path = dir.join("config.toml");
        std::fs::write(&path, "[polish]\nmodel = \"x\"\n").unwrap();
        let prov = provenance(&path).unwrap();
        assert!(prov.contains(&("polish.model".to_string(), "file")));
        assert!(prov.contains(&("polish.enabled".to_string(), "default")));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn provenance_lists_file_supplied_map_entries() {
        let dir = tempfile_dir("map_entries");

        let with = dir.join("with.toml");
        std::fs::write(&with, "[language.by_layout]\nhe = \"he\"\n").unwrap();
        let prov = provenance(&with).unwrap();
        assert!(
            prov.contains(&("language.by_layout.he".to_string(), "file")),
            "{prov:?}"
        );

        let without = dir.join("without.toml");
        std::fs::write(&without, "[polish]\nmodel = \"x\"\n").unwrap();
        let prov = provenance(&without).unwrap();
        assert!(
            prov.contains(&("language.by_layout".to_string(), "default")),
            "{prov:?}"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn missing_file_names_the_empty_endpoint_and_both_commands_that_write_one() {
        let dir = tempfile_dir("missing_file");
        let path = dir.join("config.toml");
        let msg = format!("{:#}", load(&path).unwrap_err());
        assert!(msg.contains("stt.base_url is empty"), "{msg}");
        assert!(msg.contains("byovox setup"), "{msg}");
        assert!(msg.contains("byovox config --init"), "{msg}");
        assert!(
            msg.find("byovox setup") < msg.find("byovox config --init"),
            "the wizard is offered first: {msg}"
        );
        assert!(msg.contains(&path.display().to_string()), "{msg}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_empty_endpoint_is_refused_by_name() {
        let dir = tempfile_dir("empty_endpoint");
        let path = dir.join("config.toml");
        let cases = [
            (
                "[polish]\nbase_url = \"http://x/v1\"\nmodel = \"m\"\n",
                "stt.base_url is empty",
            ),
            (
                "[stt]\nbase_url = \"http://x/v1\"\n[polish]\nmodel = \"m\"\n",
                "polish.base_url is empty",
            ),
            (
                "[stt]\nbase_url = \"http://x/v1\"\n[polish]\nbase_url = \"http://x/v1\"\n",
                "polish.model is empty",
            ),
            (
                "[stt]\nbase_url = \"http://x/v1\"\n[stt.by_language.he]\n[polish]\nenabled = false\n",
                "stt.by_language.he.base_url is empty",
            ),
            (
                "[stt]\nbase_url = \"http://x/v1\"\n[stt.by_language.hebrew]\nbase_url = \"http://x/he/v1\"\n[polish]\nenabled = false\n",
                "stt.by_language.hebrew: expected an ISO 639-1 code",
            ),
            (
                "[stt]\nbase_url = \"http://x/v1\"\n[stt.by_language.he]\nbase_url = \"http://y/v1\"\napi_key_env = \"HE_TOKEN\"\ninherit_token = true\n[polish]\nenabled = false\n",
                "stt.by_language.he: set api_key_env or inherit_token = true, not both",
            ),
        ];
        for (toml, expect) in cases {
            std::fs::write(&path, toml).unwrap();
            let msg = format!("{:#}", load(&path).unwrap_err());
            assert!(msg.contains(expect), "{toml:?}: {msg}");
        }
        std::fs::write(
            &path,
            "[stt]\nbase_url = \"http://x/v1\"\n[polish]\nenabled = false\n",
        )
        .unwrap();
        assert!(load(&path).is_ok(), "polish off needs no polish keys");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_threshold_outside_the_unit_range_names_the_key() {
        let dir = tempfile_dir("threshold_range");
        let path = dir.join("config.toml");
        let with = |key: &str, t: &str| {
            format!("[stt]\nbase_url = \"http://x/v1\"\n{key} = {t}\n[polish]\nenabled = false\n")
        };
        for key in ["no_speech_threshold", "no_speech_warn"] {
            for bad in ["1.5", "-0.1", "nan"] {
                std::fs::write(&path, with(key, bad)).unwrap();
                let msg = format!("{:#}", load(&path).unwrap_err());
                assert!(msg.contains(&format!("stt.{key}")), "{key}={bad}: {msg}");
                assert!(
                    msg.contains(&path.display().to_string()),
                    "{key}={bad}: {msg}"
                );
            }
        }
        for good in ["1.0", "0.6"] {
            std::fs::write(&path, with("no_speech_threshold", good)).unwrap();
            assert!(load(&path).is_ok(), "{good}");
        }
        std::fs::write(&path, with("no_speech_threshold", "0.0")).unwrap();
        let msg = format!("{:#}", load(&path).unwrap_err());
        assert!(msg.contains("stt.no_speech_warn"), "{msg}");
        assert!(msg.contains("set no_speech_warn = 0.0"), "{msg}");
        std::fs::write(
            &path,
            "[stt]\nbase_url = \"http://x/v1\"\nno_speech_threshold = 0.0\nno_speech_warn = 0.0\n[polish]\nenabled = false\n",
        )
        .unwrap();
        assert!(load(&path).is_ok());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_cue_volume_outside_the_unit_range_names_the_key() {
        let dir = tempfile_dir("cue_volume_range");
        let path = dir.join("config.toml");
        let with = |v: &str| {
            format!(
                "[stt]\nbase_url = \"http://x/v1\"\n[polish]\nenabled = false\n[indicator]\ncue_volume = {v}\n"
            )
        };
        for bad in ["1.5", "-0.1", "nan"] {
            std::fs::write(&path, with(bad)).unwrap();
            let msg = format!("{:#}", load(&path).unwrap_err());
            assert!(msg.contains("indicator.cue_volume"), "{bad}: {msg}");
            assert!(msg.contains("expected 0.0 to 1.0"), "{bad}: {msg}");
        }
        for good in ["0.0", "0.05", "1.0"] {
            std::fs::write(&path, with(good)).unwrap();
            assert_eq!(
                load(&path).unwrap().indicator.cue_volume,
                good.parse::<f64>().unwrap(),
                "{good}"
            );
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_default_microphone_is_the_system_one_and_padding_is_refused() {
        assert_eq!(CaptureConfig::default().device, "");
        assert_eq!(Config::default().capture.device, "");

        let dir = tempfile_dir("capture_device");
        let path = dir.join("config.toml");
        let with = |device: &str| {
            format!(
                "[stt]\nbase_url = \"http://x/v1\"\n[polish]\nenabled = false\n[capture]\ndevice = \"{device}\"\n"
            )
        };
        std::fs::write(&path, with("   ")).unwrap();
        let msg = format!("{:#}", load(&path).unwrap_err());
        assert!(msg.contains("capture.device is only whitespace"), "{msg}");
        for good in ["", "Microphone Array"] {
            std::fs::write(&path, with(good)).unwrap();
            assert!(load(&path).is_ok(), "{good:?}");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn unparsable_file_names_the_path() {
        let dir = tempfile_dir("unparsable");
        let path = dir.join("config.toml");
        std::fs::write(&path, "not = [toml").unwrap();
        let err = load(&path).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains(&path.display().to_string()), "{msg}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn resolve_token_prefers_env_then_file() {
        let dir = tempfile_dir("resolve_token");
        let f = dir.join("env");
        std::fs::write(&f, "OTHER=1\n=leaked\nMY_TOKEN=\"from-file\"\n").unwrap();
        // SAFETY: no other test touches `MY_TOKEN`; the environment API is internally synchronised on Windows.
        unsafe { std::env::remove_var("MY_TOKEN") };
        assert_eq!(
            resolve_token("MY_TOKEN", f.to_str().unwrap()),
            Some("from-file".into())
        );
        // An empty name matches nothing, so a bare `=leaked` line cannot supply a token.
        assert_eq!(resolve_token("", f.to_str().unwrap()), None);
        // SAFETY: as above.
        unsafe { std::env::set_var("MY_TOKEN", "from-env") };
        assert_eq!(
            resolve_token("MY_TOKEN", f.to_str().unwrap()),
            Some("from-env".into())
        );
        assert_eq!(resolve_token("", ""), None);
        assert_eq!(resolve_token("NOPE_UNSET", ""), None);
        // SAFETY: as above.
        unsafe { std::env::remove_var("MY_TOKEN") };
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn both_stages_resolve_a_token_from_a_key_file() {
        let dir = tempfile_dir("stt_key_file");
        let f = dir.join("env");
        std::fs::write(&f, "STT_TOKEN=\"from-file\"\nPOLISH_TOKEN=other\n").unwrap();
        // SAFETY: no other test touches these names; the environment API is internally synchronised on Windows.
        unsafe { std::env::remove_var("STT_TOKEN") };

        let cfg = SttConfig {
            api_key_env: "STT_TOKEN".into(),
            api_key_file: f.to_str().unwrap().into(),
            ..Default::default()
        };
        assert_eq!(
            resolve_token(&cfg.api_key_env, &cfg.api_key_file),
            Some("from-file".into())
        );
        let bare = SttConfig::default();
        assert_eq!(bare.api_key_file, "");
        assert_eq!(resolve_token(&bare.api_key_env, &bare.api_key_file), None);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn resolve_token_normalises_the_file_value() {
        let dir = tempfile_dir("resolve_token_value");

        let padded = dir.join("padded");
        std::fs::write(&padded, "PADDED_TOKEN=\" from-file \"\n").unwrap();
        assert_eq!(
            resolve_token("PADDED_TOKEN", padded.to_str().unwrap()),
            Some("from-file".into())
        );

        let blank = dir.join("blank");
        std::fs::write(&blank, "BLANK_TOKEN=\"\"\n").unwrap();
        assert_eq!(resolve_token("BLANK_TOKEN", blank.to_str().unwrap()), None);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn only_plain_http_to_somewhere_off_this_machine_warns() {
        for quiet in [
            "http://127.0.0.1",
            "http://127.0.0.1:8770/v1",
            "http://127.5.6.7:8770/v1",
            "http://localhost",
            "http://localhost:8770/v1",
            "http://LocalHost:8770/v1",
            "http://[::1]",
            "http://[::1]:8770/v1",
            "https://api.example.com/v1",
            "https://10.0.0.5:8770/v1",
            "HTTPS://api.example.com/v1",
            "",
            "your-whisper-host:8770/v1",
        ] {
            assert!(!is_cleartext_remote(quiet), "{quiet} must not warn");
        }
        for warns in [
            "http://10.0.0.5",
            "http://10.0.0.5:8770/v1",
            // 100.64.0.0/10 is Tailscale's CGNAT range: still a network, still in clear.
            "http://100.64.0.1:4000/v1",
            "http://your-whisper-host:8770/v1",
            "http://[2001:db8::1]:8770/v1",
            "HTTP://10.0.0.5:8770/v1",
            "  http://10.0.0.5:8770/v1  ",
            "http://user:pass@example.com/v1",
        ] {
            assert!(is_cleartext_remote(warns), "{warns} must warn");
        }
    }

    #[test]
    fn credentials_never_survive_into_a_printed_url() {
        assert_eq!(
            redact_userinfo("http://user:pass@example.com/v1"),
            "http://***@example.com/v1"
        );
        assert_eq!(
            redact_userinfo("https://token@10.0.0.5:8770/v1?x=1"),
            "https://***@10.0.0.5:8770/v1?x=1"
        );
        assert_eq!(
            redact_userinfo("http://us@er:p@ss@example.com/v1"),
            "http://***@example.com/v1"
        );
        for untouched in [
            "http://example.com/v1",
            "http://10.0.0.5:8770/v1",
            "http://[::1]:8770/v1",
            "http://example.com/a@b",
            "http://example.com/v1?to=a@b",
            "",
            "not-a-url",
        ] {
            assert_eq!(redact_userinfo(untouched), untouched, "{untouched}");
        }
        assert!(!redact_userinfo("http://u:hunter2@example.com/v1").contains("hunter2"));
    }

    /// A directory unique to this process and test; each caller removes it.
    fn tempfile_dir(test: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("byovox-test-{}-{test}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }
}
