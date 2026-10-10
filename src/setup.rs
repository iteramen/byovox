//! `byovox setup`: the wizard that writes a working config by editing `config::EXAMPLE` in place, keeping its comments.

use std::io::Write;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::audio::Audio;
use crate::check;
use crate::config::{self, Config, PolishConfig, SttConfig, SttLane, SttServer};
use crate::deploy::{self, Build, Model};
use crate::hotkey::parse_chord;
use crate::lang::Lang;
use crate::ollama;
use crate::server;
use crate::stt;

fn section_of(line: &str) -> Option<&str> {
    let t = line.trim();
    t.strip_prefix('[')?.strip_suffix(']')
}

/// Anchored at column zero with the exact `key = ` spelling: a commented `# he = "he"` is documentation, never edited.
fn assigns(line: &str, key: &str) -> bool {
    line.strip_prefix(key)
        .is_some_and(|rest| rest.starts_with(" = "))
}

pub fn set_key(text: &str, section: &str, key: &str, value: &str) -> Result<String> {
    let mut current = "";
    let mut done = false;
    let mut out: Vec<String> = Vec::new();
    for line in text.lines() {
        if let Some(s) = section_of(line) {
            current = s;
        } else if !done && current == section && assigns(line, key) {
            out.push(format!("{key} = {value}"));
            done = true;
            continue;
        }
        out.push(line.to_string());
    }
    if !done {
        bail!("`{key}` is not a key of `[{section}]` in the example config");
    }
    Ok(join(out))
}

/// For the open-ended maps `[language.by_layout]` and `[stt.by_language]`, which the example only shows commented.
pub fn append_to_section(text: &str, section: &str, entries: &[String]) -> Result<String> {
    if entries.is_empty() {
        return Ok(text.to_string());
    }
    let mut current = "";
    let mut placed = false;
    let mut out: Vec<String> = Vec::new();
    for line in text.lines() {
        if let Some(s) = section_of(line) {
            if !placed && current == section {
                place(&mut out, entries);
                placed = true;
            }
            current = s;
        }
        out.push(line.to_string());
    }
    if !placed && current == section {
        place(&mut out, entries);
        placed = true;
    }
    if !placed {
        bail!("`[{section}]` is not in the example config");
    }
    Ok(join(out))
}

fn place(out: &mut Vec<String>, entries: &[String]) {
    let blanks = out.iter().rev().take_while(|l| l.trim().is_empty()).count();
    let at = out.len() - blanks;
    out.splice(at..at, entries.iter().cloned());
}

/// Replaces in place, adds to the section, or creates it: a user-edited file may lack any key.
pub fn set_or_add_key(text: &str, section: &str, key: &str, value: &str) -> String {
    if let Ok(out) = set_key(text, section, key, value) {
        return out;
    }
    let entry = format!("{key} = {value}");
    if let Ok(out) = append_to_section(text, section, std::slice::from_ref(&entry)) {
        return out;
    }
    format!("{}\n\n[{section}]\n{entry}\n", text.trim_end())
}

/// LF with a trailing newline: the example file is pinned to LF.
fn join(lines: Vec<String>) -> String {
    let mut s = lines.join("\n");
    s.push('\n');
    s
}

pub fn toml_string(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

fn toml_list(items: &[String]) -> String {
    let inner: Vec<String> = items.iter().map(|s| toml_string(s)).collect();
    format!("[{}]", inner.join(", "))
}

const DEFAULT_KEY_FILE: &str = "~/.config/byovox/env";

/// End of input. There is no `--yes`: no answer could be invented for `stt.base_url`.
const NO_TERMINAL: &str = "setup needs a terminal — run `byovox config --init` and edit the file";

const ABORTED: &str = "aborted — nothing was written";

pub trait Prompter {
    fn ask(&mut self, question: &str) -> Option<String>;
}

pub struct Console;

impl Prompter for Console {
    fn ask(&mut self, question: &str) -> Option<String> {
        print!("{question} ");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        match std::io::stdin().read_line(&mut line) {
            Ok(0) => None,
            Ok(_) => Some(line.trim_end_matches(['\n', '\r']).to_string()),
            Err(e) => {
                eprintln!("reading stdin: {e}");
                None
            }
        }
    }
}

pub trait Probe {
    fn stt(&mut self, cfg: &Config) -> bool;
    fn polish(&mut self, cfg: &Config) -> bool;
    fn ollama(&mut self) -> Option<ollama::Models> {
        None
    }
    fn stt_port_in_use(&mut self) -> bool {
        true
    }
    fn nvidia_driver(&mut self) -> bool {
        false
    }
    fn downloaded(&mut self, _command: &str) -> bool {
        false
    }
    fn valid_server(&mut self, _server: &SttServer) -> bool {
        false
    }
    fn deploy(&mut self, build: Build, model: Model) -> Result<deploy::Installed, String>;
    fn mic(&mut self) -> bool {
        false
    }
    fn record(&mut self, cfg: &Config, secs: u64) -> Result<Audio, String> {
        let _ = (cfg, secs);
        Err("record: unimplemented".into())
    }
    fn check(&mut self, cfg: &Config, path: &Path) -> bool;
}

#[derive(Default)]
pub struct Stages {
    cuda: bool,
}

impl Probe for Stages {
    fn stt(&mut self, cfg: &Config) -> bool {
        let Some((server, health)) = server_to_start(cfg, deploy::port_in_use) else {
            return check::probe_stt(cfg).is_ok();
        };
        let Some((probed, stderr)) =
            deploy::with_server(server, &health, |_| check::probe_stt(cfg))
        else {
            return false;
        };
        for l in too_slow_note(&probed, cfg.stt.timeout_s)
            .into_iter()
            .flatten()
        {
            println!("{l}");
        }
        if self.cuda && stderr.contains(deploy::NO_GPU) {
            println!("  warning: whisper-server printed `{}`,", deploy::NO_GPU);
            println!("  so the CUDA build is running on the CPU. The CPU build does the same from");
            println!("  an {} download.", deploy::size(Build::Cpu.pin().bytes));
        }
        probed.is_ok()
    }
    fn polish(&mut self, cfg: &Config) -> bool {
        check::probe_polish(cfg)
    }
    fn ollama(&mut self) -> Option<ollama::Models> {
        ollama::discover(ollama::DEFAULT_HOST).filter(|m| !m.is_empty())
    }
    /// Elsewhere than Windows always `true`: there is no build to offer, so no offer is made.
    fn stt_port_in_use(&mut self) -> bool {
        no_download_offer(cfg!(windows), deploy::port_in_use)
    }
    fn nvidia_driver(&mut self) -> bool {
        deploy::nvidia_driver()
    }
    fn deploy(&mut self, build: Build, model: Model) -> Result<deploy::Installed, String> {
        self.cuda = build == Build::Cuda;
        deploy::install(build, model)
    }
    fn downloaded(&mut self, command: &str) -> bool {
        deploy::is_downloaded(command)
    }
    fn valid_server(&mut self, server: &SttServer) -> bool {
        server::validate(server).is_ok()
    }
    fn mic(&mut self) -> bool {
        true
    }
    fn record(&mut self, cfg: &Config, secs: u64) -> Result<Audio, String> {
        check::record(&cfg.capture.device, Duration::from_secs(secs))
    }
    fn check(&mut self, cfg: &Config, path: &Path) -> bool {
        let Some((server, health)) = server_to_start(cfg, deploy::port_in_use) else {
            return check::run(cfg, path);
        };
        deploy::with_server(server, &health, |_| check::run(cfg, path)).is_some_and(|(ok, _)| ok)
    }
}

/// The server `Stages` starts around a probe, unless something already answers on `stt.base_url`'s `host:port` or ureq cannot send to it.
fn server_to_start(
    cfg: &Config,
    port_in_use: impl FnOnce(&str) -> bool,
) -> Option<(&SttServer, String)> {
    if cfg.stt.server.command.is_empty() {
        return None;
    }
    let (addr, health) = deploy::server_endpoint(&cfg.stt.base_url)?;
    (!port_in_use(&addr)).then_some((&cfg.stt.server, health))
}

fn too_slow_note(probed: &Result<(), String>, timeout_s: u64) -> Option<[String; 4]> {
    probed
        .as_ref()
        .err()
        .filter(|e| stt::is_timeout(e))
        .map(|_| too_slow(timeout_s))
}

fn too_slow(timeout_s: u64) -> [String; 4] {
    [
        format!(
            "  The server did not transcribe 1 s of silence within stt.timeout_s ({timeout_s} s):"
        ),
        "  this model is likely too slow on this machine. Pick a smaller model, or the CUDA".into(),
        "  build if this machine has an NVIDIA GPU. Or keep this server, then raise timeout_s"
            .into(),
        "  under [stt] in the file setup writes; every dictation can then wait that long.".into(),
    ]
}

fn no_download_offer(windows: bool, port_in_use: impl FnOnce(&str) -> bool) -> bool {
    !windows || port_in_use(&format!("{}:{}", deploy::HOST, deploy::PORT))
}

#[derive(Debug, Clone, PartialEq)]
pub struct Answers {
    pub stt_base_url: String,
    pub stt_model: String,
    /// `stt_model` came from the old file with a hand-set server kept, so it is written even
    /// when empty: there an empty model is the user's own value, not an unasked question.
    pub stt_model_kept: bool,
    pub stt_api_key_env: String,
    pub stt_api_key_file: String,
    pub stt_hosted: bool,
    pub stt_timeout_s: u64,
    pub stt_connect_timeout_s: u64,
    pub stt_server_command: String,
    pub stt_server_args: Vec<String>,
    pub polish_enabled: bool,
    pub polish_base_url: String,
    pub polish_model: String,
    pub polish_api_key_env: String,
    pub polish_api_key_file: String,
    /// Written to `polish.hosted`. Also true for an Ollama model served by ollama.com
    /// rather than by the local daemon — that is a hosted service too.
    pub polish_hosted: bool,
    pub polish_timeout_s: u64,
    pub polish_connect_timeout_s: u64,
    pub hotkey_key: String,
    pub candidates: Vec<String>,
    pub by_layout: Vec<(String, String)>,
    pub lanes: Vec<(String, SttLane)>,
    pub capture_log: bool,
    pub capture_device: String,
    pub downloaded_build: Option<Build>,
    pub old_candidates: Vec<String>,
    pub old_by_layout: Vec<(String, String)>,
    pub hebrew_note_shown: bool,
}

impl Default for Answers {
    fn default() -> Self {
        let stt = SttConfig::default();
        let polish = PolishConfig::default();
        Self {
            stt_base_url: String::new(),
            stt_model: String::new(),
            stt_model_kept: false,
            stt_api_key_env: String::new(),
            stt_api_key_file: String::new(),
            stt_hosted: false,
            stt_timeout_s: stt.timeout_s,
            stt_connect_timeout_s: stt.connect_timeout_s,
            stt_server_command: String::new(),
            stt_server_args: Vec::new(),
            polish_enabled: true,
            polish_base_url: String::new(),
            polish_model: String::new(),
            polish_api_key_env: String::new(),
            polish_api_key_file: String::new(),
            polish_hosted: false,
            polish_timeout_s: polish.timeout_s,
            polish_connect_timeout_s: polish.connect_timeout_s,
            hotkey_key: "ControlRight".into(),
            candidates: Vec::new(),
            by_layout: Vec::new(),
            lanes: Vec::new(),
            capture_log: false,
            capture_device: String::new(),
            downloaded_build: None,
            old_candidates: Vec::new(),
            old_by_layout: Vec::new(),
            hebrew_note_shown: false,
        }
    }
}

fn edits(a: &Answers) -> Vec<(&'static str, &'static str, String)> {
    let mut v = vec![
        ("stt", "base_url", toml_string(&a.stt_base_url)),
        ("stt", "api_key_env", toml_string(&a.stt_api_key_env)),
        ("stt", "api_key_file", toml_string(&a.stt_api_key_file)),
        ("stt", "hosted", a.stt_hosted.to_string()),
        ("stt", "timeout_s", a.stt_timeout_s.to_string()),
        (
            "stt",
            "connect_timeout_s",
            a.stt_connect_timeout_s.to_string(),
        ),
        ("stt.server", "command", toml_string(&a.stt_server_command)),
        ("stt.server", "args", toml_list(&a.stt_server_args)),
        ("language", "candidates", toml_list(&a.candidates)),
        ("polish", "enabled", a.polish_enabled.to_string()),
        ("polish", "timeout_s", a.polish_timeout_s.to_string()),
        (
            "polish",
            "connect_timeout_s",
            a.polish_connect_timeout_s.to_string(),
        ),
        ("hotkey", "key", toml_string(&a.hotkey_key)),
        ("capture", "device", toml_string(&a.capture_device)),
        ("capture_log", "enabled", a.capture_log.to_string()),
    ];
    // Only when a model was picked or kept: otherwise the empty answer means nobody was asked,
    // and the example file's own `whisper-1` is the right value then.
    if !a.stt_model.is_empty() || a.stt_model_kept {
        v.push(("stt", "model", toml_string(&a.stt_model)));
    }
    // Nothing was asked about a gateway that will not be called, so its keys keep the empty
    // defaults the example already documents.
    if a.polish_enabled {
        v.extend([
            ("polish", "base_url", toml_string(&a.polish_base_url)),
            ("polish", "model", toml_string(&a.polish_model)),
            ("polish", "api_key_env", toml_string(&a.polish_api_key_env)),
            (
                "polish",
                "api_key_file",
                toml_string(&a.polish_api_key_file),
            ),
            ("polish", "hosted", a.polish_hosted.to_string()),
        ]);
    }
    v
}

pub fn render(a: &Answers) -> Result<String> {
    let mut text = config::EXAMPLE.to_string();
    for (section, key, value) in edits(a) {
        text = set_key(&text, section, key, &value)?;
    }
    let entries: Vec<String> = a
        .by_layout
        .iter()
        .map(|(layout, lang)| format!("{layout} = {}", toml_string(lang)))
        .collect();
    let text = append_to_section(&text, "language.by_layout", &entries)?;
    let lanes: Vec<String> = a
        .lanes
        .iter()
        .map(|(code, lane)| lane_entry(code, lane))
        .collect();
    append_to_section(&text, "stt.by_language", &lanes)
}

fn lane_entry(code: &str, lane: &SttLane) -> String {
    let mut fields = vec![format!("base_url = {}", toml_string(&lane.base_url))];
    if !lane.model.is_empty() {
        fields.push(format!("model = {}", toml_string(&lane.model)));
    }
    if lane.inherit_token {
        fields.push("inherit_token = true".into());
    }
    format!("{code} = {{ {} }}", fields.join(", "))
}

fn parse_base_url(s: &str) -> Result<String, String> {
    let url = s.trim();
    if url.is_empty() {
        return Err("required: byovox has no server it could guess".into());
    }
    reject_control_chars(url)?;
    let host = url
        .split_once("://")
        .filter(|(scheme, _)| {
            scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https")
        })
        .map(|(_, rest)| rest.split(['/', '?', '#']).next().unwrap_or_default())
        .ok_or_else(|| format!("`{url}` is not an http:// or https:// URL"))?;
    if host.is_empty() {
        return Err(format!("`{url}` names no host"));
    }
    Ok(url.trim_end_matches('/').to_string())
}

const MAX_ENV_NAME: usize = 32;

const NAME_SHAPE_FROM: usize = 20;

/// Case-sensitive: `BEARER_TOKEN` is a legal variable name.
const SECRET_PREFIXES: &[&str] = &[
    "ghp_",
    "gho_",
    "github_pat_",
    "sk-",
    "sk_",
    "xox",
    "AKIA",
    "AIza",
    "Bearer",
];

fn all_letters_upper(name: &str) -> bool {
    name.chars()
        .filter(char::is_ascii_alphabetic)
        .all(|c| c.is_ascii_uppercase())
}

/// Whether a legal-identifier answer is a credential rather than a variable name: shape rules, best effort,
/// erring towards refusing.
fn looks_like_a_secret(name: &str) -> bool {
    if name.len() > MAX_ENV_NAME {
        return true;
    }
    if SECRET_PREFIXES.iter().any(|p| name.starts_with(p)) {
        return true;
    }
    if name.len() < NAME_SHAPE_FROM {
        return false;
    }
    // Long enough to be a key: bare hex is one, and anything without a name's own shape —
    // an underscore, or letters that are all upper case — is treated as one.
    name.chars().all(|c| c.is_ascii_hexdigit()) || (!name.contains('_') && !all_letters_upper(name))
}

/// The *name* of an environment variable, never a token; refused outright when it looks like a key
/// (an accepted answer is written to the file and echoed in `check` rows). Empty = no Authorization header.
fn parse_env_name(s: &str) -> Result<String, String> {
    let name = s.trim();
    if name.is_empty() {
        return Ok(String::new());
    }
    let mut chars = name.chars();
    let shaped = chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !shaped {
        return Err(
            "that is not an environment variable name — a letter or `_`, then letters, digits \
             and `_`, e.g. `BYOVOX_API_KEY`. This is a name, not the secret."
                .into(),
        );
    }
    if looks_like_a_secret(name) {
        return Err(
            "this looks like a secret, not a variable name. byovox reads the token itself from \
             the environment or a key file, never from this prompt — give the NAME of the \
             variable that holds it, e.g. `BYOVOX_API_KEY`."
                .into(),
        );
    }
    Ok(name.to_string())
}

/// Refuse an answer carrying a control character: TOML basic strings forbid them and
/// `toml_string` does not escape them, so one would fail the parse at the end of the run.
fn reject_control_chars(s: &str) -> Result<(), String> {
    if s.chars().any(char::is_control) {
        return Err(
            "that answer contains a control character — retype it rather than pasting".into(),
        );
    }
    Ok(())
}

fn parse_key_file(s: &str) -> Result<String, String> {
    let path = s.trim();
    if path.is_empty() {
        return Ok(DEFAULT_KEY_FILE.to_string());
    }
    reject_control_chars(path)?;
    Ok(path.to_string())
}

/// A lane's base URL, or `None` on an empty line (the language keeps using `[stt]`); else held to `parse_base_url`.
fn parse_lane_url(s: &str) -> Result<Option<String>, String> {
    if s.trim().is_empty() {
        return Ok(None);
    }
    parse_base_url(s).map(Some)
}

/// Two endpoints with the same scheme and `config::host_of` (host and port), ASCII case aside.
fn same_server(a: &str, b: &str) -> bool {
    fn scheme(u: &str) -> &str {
        u.trim().split_once("://").map_or("", |(s, _)| s)
    }
    scheme(a).eq_ignore_ascii_case(scheme(b))
        && config::host_of(a).eq_ignore_ascii_case(config::host_of(b))
}

fn parse_optional(s: &str) -> Result<String, String> {
    let v = s.trim();
    reject_control_chars(v)?;
    Ok(v.to_string())
}

fn parse_required(s: &str) -> Result<String, String> {
    let v = s.trim();
    reject_control_chars(v)?;
    if v.is_empty() {
        Err("required".into())
    } else {
        Ok(v.to_string())
    }
}

fn parse_candidates(s: &str) -> Result<Vec<String>, String> {
    s.split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(parse_code)
        .collect()
}

/// `he=he, ru=ru` → `language.by_layout`. A layout named twice is refused: TOML would reject the duplicate key.
fn parse_by_layout(s: &str) -> Result<Vec<(String, String)>, String> {
    let mut out: Vec<(String, String)> = Vec::new();
    for pair in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let (layout, lang) = pair
            .split_once('=')
            .ok_or_else(|| format!("`{pair}` is not a `layout=language` pair, e.g. `he=he`"))?;
        let layout = parse_code(layout.trim())?;
        let lang = parse_code(lang.trim())?;
        if out.iter().any(|(l, _)| *l == layout) {
            return Err(format!("`{layout}` is mapped twice"));
        }
        out.push((layout, lang));
    }
    Ok(out)
}

fn parse_code(s: &str) -> Result<String, String> {
    Lang::parse(s)
        .map(|l| l.to_string())
        .ok_or_else(|| format!("`{s}` is not an ISO 639-1 code (two lowercase letters, e.g. `en`)"))
}

fn parse_yes_no(s: &str, default: bool) -> Result<bool, String> {
    match s.trim().to_ascii_lowercase().as_str() {
        "" => Ok(default),
        "y" | "yes" => Ok(true),
        "n" | "no" => Ok(false),
        other => Err(format!("`{other}`: answer y or n")),
    }
}

enum AfterFailure {
    Retry,
    Keep,
    Abort,
}

/// Re-asked until the answer parses. End of input is fatal: there is no terminal left to correct on.
fn ask<T>(
    p: &mut dyn Prompter,
    question: &str,
    parse: impl Fn(&str) -> Result<T, String>,
) -> Result<T> {
    loop {
        let Some(answer) = p.ask(question) else {
            bail!(NO_TERMINAL)
        };
        match parse(&answer) {
            Ok(v) => return Ok(v),
            Err(e) => println!("  {e}"),
        }
    }
}

/// The three ways out of a failed probe. "Keep anyway" exists because a server that is merely
/// not running yet is a perfectly good answer to write down.
fn after_failure(p: &mut dyn Prompter) -> Result<AfterFailure> {
    ask(p, "  retry, keep anyway, or abort? [R/k/a]", |s| {
        match s.trim().to_ascii_lowercase().as_str() {
            "" | "r" | "retry" => Ok(AfterFailure::Retry),
            "k" | "keep" => Ok(AfterFailure::Keep),
            "a" | "abort" => Ok(AfterFailure::Abort),
            other => Err(format!("`{other}`: answer r, k or a")),
        }
    })
}

/// The file question is asked only when the variable is unset here: naming it alone would send the request
/// unauthenticated, or fail the probe.
fn ask_token(p: &mut dyn Prompter) -> Result<(String, String)> {
    let env = ask(
        p,
        "  Bearer token — the NAME of an environment variable holding it \
         (empty = no Authorization header) []",
        parse_env_name,
    )?;
    if env.is_empty() || config::resolve_token(&env, "").is_some() {
        return Ok((env, String::new()));
    }
    println!("  {env} is not set in this environment.");
    println!(
        "  byovox can read it from a KEY=VALUE file instead: one line reading `{env}=<token>`."
    );
    println!("  That file holds a live credential in plain text and byovox neither checks nor");
    println!("  changes its permissions — keep it readable by you alone (Windows: Properties >");
    println!("  Security, remove the inherited groups; Linux/macOS: chmod 600).");
    let file = ask(
        p,
        &format!("  key file [{DEFAULT_KEY_FILE}]"),
        parse_key_file,
    )?;
    Ok((env, file))
}

fn partial_config(a: &Answers) -> Result<Config> {
    toml::from_str(&render(a)?).context("the file the wizard produced is not valid TOML")
}

/// A single local candidate and nothing hosted is announced and taken, not asked. The `bool` says whether the pick is hosted.
fn choose_model(
    p: &mut dyn Prompter,
    what: &str,
    local: &[String],
    cloud: &[String],
) -> Result<(String, bool)> {
    if let ([only], []) = (local, cloud) {
        println!("  using the only {what} model installed: {only}");
        return Ok((only.clone(), false));
    }
    println!("  {what} models Ollama can serve:");
    let entries: Vec<(&String, bool)> = local
        .iter()
        .map(|m| (m, false))
        .chain(cloud.iter().map(|m| (m, true)))
        .collect();
    for (i, (m, hosted)) in entries.iter().enumerate() {
        let note = if *hosted {
            "  (hosted by ollama.com — your text leaves this machine)"
        } else {
            ""
        };
        println!("    {}) {m}{note}", i + 1);
    }
    let n = entries.len();
    let picked = ask(p, &format!("  which one? [1-{n}]"), move |s| {
        s.trim()
            .parse::<usize>()
            .ok()
            .filter(|i| (1..=n).contains(i))
            .ok_or_else(|| format!("answer a number from 1 to {n}"))
    })?;
    let (m, hosted) = entries[picked - 1];
    Ok((m.clone(), hosted))
}

/// Asked first: only the user knows whether a `base_url` is a server they run or a hosted service.
#[derive(Clone, Copy, PartialEq)]
enum Source {
    Own,
    /// The local Ollama the probe found. Polish only: Ollama's runner refuses whisper models.
    Ollama,
    /// Somebody else's service, reached with a key. The only branch that says out loud what
    /// leaves the machine, and the one that writes `hosted = true`.
    Hosted,
}

fn ollama_option(what: &str, local: &[String], cloud: &[String]) -> Option<String> {
    if local.is_empty() && cloud.is_empty() {
        return None;
    }
    let hosted = if cloud.is_empty() {
        String::new()
    } else {
        format!(" and {} hosted by ollama.com", cloud.len())
    };
    Some(format!(
        "the local Ollama at {} — {} local {what} model(s){hosted}",
        ollama::DEFAULT_HOST,
        local.len()
    ))
}

fn ask_source(p: &mut dyn Prompter, ollama: Option<String>) -> Result<Source> {
    println!("    1) a server you run — this machine, or one on your network");
    let n = match &ollama {
        Some(line) => {
            println!("    2) {line}");
            println!("    3) a hosted service you have an API key for");
            3
        }
        None => {
            println!("    2) a hosted service you have an API key for");
            2
        }
    };
    // Not "which one?": `choose_model` asks exactly that, and both a test and a user reading
    // the transcript afterwards need to tell the endpoint pick from the model pick.
    let picked = ask(p, &format!("  choose [1-{n}]"), move |s| {
        s.trim()
            .parse::<usize>()
            .ok()
            .filter(|i| (1..=n).contains(i))
            .ok_or_else(|| format!("answer a number from 1 to {n}"))
    })?;
    Ok(match (picked, ollama.is_some()) {
        (1, _) => Source::Own,
        (2, true) => Source::Ollama,
        _ => Source::Hosted,
    })
}

/// Head of the hosted base-URL question: what this stage sends, and that byovox sends nothing else but the daily
/// version request and, if accepted, the one-time whisper.cpp download.
fn hosted_notice(what_leaves: &str) -> String {
    let mut out =
        format!("  A hosted service runs on someone else's machine: {what_leaves} will be sent");
    out.push_str("\n  there, and you are trusting it the way you trust your own keyboard.");
    out.push_str("\n  byovox sends nothing else anywhere - no telemetry, and no endpoint you");
    out.push_str("\n  did not name, except one anonymous version request to api.github.com");
    out.push_str("\n  per day (update.check = false turns it off), and the one-time whisper.cpp");
    out.push_str("\n  download setup offers, only if you said yes to it.\n");
    out
}

fn ask_menu(p: &mut dyn Prompter, head: &str, lines: &[String], what: &str) -> Result<usize> {
    let mut q = format!("{head}\n");
    for (i, line) in lines.iter().enumerate() {
        q.push_str(&format!("    {}) {line}\n", i + 1));
    }
    q.push_str(&format!("  {what} [1]"));
    let n = lines.len();
    let picked = ask(p, &q, move |s| match s.trim() {
        "" => Ok(1),
        s => s
            .parse::<usize>()
            .ok()
            .filter(|i| (1..=n).contains(i))
            .ok_or_else(|| format!("answer a number from 1 to {n}")),
    })?;
    Ok(picked - 1)
}

/// Offered only when nothing answers on 127.0.0.1:8770. `true` when installed and `a` points at it.
fn offer_download(p: &mut dyn Prompter, probe: &mut dyn Probe, a: &mut Answers) -> Result<bool> {
    let smallest = Build::Cpu.pin().bytes + Model::BaseQ8.pin().bytes;
    let largest = Build::Cuda.pin().bytes + Model::Turbo.pin().bytes;
    let head = format!(
        "\nNothing is answering on {host}:{port}, where a speech server on this machine would be.
  byovox can set one up: download whisper.cpp's whisper-server (build {tag}) and a Whisper
  model, check each against the size and SHA-256 pinned in byovox, and have the daemon start
  the server on {host}:{port} whenever it runs. {small:.2} to {large:.2} GB, into
  %LOCALAPPDATA%\\byovox\\data\\whisper-{tag}\\. Nothing is downloaded unless you say yes.
  Set one up? [y/N]",
        host = deploy::HOST,
        port = deploy::PORT,
        tag = deploy::BUILD_TAG,
        small = smallest as f64 / 1e9,
        large = largest as f64 / 1e9,
    );
    if !ask(p, &head, |s| parse_yes_no(s, false))? {
        return Ok(false);
    }
    let mut builds = vec![Build::Cpu, Build::Blas];
    if probe.nvidia_driver() {
        builds.push(Build::Cuda);
    }
    let lines: Vec<String> = builds
        .iter()
        .map(|b| {
            let pin = b.pin();
            format!(
                "{}, {}, {}",
                b.label(),
                pin.file_name(),
                deploy::size(pin.bytes)
            )
        })
        .collect();
    let build = builds[ask_menu(p, "  Which whisper.cpp build?", &lines, "build")?];
    let models = match build {
        Build::Cpu | Build::Blas => [Model::SmallQ8, Model::BaseQ8],
        Build::Cuda => [Model::Turbo, Model::TurboQ5],
    };
    let lines: Vec<String> = models
        .iter()
        .map(|m| {
            let pin = m.pin();
            let note = match m {
                Model::Turbo => "",
                Model::TurboQ5 => ", smaller download; accuracy not measured here",
                Model::SmallQ8 => ", about 5 s per dictation on a 6-core CPU at 12 threads",
                Model::BaseQ8 => ", fastest, rougher",
            };
            format!("{}, {}{note}", pin.file_name(), deploy::size(pin.bytes))
        })
        .collect();
    let mut head = "  Which model?".to_string();
    if build != Build::Cuda
        && lane_languages(&a.old_candidates, &a.old_by_layout)
            .iter()
            .any(|l| l == "he")
    {
        head = format!("{HEBREW_NOTE}\n{head}");
        a.hebrew_note_shown = true;
    }
    let model = models[ask_menu(p, &head, &lines, "model")?];
    match probe.deploy(build, model) {
        Ok(installed) => {
            a.stt_base_url = deploy::base_url();
            a.stt_hosted = false;
            a.stt_model.clear();
            a.stt_api_key_env.clear();
            a.stt_api_key_file.clear();
            let threads = std::thread::available_parallelism().ok().map(|n| n.get());
            a.stt_server_args = deploy::server_args(&installed.model, threads);
            a.stt_server_command = installed.server;
            a.downloaded_build = Some(build);
            Ok(true)
        }
        Err(e) => {
            println!("  {e}");
            Ok(false)
        }
    }
}

/// A server the old file names, offered back instead of the download (a re-run while the daemon holds 8770 would
/// otherwise write `command = ""`). The question names no URL: a `base_url` can carry `user:pass@`.
fn keep_earlier(
    p: &mut dyn Prompter,
    a: &mut Answers,
    earlier: &Answers,
    downloaded: bool,
) -> Result<bool> {
    let q = if downloaded {
        format!(
            "\n  Keep the local server set up earlier ({})? [Y/n]",
            earlier.stt_server_command
        )
    } else {
        format!(
            "\n  Keep the speech server your config names ({}) and its [stt] URL, model and \
             token? [Y/n]",
            earlier.stt_server_command
        )
    };
    let kept = ask(p, &q, |s| parse_yes_no(s, true))?;
    // Reset on a no too: a retry that declines what an earlier pass kept.
    a.stt_model_kept = kept && !downloaded;
    if !kept {
        return Ok(false);
    }
    a.stt_server_command = earlier.stt_server_command.clone();
    a.stt_server_args = earlier.stt_server_args.clone();
    a.stt_base_url = earlier.stt_base_url.clone();
    if downloaded {
        a.stt_hosted = false;
        a.stt_model.clear();
        a.stt_api_key_env.clear();
        a.stt_api_key_file.clear();
    } else {
        a.stt_hosted = earlier.stt_hosted;
        a.stt_model = earlier.stt_model.clone();
        a.stt_api_key_env = earlier.stt_api_key_env.clone();
        a.stt_api_key_file = earlier.stt_api_key_file.clone();
    }
    Ok(true)
}

fn ask_stt(p: &mut dyn Prompter, probe: &mut dyn Probe, a: &mut Answers) -> Result<()> {
    let local = probe.ollama().unwrap_or_default();
    // Empty is checked here because `server::validate` accepts it.
    let earlier = if a.stt_server_command.is_empty() {
        None
    } else if probe.downloaded(&a.stt_server_command) {
        Some((a.clone(), true))
    } else {
        let server = SttServer {
            command: a.stt_server_command.clone(),
            args: a.stt_server_args.clone(),
        };
        probe.valid_server(&server).then(|| (a.clone(), false))
    };
    loop {
        let local_server = match &earlier {
            Some((earlier, downloaded)) => keep_earlier(p, a, earlier, *downloaded)?,
            None => !probe.stt_port_in_use() && offer_download(p, probe, a)?,
        };
        if local_server {
            println!(
                "  starting the server [stt.server] names and transcribing a second of silence..."
            );
            if probe.stt(&partial_config(a)?) {
                return Ok(());
            }
            match after_failure(p)? {
                AfterFailure::Retry => continue,
                AfterFailure::Keep => return Ok(()),
                AfterFailure::Abort => bail!(ABORTED),
            }
        }
        // Cleared for the reason `stt_model` is below: a retry after a failed download probe
        // must not carry that server into an answer that names a different one.
        a.stt_server_command.clear();
        a.stt_server_args.clear();
        println!("\nWhere is your speech-to-text server?");
        if !local.unusable_speech.is_empty() {
            println!(
                "  Note: Ollama has {}, which it cannot run — a whisper",
                local.unusable_speech.join(", ")
            );
            println!("  model is built for whisper.cpp, and Ollama's runner refuses it with");
            println!("  `unknown model architecture`. Pointing byovox at it gives HTTP 500.");
            println!("  Speech needs its own server, e.g. whisper.cpp's whisper-server:");
            println!(
                "    whisper-server -m ggml-small-q8_0.bin --host 127.0.0.1 --port 8770 \
                 -l auto --inference-path /v1/audio/transcriptions"
            );
        }
        a.stt_hosted = ask_source(p, None)? == Source::Hosted;
        let url_q = if a.stt_hosted {
            format!(
                "{}  OpenAI-compatible base URL, e.g. https://api.example.com/v1 (required)",
                hosted_notice("your audio")
            )
        } else {
            "  OpenAI-compatible base URL, e.g. http://127.0.0.1:8770/v1 (required)".to_string()
        };
        a.stt_base_url = ask(p, &url_q, parse_base_url)?;
        if a.stt_hosted {
            // Only the hosted branch asks: whisper.cpp ignores the field, and the example
            // file's `whisper-1` is already the name the hosted APIs use for it.
            a.stt_model = ask(p, "  Model name (required, e.g. whisper-1)", parse_required)?;
        } else {
            // Cleared: a failed probe re-asks this group, and `edits` writes `stt.model` whenever it is
            // non-empty, so a stale hosted-branch name would reach the file.
            a.stt_model.clear();
        }
        let (env, file) = ask_token(p)?;
        a.stt_api_key_env = env;
        a.stt_api_key_file = file;
        println!("  transcribing a second of silence...");
        if probe.stt(&partial_config(a)?) {
            return Ok(());
        }
        match after_failure(p)? {
            AfterFailure::Retry => continue,
            AfterFailure::Keep => return Ok(()),
            AfterFailure::Abort => bail!(ABORTED),
        }
    }
}

fn ask_polish(p: &mut dyn Prompter, probe: &mut dyn Probe, a: &mut Answers) -> Result<()> {
    println!("\nPolish turns a raw transcript into punctuated text with the fillers removed.");
    println!("If it ever fails, the raw transcript is typed instead.");
    a.polish_enabled = ask(
        p,
        "  Clean up transcripts with a language model? [Y/n]",
        |s| parse_yes_no(s, true),
    )?;
    if !a.polish_enabled {
        return Ok(());
    }
    let local = probe.ollama().unwrap_or_default();
    loop {
        println!(
            "
Where does the clean-up model run?"
        );
        match ask_source(p, ollama_option("text", &local.text, &local.cloud))? {
            Source::Ollama => {
                let (model, is_cloud) = choose_model(p, "text", &local.text, &local.cloud)?;
                if is_cloud {
                    println!("  {model} is served by ollama.com, not by this machine: every");
                    println!("  dictation's text will be sent there (audio never is — speech is");
                    println!("  transcribed by your own server).");
                    if !ask(p, "  Send your text to ollama.com? [y/N]", |s| {
                        parse_yes_no(s, false)
                    })? {
                        continue;
                    }
                }
                a.polish_base_url = ollama::base_url(ollama::DEFAULT_HOST);
                a.polish_model = model;
                a.polish_api_key_env = String::new();
                a.polish_api_key_file = String::new();
                // An ollama.com model is a hosted service reached through the local daemon:
                // the URL is loopback and the text still leaves the machine.
                a.polish_hosted = is_cloud;
            }
            source => {
                a.polish_hosted = source == Source::Hosted;
                let url_q = if a.polish_hosted {
                    format!(
                        "{}  Chat-completions base URL, e.g. https://api.example.com/v1 (required)",
                        hosted_notice("every dictation's text")
                    )
                } else {
                    "  Chat-completions base URL, e.g. http://127.0.0.1:4000/v1 (required)"
                        .to_string()
                };
                a.polish_base_url = ask(p, &url_q, parse_base_url)?;
                a.polish_model = ask(
                    p,
                    "  Model name, or the alias your gateway serves (required)",
                    parse_required,
                )?;
                let (env, file) = ask_token(p)?;
                a.polish_api_key_env = env;
                a.polish_api_key_file = file;
            }
        }
        println!("  polishing a sample sentence...");
        if probe.polish(&partial_config(a)?) {
            return Ok(());
        }
        match after_failure(p)? {
            AfterFailure::Retry => continue,
            AfterFailure::Keep => return Ok(()),
            AfterFailure::Abort => bail!(ABORTED),
        }
    }
}

fn lane_languages(candidates: &[String], by_layout: &[(String, String)]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for code in candidates.iter().chain(by_layout.iter().map(|(_, l)| l)) {
        if !out.contains(code) {
            out.push(code.clone());
        }
    }
    out
}

const HEBREW_NOTE: &str = "  These CPU/BLAS models transcribe Hebrew poorly (60-80% WER \
    measured); a Hebrew speaker needs a GPU server or a [stt.by_language] lane for he.";

const LANE_NOTICE: &str = "
  A language can have its own speech-to-text endpoint. It gets the dictations whisper detects
  as that language, and those on a keyboard layout mapped to it when detection is unsure.
  Each one is one more server you trust the way you trust your own keyboard: it receives the
  audio of the dictations sent to it, and the transcript it returns is typed. It gets no
  token unless it names its own api_key_env or sets inherit_token = true for [stt]'s; setup
  sets inherit_token = true for a lane on [stt]'s scheme, host and port when [stt] names an
  api_key_env. A lane cannot be marked hosted, so `byovox check` prints no `note hosted` row
  for it.
";

const NO_LANE_LANGUAGES: &str = "  No per-language speech-to-text endpoint offered: setup offers \
     one for each language in the candidates or the layout map.";

/// Lanes are not probed (the closing `check` sends each one a request). A lane on `[stt]`'s scheme, host and port
/// gets `inherit_token` unasked when `[stt]` names an `api_key_env`.
fn ask_lanes(p: &mut dyn Prompter, a: &mut Answers) -> Result<()> {
    let languages = lane_languages(&a.candidates, &a.by_layout);
    if languages.is_empty() {
        println!("{NO_LANE_LANGUAGES}");
        return Ok(());
    }
    let mut head = if !a.hebrew_note_shown
        && languages.iter().any(|l| l == "he")
        && matches!(a.downloaded_build, Some(Build::Cpu | Build::Blas))
    {
        format!("{HEBREW_NOTE}\n{LANE_NOTICE}")
    } else {
        LANE_NOTICE.to_string()
    };
    for code in languages {
        let question = format!(
            "{head}  Dedicated speech-to-text endpoint for `{code}`? (base_url, empty = use [stt])"
        );
        head = String::new();
        let Some(base_url) = ask(p, &question, parse_lane_url)? else {
            continue;
        };
        let model = ask(
            p,
            &format!("  Model for `{code}` (empty = inherit stt.model)"),
            parse_optional,
        )?;
        a.lanes.push((
            code,
            SttLane {
                inherit_token: !a.stt_api_key_env.is_empty()
                    && same_server(&a.stt_base_url, &base_url),
                base_url,
                model,
                ..Default::default()
            },
        ));
    }
    Ok(())
}

pub fn interview_into(p: &mut dyn Prompter, probe: &mut dyn Probe, a: &mut Answers) -> Result<()> {
    ask_stt(p, probe, a)?;
    ask_polish(p, probe, a)?;

    println!("\nThe key you hold down to dictate.");
    a.hotkey_key = ask(
        p,
        "  Push-to-talk key — a key name or a chord, e.g. Insert or ControlLeft+ShiftLeft+Z \
         [ControlRight]",
        |s| {
            let name = match s.trim() {
                "" => "ControlRight",
                given => given,
            };
            parse_chord(name).map(|_| name.to_string())
        },
    )?;

    println!("\nLanguages. byovox lets the server detect by default.");
    a.candidates = ask(
        p,
        "  Constrain auto-detection to these languages (comma-separated ISO codes, \
         empty = server default) []",
        parse_candidates,
    )?;
    a.by_layout = ask(
        p,
        "  Map keyboard layouts to a language hint used when detection is unsure? e.g. he=he \
         (empty = none) []",
        parse_by_layout,
    )?;
    ask_lanes(p, a)?;

    println!("\nThe capture log keeps every dictation on this machine — the WAV plus a row of");
    println!("text — so you can score your own setup later; anything older than keep_days (30)");
    println!("is pruned automatically.");
    a.capture_log = ask(
        p,
        "  Keep a local copy of every dictation (audio + text) for your own re-scoring? [y/N]",
        |s| parse_yes_no(s, false),
    )?;
    Ok(())
}

pub fn interview(p: &mut dyn Prompter, probe: &mut dyn Probe) -> Result<Answers> {
    let mut a = Answers::default();
    interview_into(p, probe, &mut a)?;
    Ok(a)
}

fn ask_overwrite(p: &mut dyn Prompter, path: &Path) -> Result<bool> {
    ask(
        p,
        &format!("{} already exists. Overwrite it? [y/N]", path.display()),
        |s| parse_yes_no(s, false),
    )
}

/// Refuse to overwrite a config the user already has, unless they say so at a prompt naming
/// the file: the answers here are a small fraction of what a lived-in file holds.
fn confirm_overwrite(p: &mut dyn Prompter, path: &Path) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    if !ask_overwrite(p, path)? {
        bail!("left {} alone", path.display());
    }
    Ok(true)
}

/// `create_new` is check and write in one operation: an `exists()` first leaves a window as long as an interview.
/// `granted` is the answer asked before the interview; only an explicit `y` replaces a file that appeared meanwhile.
fn write_config(p: &mut dyn Prompter, path: &Path, text: &str, granted: bool) -> Result<()> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let failed = || format!("writing {}", path.display());
    if !granted {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
        {
            Ok(mut f) => return f.write_all(text.as_bytes()).with_context(failed),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                println!(
                    "\n  a config appeared at {} while setup was running.",
                    path.display()
                );
                if !ask_overwrite(p, path)? {
                    bail!(
                        "left {} alone — your answers were not written",
                        path.display()
                    );
                }
            }
            Err(e) => return Err(e).with_context(failed),
        }
    }
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
        .with_context(failed)?;
    f.write_all(text.as_bytes()).with_context(failed)
}

const MIC_LOW_MARGIN_DB: f32 = 10.0;
const MIC_LOW_ABSOLUTE_DBFS: f32 = -35.0;
const MIC_CLIP_DBFS: f32 = -1.0;

const MIC_INPUT_ENDED: &str = "  mic: input ended — skipping the level check";

fn mic_step(p: &mut dyn Prompter, probe: &mut dyn Probe, cfg: &Config) -> Result<()> {
    if !probe.mic() {
        return Ok(());
    }
    let check_now = loop {
        let Some(answer) = p.ask("  Check the microphone level now? [Y/n]") else {
            println!("{MIC_INPUT_ENDED}");
            return Ok(());
        };
        match parse_yes_no(&answer, true) {
            Ok(v) => break v,
            Err(e) => println!("  {e}"),
        }
    };
    if !check_now {
        return Ok(());
    }
    let Some(floor) = mic_record(
        p,
        probe,
        cfg,
        "  Stay quiet for 3 seconds — press Enter to start.",
        3,
    )?
    else {
        return Ok(());
    };
    let Some(speech) = mic_record(
        p,
        probe,
        cfg,
        "  Now say a sentence at your normal dictating volume — press Enter, then speak for 5 seconds.",
        5,
    )?
    else {
        return Ok(());
    };
    println!("{}", mic_report(&floor, &speech, cfg!(windows)));
    Ok(())
}

fn mic_record(
    p: &mut dyn Prompter,
    probe: &mut dyn Probe,
    cfg: &Config,
    prompt: &str,
    secs: u64,
) -> Result<Option<Audio>> {
    if p.ask(prompt).is_none() {
        println!("{MIC_INPUT_ENDED}");
        return Ok(None);
    }
    match probe.record(cfg, secs) {
        Ok(a) if a.duration_secs() >= secs as f32 / 2.0 => Ok(Some(a)),
        Ok(a) => {
            println!(
                "  mic: only {:.1}s of the asked {secs}s arrived",
                a.duration_secs()
            );
            Ok(None)
        }
        Err(e) => {
            println!("  mic: {e}");
            Ok(None)
        }
    }
}

fn mic_report(floor: &Audio, speech: &Audio, windows: bool) -> String {
    let floor_dbfs = floor.window_percentile_dbfs(50.0);
    let p95 = speech.window_percentile_dbfs(95.0);
    let loudest = speech.loudest_window_dbfs();
    let peak = speech.peak_dbfs();
    let mut out = format!(
        "  floor {floor_dbfs:.1} dBFS — sentence 95th-pct {p95:.1} dBFS, loudest {loudest:.1} dBFS, peak {peak:.1} dBFS"
    );
    let fix = if windows {
        "turn the input gain up, and turn off Audio enhancements for this device"
    } else {
        "turn the input gain up"
    };
    if speech.is_silent() {
        out.push_str(&format!(
            "\n  mic: this quiet, byovox would discard the dictation and type nothing — {fix}"
        ));
    } else if p95 - floor_dbfs <= MIC_LOW_MARGIN_DB || p95 < MIC_LOW_ABSOLUTE_DBFS {
        out.push_str(&format!("\n  mic: this level is low — {fix}"));
    }
    if peak >= MIC_CLIP_DBFS {
        out.push_str("\n  mic: clipping — turn the input gain down");
    }
    out
}

/// The wizard. `false` when the closing `check` failed: a file was written but is not one byovox
/// can dictate with yet. The exit code is `main`'s to choose.
pub fn run(path: &Path) -> Result<bool> {
    run_with(&mut Console, &mut Stages::default(), path)
}

fn run_with(p: &mut dyn Prompter, probe: &mut dyn Probe, path: &Path) -> Result<bool> {
    println!(
        "byovox setup — a few questions; the speech and clean-up servers are probed as you name them."
    );
    println!("Enter takes the default in brackets; Ctrl+C stops without writing anything.");
    println!("Config file: {}", path.display());
    let granted = confirm_overwrite(p, path)?;
    // Nothing is written unless every question was answered: a run that ends early leaves an
    // existing config as it was.
    let mut answers = Answers::default();
    if let Some(old) = std::fs::read_to_string(path)
        .ok()
        .and_then(|t| toml::from_str::<Config>(&t).ok())
    {
        answers.stt_server_command = old.stt.server.command;
        answers.stt_server_args = old.stt.server.args;
        answers.stt_base_url = old.stt.base_url;
        // These survive only a yes to a hand-set server: every other branch of `ask_stt`
        // overwrites or clears them.
        answers.stt_model = old.stt.model;
        answers.stt_api_key_env = old.stt.api_key_env;
        answers.stt_api_key_file = old.stt.api_key_file;
        answers.stt_hosted = old.stt.hosted;
        answers.stt_timeout_s = old.stt.timeout_s;
        answers.stt_connect_timeout_s = old.stt.connect_timeout_s;
        answers.polish_timeout_s = old.polish.timeout_s;
        answers.polish_connect_timeout_s = old.polish.connect_timeout_s;
        // A whitespace-only device fails `config::load` below; leaving it "" instead carries
        // forward the same "any device" the old file's value meant.
        if !old.capture.device.trim().is_empty() {
            answers.capture_device = old.capture.device;
        }
        answers.old_candidates = old.language.candidates;
        answers.old_by_layout = old.language.by_layout.into_iter().collect();
    }
    interview_into(p, probe, &mut answers)?;
    let text = render(&answers)?;
    toml::from_str::<Config>(&text).context("the file the wizard produced is not valid TOML")?;
    println!();
    // No `check::warn_cleartext` call here: the closing `check` prints those rows.
    write_config(p, path, &text, granted)?;
    println!("wrote {}\n", path.display());
    let cfg = config::load(path)?;
    mic_step(p, probe, &cfg)?;
    Ok(probe.check(&cfg, path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, EXAMPLE};

    #[test]
    fn setting_a_key_edits_only_that_line_and_keeps_every_comment() {
        let out = set_key(EXAMPLE, "stt", "base_url", "\"http://127.0.0.1:8770/v1\"").unwrap();
        assert!(out.contains("\nbase_url = \"http://127.0.0.1:8770/v1\"\n"));
        assert!(out.contains("# OpenAI-compatible base URL; byovox posts to"));
        assert_eq!(
            EXAMPLE.lines().count(),
            out.lines().count(),
            "an in-place edit adds and removes no lines"
        );
        assert_eq!(
            EXAMPLE.lines().filter(|l| l.starts_with('#')).count(),
            out.lines().filter(|l| l.starts_with('#')).count()
        );
    }

    #[test]
    fn a_key_of_the_same_name_in_another_section_is_untouched() {
        let out = set_key(EXAMPLE, "polish", "base_url", "\"http://p:4000/v1\"").unwrap();
        let cfg: Config = toml::from_str(&out).unwrap();
        assert_eq!(cfg.polish.base_url, "http://p:4000/v1");
        assert_eq!(cfg.stt.base_url, "");

        let out = set_key(EXAMPLE, "capture_log", "enabled", "true").unwrap();
        let cfg: Config = toml::from_str(&out).unwrap();
        assert!(cfg.capture_log.enabled);
        assert!(cfg.polish.enabled, "the polish flag is a different line");
    }

    #[test]
    fn a_commented_example_line_is_not_an_assignment() {
        assert!(!assigns("# he = \"he\"", "he"));
        assert!(!assigns("#base_url = \"\"", "base_url"));
        assert!(assigns("base_url = \"\"", "base_url"));
        assert!(!assigns("base_url_2 = \"\"", "base_url"));
        assert!(!assigns("timeout_s = 30", "timeout"));
    }

    #[test]
    fn a_key_that_is_not_in_the_section_is_refused() {
        let e = set_key(EXAMPLE, "stt", "no_such_key", "1").unwrap_err();
        assert!(e.to_string().contains("no_such_key"), "{e}");
        assert!(set_key(EXAMPLE, "stt", "key", "\"Insert\"").is_err());
    }

    #[test]
    fn layout_entries_land_under_the_map_and_nowhere_else() {
        let out = append_to_section(
            EXAMPLE,
            "language.by_layout",
            &["he = \"he\"".into(), "ru = \"ru\"".into()],
        )
        .unwrap();
        let cfg: Config = toml::from_str(&out).unwrap();
        assert_eq!(
            cfg.language.by_layout.get("he").map(String::as_str),
            Some("he")
        );
        assert_eq!(
            cfg.language.by_layout.get("ru").map(String::as_str),
            Some("ru")
        );
        assert!(
            out.contains("# he = \"he\"\nhe = \"he\"\nru = \"ru\"\n\n[polish]"),
            "{out}"
        );

        assert_eq!(
            append_to_section(EXAMPLE, "language.by_layout", &[]).unwrap(),
            EXAMPLE
        );
        assert!(append_to_section(EXAMPLE, "no.such.section", &["a = 1".into()]).is_err());
    }

    #[test]
    fn an_edited_example_round_trips_through_the_config_loader() {
        let out = set_key(EXAMPLE, "stt", "base_url", "\"http://127.0.0.1:8770/v1\"").unwrap();
        let out = set_key(&out, "polish", "enabled", "false").unwrap();
        let dir =
            std::env::temp_dir().join(format!("byovox-setup-{}-roundtrip", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, &out).unwrap();
        let cfg = crate::config::load(&path).expect("the edited example validates");
        assert_eq!(cfg.stt.base_url, "http://127.0.0.1:8770/v1");
        assert!(!cfg.polish.enabled);
        assert_eq!(cfg.hotkey.key, "ControlRight");
        assert_eq!(cfg.stt.no_speech_threshold, 0.3);
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("# byovox configuration.")
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_value_cannot_break_out_of_its_string_literal() {
        assert_eq!(toml_string("http://x/v1"), "\"http://x/v1\"");
        assert_eq!(toml_string(r#"a"b"#), r#""a\"b""#);
        assert_eq!(toml_string(r"C:\keys\env"), r#""C:\\keys\\env""#);
        let out = set_key(EXAMPLE, "stt", "api_key_file", &toml_string(r"C:\keys\env")).unwrap();
        let cfg: Config = toml::from_str(&out).unwrap();
        assert_eq!(cfg.stt.api_key_file, r"C:\keys\env");
    }

    #[test]
    fn every_key_the_wizard_writes_is_settable_exactly_once_in_the_example() {
        let a = Answers {
            polish_enabled: true,
            stt_model: "whisper-large-v3".into(),
            ..Default::default()
        };
        let written = edits(&a);
        assert!(written.len() >= 12, "{written:?}");
        for (section, key, _) in &written {
            let mut current = "";
            let mut hits = 0;
            for line in EXAMPLE.lines() {
                if let Some(s) = section_of(line) {
                    current = s;
                } else if current == *section && assigns(line, key) {
                    hits += 1;
                }
            }
            assert_eq!(hits, 1, "`{key}` under `[{section}]` in the example file");
        }
        assert!(EXAMPLE.contains("\n[language.by_layout]\n"));
        assert!(Config::default().language.by_layout.is_empty());
        assert!(EXAMPLE.contains("\n[stt.by_language]\n"));
        assert!(Config::default().stt.by_language.is_empty());
    }

    #[test]
    fn only_an_absolute_http_url_with_a_host_is_accepted() {
        for good in [
            "http://127.0.0.1:8770/v1",
            "https://api.example.com/v1",
            "HTTP://10.0.0.5:8770/v1",
            "  http://host:8770/v1  ",
        ] {
            assert!(parse_base_url(good).is_ok(), "{good}");
        }
        assert_eq!(
            parse_base_url("http://h:8770/v1/").unwrap(),
            "http://h:8770/v1",
            "a trailing slash is trimmed, as the clients trim it"
        );
        for bad in [
            "",
            "  ",
            "127.0.0.1:8770",
            "ftp://h/v1",
            "http://",
            "http:///v1",
        ] {
            assert!(parse_base_url(bad).is_err(), "{bad}");
        }
        assert!(
            parse_base_url("").unwrap_err().contains("required"),
            "the one question with no default says so"
        );
    }

    #[test]
    fn a_variable_name_is_accepted_and_anything_shaped_like_a_secret_is_not() {
        for good in [
            "MY_KEY",
            "OPENAI_API_KEY",
            "_x1",
            "key",
            "EXAMPLE_API_KEY",
            "  EXAMPLE_API_KEY  ",
            "_private",
            "GOOGLE_APPLICATION_CREDENTIALS",
            "K2",
        ] {
            assert_eq!(parse_env_name(good).unwrap(), good.trim(), "{good}");
        }
        assert_eq!(parse_env_name("").unwrap(), "", "empty = no token at all");

        for bad in ["sk-live-abc.def", "2MANY", "has space", "a=b"] {
            let e = parse_env_name(bad).unwrap_err();
            assert!(e.contains("not the secret"), "{bad}: {e}");
        }
    }

    #[test]
    fn a_credential_pasted_where_the_name_goes_is_refused_by_its_shape() {
        let pat = "ghp_123456789012345678901234567890123456";
        assert_eq!(pat.len(), 40, "the length the old threshold let through");

        let overlong = "A_".repeat(21);
        let cases = [
            (pat, "issuer prefix, and over the length bound"),
            ("ghp_1234567890", "issuer prefix alone: 14 chars, has `_`"),
            ("github_pat_11ABCDEFG0abcdefghijklm", "issuer prefix"),
            ("sk_live_51H8xY2abcdef", "issuer prefix, has `_`"),
            (
                "AKIAIOSFODNN7EXAMPLE",
                "issuer prefix: 20 chars, letters all upper, otherwise a fine name",
            ),
            ("AIzaSyD1234567890abcdefghijklmno", "issuer prefix"),
            ("xoxb1234567890abcdef", "issuer prefix"),
            ("a1b2c3d4a1b2c3d4a1b2c3d4a1b2c3d4", "bare hex, 32"),
            ("DEADBEEFCAFE12345678", "bare hex, 20, letters all upper"),
            ("aBcDeFgHiJkLmNoPqRsTuVwXyZ01", "28, mixed case, no `_`"),
            ("sk8Kj2LmNpQrStUvWxYz0123456789", "30, mixed case, no `_`"),
            (overlong.as_str(), "42, past the length bound"),
        ];
        for (bad, why) in cases {
            let e = parse_env_name(bad).unwrap_err();
            assert!(
                e.contains("this looks like a secret, not a variable name"),
                "{bad} ({why}): {e}"
            );
            assert!(!e.contains(bad), "the refusal repeated the answer: {e}");
        }
        assert!(
            !parse_env_name("sk-live-hunter2")
                .unwrap_err()
                .contains("hunter2"),
            "nor does the malformed-identifier refusal"
        );
    }

    #[test]
    fn language_answers_parse_the_way_the_loader_would_read_them() {
        assert_eq!(parse_candidates("").unwrap(), Vec::<String>::new());
        assert_eq!(parse_candidates(" en , ru ").unwrap(), ["en", "ru"]);
        assert_eq!(parse_candidates("en,").unwrap(), ["en"]);
        assert!(parse_candidates("english").is_err());
        assert!(parse_candidates("EN").is_err());

        assert_eq!(parse_by_layout("").unwrap(), Vec::new());
        assert_eq!(
            parse_by_layout(" he=he , ru=en ").unwrap(),
            [
                ("he".to_string(), "he".to_string()),
                ("ru".into(), "en".into())
            ]
        );
        assert!(
            parse_by_layout("he")
                .unwrap_err()
                .contains("layout=language")
        );
        assert!(parse_by_layout("he=auto").is_err());
        assert!(
            parse_by_layout("he=he, he=en")
                .unwrap_err()
                .contains("twice")
        );
    }

    #[test]
    fn a_control_character_re_asks_the_question_instead_of_ending_the_run() {
        for e in [
            parse_base_url("http://h:8770/v\u{b}1").unwrap_err(),
            parse_required("dict\u{7}ate").unwrap_err(),
            parse_key_file("~/.config/byovox/e\u{b}nv").unwrap_err(),
        ] {
            assert!(e.contains("control character"), "{e}");
        }
        assert_eq!(parse_key_file("").unwrap(), DEFAULT_KEY_FILE);
        assert_eq!(parse_key_file("  ~/x/env  ").unwrap(), "~/x/env");

        let mut p = Script::new(&[
            "1",                     // speech: a server you run
            "http://h:8770/v\u{b}1", // re-asked
            "http://127.0.0.1:8770/v1",
            "",  // no token
            "n", // no polish
            "",  // hotkey
            "",  // candidates
            "",  // by_layout
            "",  // capture log
        ]);
        let a = interview(&mut p, &mut Verdicts::new(&[true], &[])).unwrap();
        assert!(p.answers.is_empty(), "the script covered every question");
        assert_eq!(a.stt_base_url, "http://127.0.0.1:8770/v1");
        toml::from_str::<Config>(&render(&a).unwrap()).expect("valid TOML");
    }

    #[test]
    fn yes_no_takes_the_default_on_an_empty_line() {
        assert!(parse_yes_no("", true).unwrap());
        assert!(!parse_yes_no("", false).unwrap());
        assert!(parse_yes_no("Y", false).unwrap());
        assert!(parse_yes_no("yes", false).unwrap());
        assert!(!parse_yes_no("N", true).unwrap());
        assert!(parse_yes_no("maybe", true).is_err());
    }

    fn tone_dbfs(secs: f64, dbfs: f32) -> Audio {
        let amp = (i16::MAX as f32 * 10f32.powf(dbfs / 20.0)).round() as i16;
        Audio {
            samples: vec![amp; (crate::audio::SAMPLE_RATE as f64 * secs) as usize],
        }
    }

    fn with_peak(mut a: Audio, dbfs: f32) -> Audio {
        let amp = (i16::MAX as f32 * 10f32.powf(dbfs / 20.0)).round() as i16;
        a.samples.push(amp);
        a
    }

    #[test]
    fn a_healthy_room_and_sentence_print_all_four_levels_with_no_warning() {
        let floor = tone_dbfs(3.0, -45.0);
        let speech = with_peak(tone_dbfs(5.0, -18.0), -6.0);
        let report = mic_report(&floor, &speech, true);
        assert!(report.contains("floor -45.0 dBFS"), "{report}");
        assert!(report.contains("95th-pct -18.0 dBFS"), "{report}");
        assert!(report.contains("peak -6.0 dBFS"), "{report}");
        assert!(!report.contains("mic:"), "{report}");
    }

    #[test]
    fn a_sentence_within_ten_db_of_the_floor_is_low() {
        let floor = tone_dbfs(3.0, -40.0);
        let speech = tone_dbfs(5.0, -32.0);
        let report = mic_report(&floor, &speech, true);
        assert!(report.contains("mic: this level is low"), "{report}");
        assert!(!report.contains("discard"), "{report}");
        assert!(!report.contains("clipping"), "{report}");
    }

    #[test]
    fn a_sentence_under_the_absolute_low_level_is_low_however_far_from_the_floor() {
        let floor = tone_dbfs(3.0, -55.0);
        let speech = tone_dbfs(5.0, -36.0);
        let report = mic_report(&floor, &speech, true);
        assert!(report.contains("mic: this level is low"), "{report}");
    }

    #[test]
    fn a_silent_sentence_would_be_discarded_and_not_low() {
        let floor = tone_dbfs(3.0, -45.0);
        let speech = tone_dbfs(5.0, -42.0);
        assert!(
            speech.is_silent(),
            "the probe audio must actually be silent"
        );
        let report = mic_report(&floor, &speech, true);
        assert!(report.contains("mic: this quiet"), "{report}");
        assert!(report.contains("discard"), "{report}");
        assert!(!report.contains("is low"), "{report}");
    }

    #[test]
    fn a_full_scale_sample_clips() {
        let floor = tone_dbfs(3.0, -45.0);
        let speech = with_peak(tone_dbfs(5.0, -18.0), 0.0);
        assert!(speech.peak_dbfs() >= -1.0, "{}", speech.peak_dbfs());
        let report = mic_report(&floor, &speech, true);
        assert!(report.contains("mic: clipping"), "{report}");
        assert!(!report.contains("is low"), "{report}");
        assert!(!report.contains("discard"), "{report}");
    }

    #[test]
    fn a_noisy_floor_prints_with_no_warning_of_its_own() {
        let floor = tone_dbfs(3.0, -30.0);
        let speech = tone_dbfs(5.0, -15.0);
        let report = mic_report(&floor, &speech, true);
        assert!(report.contains("floor -30.0 dBFS"), "{report}");
        assert!(!report.contains("mic:"), "{report}");
    }

    #[test]
    fn the_enhancements_advice_is_windows_only() {
        let floor = tone_dbfs(3.0, -45.0);
        let speech = tone_dbfs(5.0, -42.0);
        let win = mic_report(&floor, &speech, true);
        let other = mic_report(&floor, &speech, false);
        assert!(win.contains("Audio enhancements"), "{win}");
        assert!(!other.contains("Audio enhancements"), "{other}");
        assert!(win.contains("input gain up") && other.contains("input gain up"));
    }

    struct Script {
        answers: std::collections::VecDeque<String>,
        asked: Vec<String>,
    }

    impl Script {
        fn new(answers: &[&str]) -> Script {
            Script {
                answers: answers.iter().map(|s| s.to_string()).collect(),
                asked: Vec::new(),
            }
        }
    }

    impl Prompter for Script {
        fn ask(&mut self, question: &str) -> Option<String> {
            self.asked.push(question.to_string());
            self.answers.pop_front()
        }
    }

    struct Verdicts {
        stt: std::collections::VecDeque<bool>,
        polish: std::collections::VecDeque<bool>,
        stt_seen: Vec<Config>,
        polish_seen: Vec<Config>,
        installed: Option<ollama::Models>,
        port_free: bool,
        nvidia: bool,
        deploy_fails: bool,
        deployed: Vec<(Build, Model)>,
        checked: Vec<String>,
        downloaded: bool,
        valid_server: bool,
        mic: bool,
        recordings: std::collections::VecDeque<Result<Audio, String>>,
        recorded: Vec<(String, u64)>,
    }

    const SERVER: &str =
        r"C:\Users\A User\AppData\Local\byovox\data\whisper-b5130\cpu\whisper-server.exe";
    const MODEL: &str =
        r"C:\Users\A User\AppData\Local\byovox\data\whisper-b5130\ggml-large-v3-turbo-q5_0.bin";

    impl Verdicts {
        fn new(stt: &[bool], polish: &[bool]) -> Verdicts {
            Verdicts {
                stt: stt.iter().copied().collect(),
                polish: polish.iter().copied().collect(),
                stt_seen: Vec::new(),
                polish_seen: Vec::new(),
                installed: None,
                port_free: false,
                nvidia: false,
                deploy_fails: false,
                deployed: Vec::new(),
                checked: Vec::new(),
                downloaded: false,
                valid_server: false,
                mic: false,
                recordings: std::collections::VecDeque::new(),
                recorded: Vec::new(),
            }
        }

        fn port_free(mut self) -> Verdicts {
            self.port_free = true;
            self
        }

        fn with_mic(mut self, recordings: Vec<Result<Audio, String>>) -> Verdicts {
            self.mic = true;
            self.recordings = recordings.into();
            self
        }

        fn with_ollama(mut self, speech: &[&str], text: &[&str], cloud: &[&str]) -> Verdicts {
            self.installed = Some(ollama::Models {
                unusable_speech: speech.iter().map(|s| s.to_string()).collect(),
                text: text.iter().map(|s| s.to_string()).collect(),
                cloud: cloud.iter().map(|s| s.to_string()).collect(),
            });
            self
        }
    }

    impl Probe for Verdicts {
        fn stt(&mut self, cfg: &Config) -> bool {
            self.stt_seen.push(cfg.clone());
            self.stt.pop_front().unwrap_or(true)
        }
        fn polish(&mut self, cfg: &Config) -> bool {
            self.polish_seen.push(cfg.clone());
            self.polish.pop_front().unwrap_or(true)
        }
        fn ollama(&mut self) -> Option<ollama::Models> {
            self.installed.clone()
        }
        fn stt_port_in_use(&mut self) -> bool {
            !self.port_free
        }
        fn nvidia_driver(&mut self) -> bool {
            self.nvidia
        }
        fn mic(&mut self) -> bool {
            self.mic
        }
        fn record(&mut self, cfg: &Config, secs: u64) -> Result<Audio, String> {
            self.recorded.push((cfg.capture.device.clone(), secs));
            self.recordings
                .pop_front()
                .unwrap_or_else(|| Err("no recording scripted".into()))
        }
        fn deploy(&mut self, build: Build, model: Model) -> Result<deploy::Installed, String> {
            self.deployed.push((build, model));
            if self.deploy_fails {
                return Err(deploy::MISSING_DLL.into());
            }
            Ok(deploy::Installed {
                server: SERVER.into(),
                model: MODEL.into(),
            })
        }
        fn check(&mut self, cfg: &Config, _path: &Path) -> bool {
            self.checked.push(cfg.stt.server.command.clone());
            true
        }
        fn downloaded(&mut self, _command: &str) -> bool {
            self.downloaded
        }
        fn valid_server(&mut self, _server: &SttServer) -> bool {
            self.valid_server
        }
    }

    #[test]
    fn a_local_ollama_is_offered_for_polish_and_the_pick_is_written() {
        let mut p = Script::new(&[
            "1",                        // speech: a server you run
            "http://127.0.0.1:8770/v1", // stt is still a server of its own
            "",                         // no token
            "",                         // polish? default yes
            "2",                        // polish: the local Ollama
            "2",                        // the second text model
            "",                         // hotkey
            "",                         // candidates
            "",                         // by_layout
            "",                         // capture log
        ]);
        let mut probes =
            Verdicts::new(&[true], &[true]).with_ollama(&[], &["llama3.2", "qwen2.5"], &[]);
        let a = interview(&mut p, &mut probes).unwrap();
        assert!(p.answers.is_empty(), "the script covered every question");
        assert_eq!(a.polish_base_url, ollama::base_url(ollama::DEFAULT_HOST));
        assert_eq!(a.polish_model, "qwen2.5");
        let cfg: Config = toml::from_str(&render(&a).unwrap()).unwrap();
        assert_eq!(cfg.polish.model, "qwen2.5");
    }

    #[test]
    fn a_whisper_model_in_ollama_is_never_offered_for_speech() {
        let mut p = Script::new(&[
            "1",                        // speech: a server you run, never an Ollama
            "http://127.0.0.1:8770/v1", // the URL question is asked, not skipped
            "",                         // no token
            "n",                        // polish off
            "",                         // hotkey
            "",                         // candidates
            "",                         // by_layout
            "",                         // capture log
        ]);
        let mut probes =
            Verdicts::new(&[true], &[]).with_ollama(&["whisper-large-v3"], &["llama3.2"], &[]);
        let a = interview(&mut p, &mut probes).unwrap();
        assert!(p.answers.is_empty(), "the script covered every question");
        assert_eq!(a.stt_base_url, "http://127.0.0.1:8770/v1");
        assert_eq!(a.stt_model, "", "nothing from Ollama reached stt.model");
        assert!(
            !p.asked.iter().any(|q| q.contains("speech-to-text?")),
            "Ollama was never offered for speech: {:?}",
            p.asked
        );
    }

    #[test]
    fn one_text_model_is_taken_without_asking_which() {
        let mut p = Script::new(&[
            "1", // speech: a server you run
            "http://127.0.0.1:8770/v1",
            "",  // no token
            "",  // polish? yes
            "2", // polish: the local Ollama — and it has only one model
            "",  // hotkey
            "",  // candidates
            "",  // by_layout
            "",  // capture log
        ]);
        let mut probes = Verdicts::new(&[true], &[true]).with_ollama(&[], &["llama3.2"], &[]);
        let a = interview(&mut p, &mut probes).unwrap();
        assert!(p.answers.is_empty(), "the script covered every question");
        assert_eq!(a.polish_model, "llama3.2");
        assert!(
            !p.asked.iter().any(|q| q.contains("which one?")),
            "{:?}",
            p.asked
        );
    }

    #[test]
    fn a_hosted_model_is_offered_apart_and_needs_a_yes() {
        let mut p = Script::new(&[
            "1", // speech: a server you run
            "http://127.0.0.1:8770/v1",
            "",  // no token
            "",  // polish? yes
            "2", // polish: the local Ollama
            "2", // the hosted one, listed after the local one
            "y", // send text to ollama.com: an explicit yes
            "",  // hotkey
            "",  // candidates
            "",  // by_layout
            "",  // capture log
        ]);
        let mut probes = Verdicts::new(&[true], &[true]).with_ollama(
            &[],
            &["gemma4:e2b"],
            &["gemma4:31b-cloud"],
        );
        let a = interview(&mut p, &mut probes).unwrap();
        assert!(p.answers.is_empty(), "the script covered every question");
        assert_eq!(a.polish_model, "gemma4:31b-cloud");
        assert!(
            p.asked.iter().any(|q| q.contains("ollama.com")),
            "the hosted pick was named as such: {:?}",
            p.asked
        );

        let mut p = Script::new(&[
            "1", // speech: a server you run
            "http://127.0.0.1:8770/v1",
            "",
            "",
            "2", // polish: the local Ollama
            "2",
            "",  // send text to ollama.com? default no
            "2", // the pick is asked again: Ollama
            "1", // the local one
            "",
            "",
            "",
            "",
        ]);
        let mut probes = Verdicts::new(&[true], &[true]).with_ollama(
            &[],
            &["gemma4:e2b"],
            &["gemma4:31b-cloud"],
        );
        let a = interview(&mut p, &mut probes).unwrap();
        assert!(p.answers.is_empty(), "the script covered every question");
        assert_eq!(a.polish_model, "gemma4:e2b");
    }

    #[test]
    fn a_hosted_pick_says_what_leaves_the_machine_and_is_recorded() {
        let mut p = Script::new(&[
            "2",                          // speech: a hosted service (no Ollama option here)
            "https://api.example.com/v1", // its base URL
            "not-whisper-1",              // its model. Not "whisper-1": that is the
            "",                           // no token
            "",                           // polish? default yes
            "2",                          // polish: a hosted service (no Ollama running)
            "https://api.example.com/v1",
            "some-chat-model",
            "OPENAI_COMPATIBLE_TOKEN", // an unset variable
            "",                        // key file: the default
            "",                        // hotkey
            "",                        // candidates
            "",                        // by_layout
            "",                        // capture log
        ]);
        let a = interview(&mut p, &mut Verdicts::new(&[true], &[true])).unwrap();
        assert!(p.answers.is_empty(), "the script covered every question");
        assert!(a.stt_hosted && a.polish_hosted);
        assert_eq!(a.stt_model, "not-whisper-1");
        assert_eq!(a.polish_base_url, "https://api.example.com/v1");
        assert_eq!(a.polish_model, "some-chat-model");
        assert_eq!(a.polish_api_key_env, "OPENAI_COMPATIBLE_TOKEN");

        let stt_q = p
            .asked
            .iter()
            .find(|q| q.contains("OpenAI-compatible"))
            .expect("the speech URL was asked");
        assert!(stt_q.contains("your audio will be sent"), "{stt_q}");
        assert!(
            stt_q.find("your audio").unwrap() < stt_q.find("OpenAI-compatible").unwrap(),
            "the notice comes before the question: {stt_q}"
        );
        let polish_q = p
            .asked
            .iter()
            .find(|q| q.contains("Chat-completions"))
            .expect("the polish URL was asked");
        assert!(
            polish_q.contains("every dictation's text will be sent"),
            "{polish_q}"
        );
        for q in [stt_q, polish_q] {
            assert!(!q.to_lowercase().contains("openai."), "{q}");
            assert!(!q.to_lowercase().contains("ollama"), "{q}");
        }

        let cfg: Config = toml::from_str(&render(&a).unwrap()).unwrap();
        assert!(cfg.stt.hosted && cfg.polish.hosted);
        assert_eq!(
            cfg.stt.model, "not-whisper-1",
            "the hosted answer reached the file, not the example default"
        );
    }

    #[test]
    fn a_server_you_run_is_recorded_as_not_hosted() {
        let mut p = Script::new(HAPPY);
        let a = interview(&mut p, &mut Verdicts::new(&[true], &[true])).unwrap();
        assert!(!a.stt_hosted && !a.polish_hosted);
        assert!(
            !p.asked.iter().any(|q| q.contains("will be sent")),
            "{:?}",
            p.asked
        );
        assert!(
            !p.asked
                .iter()
                .any(|q| q.contains("Model name (required, e.g. whisper-1)")),
            "speech asks for a model only on the hosted branch: {:?}",
            p.asked
        );
        let cfg: Config = toml::from_str(&render(&a).unwrap()).unwrap();
        assert!(!cfg.stt.hosted && !cfg.polish.hosted);
        assert_eq!(cfg.stt.model, "whisper-1", "the example's own value stands");
    }

    #[test]
    fn an_ollama_com_model_is_recorded_as_hosted_though_its_url_is_loopback() {
        let mut p = Script::new(&[
            "1", // speech: a server you run
            "http://127.0.0.1:8770/v1",
            "",  // no token
            "",  // polish? yes
            "2", // polish: the local Ollama
            "2", // the ollama.com model, listed after the local one
            "y", // yes, send text there
            "",
            "",
            "",
            "",
        ]);
        let mut probes = Verdicts::new(&[true], &[true]).with_ollama(
            &[],
            &["gemma4:e2b"],
            &["gemma4:31b-cloud"],
        );
        let a = interview(&mut p, &mut probes).unwrap();
        assert!(p.answers.is_empty(), "the script covered every question");
        assert_eq!(a.polish_base_url, ollama::base_url(ollama::DEFAULT_HOST));
        assert!(a.polish_hosted, "loopback URL, somebody else's machine");
        assert!(!a.stt_hosted);
        let cfg: Config = toml::from_str(&render(&a).unwrap()).unwrap();
        assert!(cfg.polish.hosted);
    }

    #[test]
    fn a_retry_does_not_keep_the_abandoned_branch_answer() {
        let mut p = Script::new(&[
            "2",                          // speech: a hosted service
            "https://api.example.com/v1", // its URL
            "not-whisper-1",              // its model, distinguishable from the default
            "",                           // no token
            "r",                          // the probe fails: retry the whole group
            "1",                          // this time, a server you run
            "http://127.0.0.1:8770/v1",
            "",  // no token
            "n", // polish off
            "",
            "",
            "",
            "",
        ]);
        let a = interview(&mut p, &mut Verdicts::new(&[false, true], &[])).unwrap();
        assert!(p.answers.is_empty(), "the script covered every question");
        assert!(!a.stt_hosted, "the second pick is the one that counts");
        assert_eq!(a.stt_base_url, "http://127.0.0.1:8770/v1");
        assert_eq!(a.stt_model, "", "the hosted attempt's model name is gone");
        let cfg: Config = toml::from_str(&render(&a).unwrap()).unwrap();
        assert!(!cfg.stt.hosted);
        assert_eq!(cfg.stt.model, "whisper-1", "the example's own value stands");
    }

    #[test]
    fn answers_given_before_an_abort_survive_it() {
        let mut p = Script::new(&["1", "http://127.0.0.1:8770/v1", "", "a"]);
        let mut a = Answers::default();
        let e = interview_into(&mut p, &mut Verdicts::new(&[false], &[]), &mut a).unwrap_err();
        assert_eq!(e.to_string(), ABORTED);
        assert_eq!(a.stt_base_url, "http://127.0.0.1:8770/v1");
        assert_eq!(a.hotkey_key, "ControlRight", "never reached, still valid");
        let cfg: Config = toml::from_str(&render(&a).unwrap()).expect("a partial file loads");
        assert_eq!(cfg.stt.base_url, "http://127.0.0.1:8770/v1");
        assert_eq!(cfg.hotkey.key, "ControlRight");
    }

    const HAPPY: &[&str] = &[
        "1",                        // speech: a server you run
        "http://127.0.0.1:8770/v1", // stt.base_url
        "",                         // stt token variable: none
        "",                         // polish? default yes
        "1",                        // polish: a server you run
        "http://127.0.0.1:4000/v1", // polish.base_url
        "polish-model",             // polish.model
        "",                         // polish token variable: none
        "",                         // hotkey: default ControlRight
        "en, ru",                   // language.candidates
        "he=he",                    // language.by_layout
        "",                         // en: no lane of its own
        "",                         // ru: none either
        "",                         // he, the mapped layout's language: none
        "y",                        // capture log
    ];

    #[test]
    fn the_happy_path_writes_every_answer_into_a_file_that_loads() {
        let mut p = Script::new(HAPPY);
        let mut probes = Verdicts::new(&[true], &[true]);
        let a = interview(&mut p, &mut probes).expect("every answer is valid");
        assert!(p.answers.is_empty(), "the script covered every question");

        assert_eq!(probes.stt_seen.len(), 1);
        assert_eq!(probes.stt_seen[0].stt.base_url, "http://127.0.0.1:8770/v1");
        assert_eq!(probes.polish_seen[0].polish.model, "polish-model");

        let dir = std::env::temp_dir().join(format!("byovox-setup-{}-happy", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, render(&a).unwrap()).unwrap();

        let cfg = crate::config::load(&path).expect("the wizard's file validates");
        assert_eq!(cfg.stt.base_url, "http://127.0.0.1:8770/v1");
        assert_eq!(cfg.stt.api_key_env, "");
        assert!(cfg.polish.enabled);
        assert_eq!(cfg.polish.base_url, "http://127.0.0.1:4000/v1");
        assert_eq!(cfg.polish.model, "polish-model");
        assert_eq!(cfg.hotkey.key, "ControlRight");
        assert_eq!(cfg.language.candidates, ["en", "ru"]);
        assert_eq!(
            cfg.language.by_layout.get("he").map(String::as_str),
            Some("he")
        );
        assert!(cfg.capture_log.enabled);
        assert_eq!(cfg.stt.model, "whisper-1");
        assert_eq!(cfg.inject.mode, "auto");

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# byovox configuration."));
        assert!(text.contains("# Sent as the `model` field."));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_port_in_use_proposes_no_download() {
        let mut p = Script::new(HAPPY);
        let result = interview(&mut p, &mut Verdicts::new(&[true], &[true]));
        assert!(
            !p.asked.iter().any(|q| q.contains("Set one up?")),
            "a download was proposed: {:?}",
            p.asked
        );
        assert!(p.asked[0].ends_with("choose [1-2]"), "{:?}", p.asked[0]);
        let a = result.unwrap();
        assert!(p.answers.is_empty(), "the script covered every question");
        assert_eq!(a.stt_server_command, "");
    }

    #[test]
    fn a_declined_or_failed_download_goes_on_to_the_source_question() {
        for (answer, fails) in [("", false), ("n", false), ("y", true)] {
            let mut script = vec![answer];
            if answer == "y" {
                script.extend(["1", "1"]); // CPU, small-q8_0
            }
            script.extend(["1", "http://127.0.0.1:9000/v1", "", "n", "", "", "", ""]);
            let mut p = Script::new(&script);
            let mut probes = Verdicts::new(&[true], &[]).port_free();
            probes.deploy_fails = fails;
            let result = interview(&mut p, &mut probes);
            let offer = &p.asked[0];
            assert!(offer.ends_with("Set one up? [y/N]"), "{offer}");
            assert!(offer.contains("0.09 to 2.30 GB"), "{offer}");
            assert!(
                offer.contains(r"%LOCALAPPDATA%\byovox\data\whisper-b5130\"),
                "{offer}"
            );
            let at = if fails { 3 } else { 1 };
            assert!(
                p.asked[at].ends_with("choose [1-2]"),
                "{answer:?} did not go on to the source question: {:?}",
                p.asked
            );
            let a = result.unwrap();
            assert!(p.answers.is_empty(), "the script covered every question");
            assert_eq!(probes.deployed.len(), usize::from(fails), "{answer:?}");
            assert_eq!(a.stt_base_url, "http://127.0.0.1:9000/v1");
            assert_eq!(a.stt_server_command, "");
            assert!(a.stt_server_args.is_empty());
        }
    }

    #[test]
    fn an_accepted_download_writes_the_server_and_its_url() {
        let mut p = Script::new(&["y", "1", "2", "n", "", "", "", ""]);
        let mut probes = Verdicts::new(&[true], &[]).port_free();
        let a = interview(&mut p, &mut probes).unwrap();
        assert!(p.answers.is_empty(), "the script covered every question");
        assert_eq!(probes.deployed, [(Build::Cpu, Model::BaseQ8)]);
        let model_q = p.asked.iter().find(|q| q.contains("Which model?")).unwrap();
        assert!(
            model_q.contains("2) ggml-base-q8_0.bin, 81.8 MB, fastest, rougher"),
            "{model_q}"
        );
        assert!(
            !p.asked.iter().any(|q| q.contains("OpenAI-compatible")),
            "no URL question: {:?}",
            p.asked
        );

        let threads = std::thread::available_parallelism()
            .unwrap()
            .get()
            .to_string();
        let args = [
            "-m",
            MODEL,
            "--host",
            "127.0.0.1",
            "--port",
            "8770",
            "-l",
            "auto",
            "--inference-path",
            "/v1/audio/transcriptions",
            "-t",
            threads.as_str(),
        ];
        assert_eq!(a.stt_server_args, args, "the argument list, in order");
        assert_eq!(
            probes.stt_seen[0].stt.server.command, SERVER,
            "probed with it"
        );

        let dir = std::env::temp_dir().join(format!("byovox-setup-{}-deploy", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, render(&a).unwrap()).unwrap();
        let cfg = crate::config::load(&path).expect("the wizard's file validates");
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(cfg.stt.server.command, SERVER);
        assert_eq!(cfg.stt.server.args, args);
        assert_eq!(cfg.stt.base_url, "http://127.0.0.1:8770/v1");
        assert!(!cfg.stt.hosted);
        assert_eq!(cfg.stt.model, "whisper-1", "the example's own value stands");
        assert_eq!(cfg.stt.api_key_env, "", "no token");
    }

    #[test]
    fn the_closing_check_is_handed_the_written_file_with_its_server() {
        let dir = std::env::temp_dir().join(format!("byovox-setup-{}-closing", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let mut p = Script::new(&["y", "1", "2", "n", "", "", "", ""]);
        let mut probes = Verdicts::new(&[true], &[]).port_free();
        let passed = run_with(&mut p, &mut probes, &path);
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(
            probes.checked,
            [SERVER],
            "the closing check was not handed the written server"
        );
        assert!(passed.unwrap());
    }

    #[test]
    fn the_mic_step_records_the_floor_then_the_sentence_against_the_written_config() {
        let first = interview(
            &mut Script::new(&["y", "1", "2", "n", "", "", "", ""]),
            &mut Verdicts::new(&[true], &[]).port_free(),
        )
        .unwrap();
        let old = render(&first).unwrap();
        let old = set_key(&old, "capture", "device", &toml_string("Microphone")).unwrap();

        let mut p = Script::new(&[
            "y", "", "n", "", "", "", "", // overwrite, keep server, no polish, 4 defaults
            "", // mic offer: Enter = yes
            "", // Enter to start the floor capture
            "", // Enter to start the sentence capture
        ]);
        let floor = Audio {
            samples: vec![300; 48_000],
        };
        let speech = Audio {
            samples: vec![3_000; 80_000],
        };
        let mut probes = Verdicts::new(&[true], &[]).with_mic(vec![Ok(floor), Ok(speech)]);
        probes.downloaded = true;
        let cfg = rerun("mic-order", &old, &mut p, &mut probes);
        assert!(p.answers.is_empty(), "the script covered every question");
        assert_eq!(cfg.capture.device, "Microphone");
        assert_eq!(
            probes.recorded,
            [("Microphone".to_string(), 3), ("Microphone".to_string(), 5)],
            "the floor before the sentence, against the device the written file names"
        );
    }

    #[test]
    fn declining_the_mic_offer_records_nothing() {
        let dir = std::env::temp_dir().join(format!("byovox-setup-{}-mic-no", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let mut p = Script::new(&[
            "1",
            "http://127.0.0.1:8770/v1",
            "",
            "n",
            "",
            "",
            "",
            "",
            "n", // mic offer declined
        ]);
        let mut probes = Verdicts::new(&[true], &[]).with_mic(vec![]);
        let passed = run_with(&mut p, &mut probes, &path);
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(passed.unwrap());
        assert!(p.answers.is_empty(), "the script covered every question");
        assert!(
            probes.recorded.is_empty(),
            "declining the offer recorded something"
        );
    }

    #[test]
    fn with_mic_off_the_offer_is_never_asked() {
        let dir = std::env::temp_dir().join(format!("byovox-setup-{}-mic-off", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let mut p = Script::new(&["1", "http://127.0.0.1:8770/v1", "", "n", "", "", "", ""]);
        let mut probes = Verdicts::new(&[true], &[]);
        let passed = run_with(&mut p, &mut probes, &path);
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(passed.unwrap());
        assert!(p.answers.is_empty(), "the script covered every question");
        assert!(
            !p.asked
                .iter()
                .any(|q| q.contains("Check the microphone level")),
            "the mic offer was asked though mic() is false: {:?}",
            p.asked
        );
    }

    #[test]
    fn a_record_error_does_not_stop_the_closing_check() {
        let dir = std::env::temp_dir().join(format!("byovox-setup-{}-mic-err", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let mut p = Script::new(&[
            "1",
            "http://127.0.0.1:8770/v1",
            "",
            "n",
            "",
            "",
            "",
            "",
            "", // mic offer: Enter = yes
            "", // Enter to start the floor capture
        ]);
        let mut probes = Verdicts::new(&[true], &[])
            .with_mic(vec![Err("microphone did not start within 5 s".into())]);
        let passed = run_with(&mut p, &mut probes, &path);
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(passed.unwrap());
        assert!(p.answers.is_empty(), "the script covered every question");
        assert_eq!(
            probes.recorded.len(),
            1,
            "the sentence capture ran despite the floor one failing"
        );
        assert_eq!(
            probes.checked.len(),
            1,
            "the closing check did not run after a record error"
        );
    }

    #[test]
    fn input_ending_at_the_mic_offer_still_reaches_the_closing_check() {
        let dir =
            std::env::temp_dir().join(format!("byovox-setup-{}-mic-eof1", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let mut p = Script::new(&["1", "http://127.0.0.1:8770/v1", "", "n", "", "", "", ""]);
        let mut probes = Verdicts::new(&[true], &[]).with_mic(vec![]);
        let passed = run_with(&mut p, &mut probes, &path);
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(passed.unwrap());
        assert!(probes.recorded.is_empty(), "no recording was attempted");
        assert_eq!(probes.checked.len(), 1, "the closing check did not run");
    }

    #[test]
    fn input_ending_at_the_first_mic_prompt_still_reaches_the_closing_check() {
        let dir =
            std::env::temp_dir().join(format!("byovox-setup-{}-mic-eof2", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let mut p = Script::new(&[
            "1",
            "http://127.0.0.1:8770/v1",
            "",
            "n",
            "",
            "",
            "",
            "",
            "", // mic offer: Enter = yes
        ]);
        let mut probes = Verdicts::new(&[true], &[]).with_mic(vec![]);
        let passed = run_with(&mut p, &mut probes, &path);
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(passed.unwrap());
        assert!(probes.recorded.is_empty(), "no recording was attempted");
        assert_eq!(probes.checked.len(), 1, "the closing check did not run");
    }

    #[test]
    fn a_rerun_with_the_port_in_use_keeps_the_downloaded_server() {
        let first = interview(
            &mut Script::new(&["y", "1", "2", "n", "", "", "", ""]),
            &mut Verdicts::new(&[true], &[]).port_free(),
        )
        .unwrap();
        let dir = std::env::temp_dir().join(format!("byovox-setup-{}-rerun", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, render(&first).unwrap()).unwrap();

        let mut p = Script::new(&["y", "", "n", "", "", "", ""]); // overwrite, keep (Enter)
        let mut probes = Verdicts::new(&[true], &[]); // 8770 in use
        probes.downloaded = true;
        let passed = run_with(&mut p, &mut probes, &path);
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        let keep = format!("Keep the local server set up earlier ({SERVER})? [Y/n]");
        assert!(
            p.asked.iter().any(|q| q.contains(&keep)),
            "no keep question: {:?}",
            p.asked
        );
        let cfg: Config = toml::from_str(&text).unwrap();
        assert_eq!(
            cfg.stt.server.command, SERVER,
            "the downloaded server was dropped"
        );
        assert_eq!(cfg.stt.server.args, first.stt_server_args);
        assert_eq!(cfg.stt.base_url, "http://127.0.0.1:8770/v1");
        assert!(!cfg.stt.hosted);
        assert!(passed.unwrap());
    }

    #[test]
    fn a_rerun_carries_hand_raised_timeouts_and_the_pinned_capture_device() {
        let first = interview(
            &mut Script::new(&["y", "1", "2", "n", "", "", "", ""]),
            &mut Verdicts::new(&[true], &[]).port_free(),
        )
        .unwrap();
        let dir =
            std::env::temp_dir().join(format!("byovox-setup-{}-timeouts", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let old = render(&first).unwrap();
        let old = set_key(&old, "stt", "timeout_s", "120").unwrap();
        let old = set_key(&old, "stt", "connect_timeout_s", "12").unwrap();
        let old = set_key(&old, "polish", "timeout_s", "90").unwrap();
        let old = set_key(&old, "polish", "connect_timeout_s", "7").unwrap();
        let old = set_key(&old, "capture", "device", &toml_string("Microphone")).unwrap();
        std::fs::write(&path, &old).unwrap();

        let mut p = Script::new(&["y", "", "n", "", "", "", ""]); // overwrite, keep (Enter)
        let mut probes = Verdicts::new(&[true], &[]); // 8770 in use
        probes.downloaded = true;
        let passed = run_with(&mut p, &mut probes, &path);
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(passed.unwrap());

        let cfg: Config = toml::from_str(&text).unwrap();
        assert_eq!(
            cfg.stt.timeout_s, 120,
            "the hand-raised stt timeout was reset"
        );
        assert_eq!(cfg.stt.connect_timeout_s, 12, "counted inside timeout_s");
        assert_eq!(
            cfg.polish.timeout_s, 90,
            "the hand-raised polish timeout was reset, though polish is off"
        );
        assert_eq!(
            cfg.polish.connect_timeout_s, 7,
            "the hand-raised polish connect timeout was reset, though polish is off"
        );
        assert_eq!(
            cfg.capture.device, "Microphone",
            "the pinned capture device was reset"
        );
        assert_eq!(
            probes.stt_seen[0].stt.timeout_s, 120,
            "the probe ran at the example's timeout, not the carried one"
        );
        assert_eq!(
            probes.stt_seen[0].polish.connect_timeout_s, 7,
            "the probe ran at the example's polish connect timeout, not the carried one"
        );
    }

    #[test]
    fn a_rerun_over_a_whitespace_only_capture_device_writes_it_empty() {
        let first = interview(
            &mut Script::new(&["y", "1", "2", "n", "", "", "", ""]),
            &mut Verdicts::new(&[true], &[]).port_free(),
        )
        .unwrap();
        let old = render(&first).unwrap();
        let old = set_key(&old, "capture", "device", &toml_string(" ")).unwrap();

        let mut p = Script::new(&["y", "", "n", "", "", "", ""]); // overwrite, keep (Enter)
        let mut probes = Verdicts::new(&[true], &[]); // 8770 in use
        probes.downloaded = true;
        let cfg = rerun("whitespace-device", &old, &mut p, &mut probes);
        assert_eq!(cfg.capture.device, "");
    }

    #[test]
    fn a_kept_download_keeps_its_args_byte_identical_no_threads_added() {
        let old_args = deploy::server_args(MODEL, None);
        let old = render(&Answers {
            stt_server_command: SERVER.into(),
            stt_server_args: old_args.clone(),
            stt_base_url: deploy::base_url(),
            ..Answers::default()
        })
        .unwrap();

        let mut p = Script::new(&["y", "", "n", "", "", "", ""]); // overwrite, keep (Enter)
        let mut probes = Verdicts::new(&[true], &[]); // 8770 in use
        probes.downloaded = true;
        let cfg = rerun("kept-no-threads", &old, &mut p, &mut probes);
        assert!(p.answers.is_empty(), "the script covered every question");
        assert_eq!(
            cfg.stt.server.args, old_args,
            "the keep path changed a downloaded server's args"
        );
        assert!(
            !cfg.stt.server.args.iter().any(|a| a == "-t"),
            "-t was added to a kept server: {:?}",
            cfg.stt.server.args
        );
    }

    #[test]
    fn unasked_timeouts_and_capture_device_render_byte_identical_to_the_example() {
        assert_eq!(render(&Answers::default()).unwrap(), EXAMPLE);
    }

    const HAND_SET: &str = r"D:\tools\whisper\whisper-server.exe";

    fn hand_set() -> Answers {
        Answers {
            stt_base_url: "http://127.0.0.1:9100/v1".into(),
            stt_model: "large-v3".into(),
            stt_api_key_env: "BYOVOX_SETUP_HAND_SET_TOKEN".into(),
            stt_api_key_file: r"D:\keys\byovox.env".into(),
            stt_hosted: true,
            stt_server_command: HAND_SET.into(),
            stt_server_args: vec!["--port".into(), "9100".into()],
            ..Answers::default()
        }
    }

    fn rerun(tag: &str, old: &str, p: &mut Script, probes: &mut Verdicts) -> Config {
        let dir = std::env::temp_dir().join(format!("byovox-setup-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, old).unwrap();
        let passed = run_with(p, probes, &path);
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(passed.unwrap());
        toml::from_str(&text).unwrap()
    }

    fn hand_set_keep() -> String {
        format!(
            "Keep the speech server your config names ({HAND_SET}) and its [stt] URL, model and \
             token? [Y/n]"
        )
    }

    #[test]
    fn a_rerun_with_the_port_in_use_keeps_a_hand_set_server_and_its_stt_settings() {
        let mut p = Script::new(&["y", "", "n", "", "", "", ""]); // overwrite, keep (Enter)
        let mut probes = Verdicts::new(&[true], &[]); // 8770 in use
        probes.valid_server = true;
        let cfg = rerun(
            "hand-set-kept",
            &render(&hand_set()).unwrap(),
            &mut p,
            &mut probes,
        );
        assert!(p.answers.is_empty(), "the script covered every question");
        assert!(
            p.asked[1].ends_with(&hand_set_keep()),
            "no hand-set keep question: {:?}",
            p.asked
        );
        assert_eq!(cfg.stt.server.command, HAND_SET, "the hand-set server");
        assert_eq!(cfg.stt.server.args, ["--port", "9100"]);
        assert_eq!(cfg.stt.base_url, "http://127.0.0.1:9100/v1");
        assert_eq!(cfg.stt.model, "large-v3");
        assert_eq!(cfg.stt.api_key_env, "BYOVOX_SETUP_HAND_SET_TOKEN");
        assert_eq!(cfg.stt.api_key_file, r"D:\keys\byovox.env");
        assert!(cfg.stt.hosted, "stt.hosted was cleared");
    }

    #[test]
    fn a_rerun_with_the_port_free_offers_the_hand_set_server_not_the_download() {
        let mut p = Script::new(&[
            "y", // overwrite
            "n", // keep the hand-set server: no
            "1",
            "http://127.0.0.1:9000/v1",
            "",
            "n",
            "",
            "",
            "",
            "",
        ]);
        let mut probes = Verdicts::new(&[true], &[]).port_free();
        probes.valid_server = true;
        let cfg = rerun(
            "hand-set-declined",
            &render(&hand_set()).unwrap(),
            &mut p,
            &mut probes,
        );
        assert!(p.answers.is_empty(), "the script covered every question");
        assert!(
            p.asked[1].ends_with(&hand_set_keep()),
            "no hand-set keep question: {:?}",
            p.asked
        );
        assert!(
            !p.asked.iter().any(|q| q.contains("Set one up?")),
            "the download was offered: {:?}",
            p.asked
        );
        assert!(
            p.asked[2].ends_with("choose [1-2]"),
            "a no did not go on to the source question: {:?}",
            p.asked
        );
        assert_eq!(
            cfg.stt.server.command, "",
            "the declined server was written"
        );
        assert_eq!(cfg.stt.base_url, "http://127.0.0.1:9000/v1");
        let stt = (
            cfg.stt.model.as_str(),
            cfg.stt.api_key_env.as_str(),
            cfg.stt.hosted,
        );
        assert_eq!(
            stt,
            ("whisper-1", "", false),
            "an old [stt] answer outlived a no"
        );
    }

    #[test]
    fn a_rerun_over_an_invalid_server_asks_no_keep_question() {
        let mut p = Script::new(&[
            "y",
            "1",
            "http://127.0.0.1:9000/v1",
            "",
            "n",
            "",
            "",
            "",
            "",
        ]);
        let mut probes = Verdicts::new(&[true], &[]); // 8770 in use, downloaded and valid false
        let cfg = rerun(
            "hand-set-invalid",
            &render(&hand_set()).unwrap(),
            &mut p,
            &mut probes,
        );
        assert!(p.answers.is_empty(), "the script covered every question");
        assert!(
            p.asked[1].ends_with("choose [1-2]"),
            "the source question was not the first: {:?}",
            p.asked
        );
        assert!(
            !p.asked.iter().any(|q| q.contains("Keep the")),
            "a keep question: {:?}",
            p.asked
        );
        assert_eq!(cfg.stt.server.command, "", "the invalid server was written");
        assert_eq!(cfg.stt.base_url, "http://127.0.0.1:9000/v1");
    }

    #[test]
    fn a_hand_set_keep_writes_an_empty_model_back_and_a_later_no_does_not() {
        let old = set_key(&render(&hand_set()).unwrap(), "stt", "model", r#""""#).unwrap();
        let mut p = Script::new(&["y", "", "n", "", "", "", ""]); // overwrite, keep (Enter)
        let mut probes = Verdicts::new(&[true], &[]);
        probes.valid_server = true;
        let cfg = rerun("hand-set-empty-model", &old, &mut p, &mut probes);
        assert_eq!(cfg.stt.model, "", "the empty model was not kept");

        let mut p = Script::new(&[
            "y", // overwrite
            "",  // keep the hand-set server
            "r", // its probe failed: retry
            "n", // keep it: no, this time
            "1",
            "http://127.0.0.1:9000/v1",
            "",
            "n",
            "",
            "",
            "",
            "",
        ]);
        let mut probes = Verdicts::new(&[false, true], &[]);
        probes.valid_server = true;
        let cfg = rerun("hand-set-empty-model-no", &old, &mut p, &mut probes);
        assert!(p.answers.is_empty(), "the script covered every question");
        assert_eq!(
            cfg.stt.model, "whisper-1",
            "an empty model outlived the no to its server"
        );
    }

    #[test]
    fn a_rerun_over_an_empty_command_asks_no_keep_question() {
        let mut old = hand_set();
        old.stt_server_command.clear();
        let mut p = Script::new(&[
            "y",
            "1",
            "http://127.0.0.1:9000/v1",
            "",
            "n",
            "",
            "",
            "",
            "",
        ]);
        let mut probes = Verdicts::new(&[true], &[]); // 8770 in use
        probes.valid_server = true;
        let cfg = rerun("empty-command", &render(&old).unwrap(), &mut p, &mut probes);
        assert!(
            p.asked[1].ends_with("choose [1-2]"),
            "an empty command was offered back: {:?}",
            p.asked
        );
        assert_eq!(cfg.stt.server.command, "");
    }

    #[test]
    fn a_downloaded_keep_clears_the_old_files_hosted_model_and_token() {
        let mut p = Script::new(&["y", "", "n", "", "", "", ""]); // overwrite, keep (Enter)
        let mut probes = Verdicts::new(&[true], &[]); // 8770 in use
        probes.downloaded = true;
        let cfg = rerun(
            "downloaded-clears",
            &render(&hand_set()).unwrap(),
            &mut p,
            &mut probes,
        );
        let keep = format!("Keep the local server set up earlier ({HAND_SET})? [Y/n]");
        assert!(p.asked[1].ends_with(&keep), "{:?}", p.asked);
        assert_eq!(cfg.stt.server.command, HAND_SET);
        let stt = (
            cfg.stt.hosted,
            cfg.stt.model.as_str(),
            cfg.stt.api_key_env.as_str(),
            cfg.stt.api_key_file.as_str(),
        );
        assert_eq!(
            stt,
            (false, "whisper-1", "", ""),
            "the downloaded keep carried the old [stt] over"
        );
    }

    #[test]
    fn stages_offers_back_only_a_server_the_daemon_would_start() {
        let exe = std::env::current_exe().unwrap().display().to_string();
        let server = |command: &str| SttServer {
            command: command.into(),
            args: Vec::new(),
        };
        assert!(Stages::default().valid_server(&server(&exe)), "{exe}");
        assert!(
            !Stages::default().valid_server(&server("whisper-server.exe")),
            "a relative command was offered back"
        );
    }

    #[test]
    fn a_server_is_started_for_a_probe_only_when_named_and_its_port_is_free() {
        let mut cfg = Config::default();
        assert!(
            server_to_start(&cfg, |_| panic!("the port was tried with no server named")).is_none()
        );
        cfg.stt.server.command = SERVER.into();
        cfg.stt.base_url = deploy::base_url();
        assert_eq!(
            server_to_start(&cfg, |_| false).map(|(_, h)| h).as_deref(),
            Some("http://127.0.0.1:8770/health"),
            "a named server on a free port"
        );
        assert!(
            server_to_start(&cfg, |_| true).is_none(),
            "a second server was started beside one already on the port"
        );
        cfg.stt.base_url = "127.0.0.1:9100/v1".into();
        assert!(
            server_to_start(&cfg, |_| panic!(
                "the port of a base_url ureq cannot use was tried"
            ))
            .is_none()
        );
    }

    #[test]
    fn the_too_slow_note_names_the_configured_timeout() {
        let note = too_slow(45).join("\n");
        assert!(note.contains("45 s"), "{note}");
        assert!(note.contains("timeout_s"), "{note}");
        assert!(!note.contains("30"), "{note}");
        assert!(note.contains("keep this server"), "{note}");
    }

    #[test]
    fn only_a_timed_out_probe_gets_the_too_slow_note() {
        let timed_out = Err("stt timeout: no reply within 45 s".to_string());
        assert_eq!(too_slow_note(&timed_out, 45), Some(too_slow(45)));
        for probed in [
            Ok(()),
            Err("stt transport: io: Connection refused".to_string()),
            Err("stt HTTP 500".to_string()),
        ] {
            assert_eq!(too_slow_note(&probed, 45), None, "{probed:?}");
        }
    }

    #[test]
    fn a_server_already_answering_on_its_own_port_is_not_started_again() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let mut cfg = Config::default();
        cfg.stt.server.command = SERVER.into();
        cfg.stt.base_url = format!("http://{}/v1", listener.local_addr().unwrap());
        assert!(
            server_to_start(&cfg, deploy::port_in_use).is_none(),
            "a second server was started beside one answering on {}",
            cfg.stt.base_url
        );
        cfg.stt.base_url = "http://127.0.0.1:0/v1".into();
        assert!(
            server_to_start(&cfg, deploy::port_in_use).is_some(),
            "nothing answers on {}",
            cfg.stt.base_url
        );
    }

    #[cfg(windows)]
    #[test]
    fn the_stages_offer_asks_the_real_port() {
        assert_eq!(
            Stages::default().stt_port_in_use(),
            deploy::port_in_use("127.0.0.1:8770"),
            "the offer did not ask 127.0.0.1:8770"
        );
    }

    #[test]
    fn no_download_is_offered_off_windows_and_the_port_decides_on_it() {
        assert!(no_download_offer(false, |_| panic!(
            "the port was tried off Windows"
        )));
        let mut tried = String::new();
        assert!(
            !no_download_offer(true, |a| {
                tried = a.to_string();
                false
            }),
            "Windows, port free"
        );
        assert_eq!(tried, "127.0.0.1:8770", "the port tried for the offer");
        assert!(no_download_offer(true, |_| true), "Windows, port taken");
    }

    #[test]
    fn the_cuda_build_is_listed_only_with_the_nvidia_driver() {
        for nvidia in [false, true] {
            let mut p = Script::new(&["y", "1", "1", "n", "", "", "", ""]);
            let mut probes = Verdicts::new(&[true], &[]).port_free();
            probes.nvidia = nvidia;
            interview(&mut p, &mut probes).unwrap();
            let menu = p
                .asked
                .iter()
                .find(|q| q.contains("Which whisper.cpp build?"))
                .unwrap();
            assert!(
                menu.contains("1) CPU, whisper-bin-x64.zip, 8.6 MB"),
                "{menu}"
            );
            assert!(
                menu.contains("2) CPU with OpenBLAS, whisper-blas-bin-x64.zip, 21.4 MB"),
                "{menu}"
            );
            assert_eq!(
                menu.contains("3) NVIDIA CUDA 12.4, whisper-cublas-12.4.0-bin-x64.zip, 675 MB"),
                nvidia,
                "{menu}"
            );
        }
    }

    #[test]
    fn enter_at_the_model_menu_deploys_the_per_build_recommendation() {
        for (build_answer, nvidia, want) in [
            ("1", false, (Build::Cpu, Model::SmallQ8)),
            ("2", false, (Build::Blas, Model::SmallQ8)),
            ("3", true, (Build::Cuda, Model::Turbo)),
        ] {
            let mut p = Script::new(&["y", build_answer, "", "n", "", "", "", ""]);
            let mut probes = Verdicts::new(&[true], &[]).port_free();
            probes.nvidia = nvidia;
            interview(&mut p, &mut probes).unwrap();
            assert_eq!(probes.deployed, [want], "{build_answer}");
            let model_q = p.asked.iter().find(|q| q.contains("Which model?")).unwrap();
            assert_eq!(model_q.contains("turbo"), build_answer == "3", "{model_q}");
        }
    }

    #[test]
    fn the_hebrew_note_follows_the_old_files_candidates_and_never_shows_for_cuda() {
        for (i, (candidates, build_answer, nvidia, want)) in [
            (vec!["he".to_string()], "1", false, true),
            (vec!["en".to_string()], "1", false, false),
            (vec!["he".to_string()], "3", true, false),
        ]
        .into_iter()
        .enumerate()
        {
            let old = render(&Answers {
                candidates,
                ..Answers::default()
            })
            .unwrap();
            let mut p = Script::new(&["y", "y", build_answer, "", "n", "", "", "", "n"]);
            let mut probes = Verdicts::new(&[true], &[]).port_free();
            probes.nvidia = nvidia;
            let _cfg = rerun(&format!("hebrew-menu-{i}"), &old, &mut p, &mut probes);
            assert!(p.answers.is_empty(), "the script covered every question");
            let model_q = p.asked.iter().find(|q| q.contains("Which model?")).unwrap();
            assert_eq!(model_q.contains("Hebrew"), want, "{model_q}");
            assert!(
                probes.stt_seen[0].language.candidates.is_empty(),
                "the old file's candidates reached the probe: {:?}",
                probes.stt_seen[0].language.candidates
            );
        }
    }

    #[test]
    fn the_hebrew_note_appears_before_the_first_lane_question_on_a_fresh_cpu_download() {
        let mut p = Script::new(&[
            "y",  // Set one up?
            "1",  // build: CPU
            "",   // model: Enter -> SmallQ8
            "n",  // polish: decline
            "",   // hotkey: default
            "he", // language.candidates
            "",   // language.by_layout
            "",   // he: no lane of its own
            "n",  // capture log
        ]);
        let mut probes = Verdicts::new(&[true], &[]).port_free();
        interview(&mut p, &mut probes).unwrap();
        assert!(p.answers.is_empty(), "the script covered every question");
        let model_q = p.asked.iter().find(|q| q.contains("Which model?")).unwrap();
        assert!(!model_q.contains("Hebrew"), "{model_q}");
        let lane_q = p
            .asked
            .iter()
            .find(|q| q.contains("Dedicated speech-to-text endpoint"))
            .unwrap();
        assert!(lane_q.contains("Hebrew"), "{lane_q}");
    }

    #[test]
    fn the_hebrew_note_shows_once_on_a_rerun_that_also_downloads() {
        let old = render(&Answers {
            candidates: vec!["he".into()],
            ..Answers::default()
        })
        .unwrap();
        let mut p = Script::new(&[
            "y",  // overwrite
            "y",  // Set one up?
            "1",  // build: CPU
            "",   // model: Enter -> SmallQ8
            "n",  // polish: decline
            "",   // hotkey: default
            "he", // language.candidates
            "",   // language.by_layout
            "",   // he: no lane of its own
            "n",  // capture log
        ]);
        let mut probes = Verdicts::new(&[true], &[]).port_free();
        let _cfg = rerun("hebrew-once", &old, &mut p, &mut probes);
        assert!(p.answers.is_empty(), "the script covered every question");
        let notes = p
            .asked
            .iter()
            .filter(|q| q.contains("transcribe Hebrew poorly"))
            .count();
        assert_eq!(notes, 1, "{:?}", p.asked);
    }

    #[test]
    fn a_retry_after_a_download_does_not_keep_its_server() {
        let mut p = Script::new(&[
            "y",
            "1",
            "1",
            "r", // the downloaded server failed its probe
            "n", // offered again: no
            "1",
            "http://127.0.0.1:9000/v1",
            "",
            "n",
            "",
            "",
            "",
            "",
        ]);
        let mut probes = Verdicts::new(&[false, true], &[]).port_free();
        let a = interview(&mut p, &mut probes).unwrap();
        assert!(p.answers.is_empty(), "the script covered every question");
        assert_eq!(a.stt_base_url, "http://127.0.0.1:9000/v1");
        assert_eq!(a.stt_server_command, "", "the abandoned download's server");
        assert!(a.stt_server_args.is_empty(), "{:?}", a.stt_server_args);
    }

    #[test]
    fn no_language_named_asks_no_lane_question_and_renders_as_before() {
        let mut p = Script::new(&["1", "http://127.0.0.1:8770/v1", "", "n", "", "", "", ""]);
        let a = interview(&mut p, &mut Verdicts::new(&[true], &[])).unwrap();
        assert!(p.answers.is_empty(), "the script covered every question");
        assert!(
            !p.asked
                .iter()
                .any(|q| q.contains("speech-to-text endpoint")),
            "{:?}",
            p.asked
        );
        assert!(a.lanes.is_empty());
        let mut before = EXAMPLE.to_string();
        for (section, key, value) in edits(&a) {
            before = set_key(&before, section, key, &value).unwrap();
        }
        assert_eq!(render(&a).unwrap(), before, "byte for byte");
    }

    #[test]
    fn a_lane_entry_escapes_what_toml_would_misread() {
        let lane = SttLane {
            base_url: r##"http://127.0.0.1:8770/he/v1?a="b"#c"##.into(),
            model: r##"m "x" \y #z שלום"##.into(),
            ..Default::default()
        };
        let line = lane_entry("he", &lane);
        let back: toml::Table = toml::from_str(&line).expect("the lane line parses");
        let he = back["he"].as_table().unwrap();
        assert_eq!(
            he["base_url"].as_str(),
            Some(lane.base_url.as_str()),
            "{line}"
        );
        assert_eq!(he["model"].as_str(), Some(lane.model.as_str()), "{line}");
    }

    #[test]
    fn a_lane_answer_is_written_under_the_lane_table_and_loads() {
        let mut p = Script::new(&[
            "1",
            "http://127.0.0.1:8770/v1",
            "",
            "n",
            "",
            "en, ru, he",                  // candidates
            "",                            // by_layout
            "",                            // en: [stt]
            "http://127.0.0.1:8770/ru/v1", // ru: a lane, model inherited
            "",
            "http://127.0.0.1:8770/he/v1", // he: a lane with a model of its own
            "ivrit-large-v3",
            "", // capture log
        ]);
        let a = interview(&mut p, &mut Verdicts::new(&[true], &[])).unwrap();
        assert!(p.answers.is_empty(), "the script covered every question");

        let text = render(&a).unwrap();
        assert!(
            text.contains(
                "[stt.by_language]\n\
                 ru = { base_url = \"http://127.0.0.1:8770/ru/v1\" }\n\
                 he = { base_url = \"http://127.0.0.1:8770/he/v1\", model = \"ivrit-large-v3\" }\n\
                 \n[language]"
            ),
            "{text}"
        );
        let dir = std::env::temp_dir().join(format!("byovox-setup-{}-lanes", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, &text).unwrap();
        let cfg = crate::config::load(&path).expect("the wizard's file validates");
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(cfg.stt.by_language.len(), 2, "en kept [stt]");
        assert_eq!(
            cfg.stt.by_language["he"].base_url,
            "http://127.0.0.1:8770/he/v1"
        );
        assert_eq!(cfg.stt.by_language["he"].model, "ivrit-large-v3");
        assert_eq!(cfg.stt.by_language["ru"].model, "", "inherits stt.model");
    }

    #[test]
    fn the_first_lane_question_says_what_a_lane_gets_and_that_it_is_trusted() {
        let mut p = Script::new(&[
            "1",
            "http://127.0.0.1:8770/v1",
            "",
            "n",
            "",
            "en, he",
            "",
            "",
            "",
            "",
        ]);
        interview(&mut p, &mut Verdicts::new(&[true], &[])).unwrap();
        assert!(p.answers.is_empty(), "the script covered every question");
        let lane_qs: Vec<&String> = p
            .asked
            .iter()
            .filter(|q| q.contains("Dedicated speech-to-text endpoint"))
            .collect();
        assert_eq!(lane_qs.len(), 2, "{lane_qs:?}");
        let first = lane_qs[0];
        for said in [
            "It gets the dictations whisper detects\n  as that language",
            "mapped to it when detection is unsure",
            "one more server you trust",
            "token unless it names its own api_key_env or sets inherit_token = true",
            "sets inherit_token = true for a lane on [stt]'s scheme, host and port",
            "when [stt] names an\n  api_key_env",
            "cannot be marked hosted",
        ] {
            assert!(first.contains(said), "missing {said:?}: {first}");
        }
        assert!(!first.contains("token when one is set"), "{first}");
        assert!(
            first.find("one more server").unwrap() < first.find("Dedicated").unwrap(),
            "the notice comes before the question: {first}"
        );
        assert!(
            !lane_qs[1].contains("one more server you trust"),
            "{}",
            lane_qs[1]
        );
    }

    fn he_lane_line(a: &mut Answers, lane_url: &str) -> String {
        a.candidates = vec!["he".into()];
        a.polish_enabled = false;
        let mut p = Script::new(&[lane_url, ""]);
        ask_lanes(&mut p, a).unwrap();
        assert!(p.answers.is_empty(), "the script covered every question");
        let text = render(a).unwrap();
        let line = text.lines().find(|l| l.starts_with("he = "));
        line.unwrap_or_else(|| panic!("no he lane: {text}"))
            .to_string()
    }

    #[test]
    fn a_lane_on_the_stt_server_is_written_to_inherit_its_token() {
        let mut a = Answers {
            stt_base_url: "http://Stt-Host:8770/v1".into(),
            stt_api_key_env: "STT_TOKEN".into(),
            stt_api_key_file: "~/stt.env".into(),
            ..Default::default()
        };
        let line = he_lane_line(&mut a, "HTTP://stt-host:8770/he/v1");
        assert_eq!(
            line,
            "he = { base_url = \"HTTP://stt-host:8770/he/v1\", inherit_token = true }"
        );
        let dir =
            std::env::temp_dir().join(format!("byovox-setup-{}-lane-token", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, render(&a).unwrap()).unwrap();
        let cfg = crate::config::load(&path).expect("the wizard's file validates");
        std::fs::remove_dir_all(&dir).unwrap();
        let he = cfg.stt.lane_config(&cfg.stt.by_language["he"]);
        assert_eq!(
            (he.api_key_env.as_str(), he.api_key_file.as_str()),
            ("STT_TOKEN", "~/stt.env")
        );
    }

    #[test]
    fn a_lane_elsewhere_or_without_an_stt_token_is_written_without_one() {
        for (stt_token, lane_url, why) in [
            ("STT_TOKEN", "http://ivrit:8770/v1", "another host"),
            ("STT_TOKEN", "http://stt-host:8771/he/v1", "another port"),
            ("STT_TOKEN", "https://stt-host:8770/he/v1", "another scheme"),
            ("", "http://stt-host:8770/he/v1", "no [stt] token"),
        ] {
            let mut a = Answers {
                stt_base_url: "http://stt-host:8770/v1".into(),
                stt_api_key_env: stt_token.into(),
                ..Default::default()
            };
            assert_eq!(
                he_lane_line(&mut a, lane_url),
                format!("he = {{ base_url = \"{lane_url}\" }}"),
                "{why}"
            );
        }
    }

    #[test]
    fn a_language_both_detected_and_mapped_is_asked_about_once() {
        let mut p = Script::new(&[
            "1",
            "http://127.0.0.1:8770/v1",
            "",
            "n",
            "",
            "en, he",       // candidates
            "he=he, ru=ru", // by_layout
            "",             // en
            "",             // he
            "",             // ru
            "",             // capture log
        ]);
        let run = interview(&mut p, &mut Verdicts::new(&[true], &[]));
        let asked: Vec<&str> = p
            .asked
            .iter()
            .filter(|q| q.contains("Dedicated speech-to-text endpoint"))
            .map(|q| {
                q.rsplit("endpoint for `")
                    .next()
                    .unwrap()
                    .split('`')
                    .next()
                    .unwrap()
            })
            .collect();
        assert_eq!(
            asked,
            ["en", "he", "ru"],
            "each language once, candidates first"
        );
        run.unwrap();
        assert!(p.answers.is_empty(), "the script covered every question");
    }

    #[test]
    fn a_malformed_lane_url_is_asked_again() {
        let mut p = Script::new(&[
            "1",
            "http://127.0.0.1:8770/v1",
            "",
            "n",
            "",
            "he",                   // candidates
            "",                     // by_layout
            "127.0.0.1:8770/he/v1", // no scheme
            "http://127.0.0.1:8770/he/v1",
            "", // model: inherit
            "", // capture log
        ]);
        let a = interview(&mut p, &mut Verdicts::new(&[true], &[])).unwrap();
        let he_qs = p
            .asked
            .iter()
            .filter(|q| q.contains("endpoint for `he`"))
            .count();
        assert_eq!(he_qs, 2, "asked again after the bad URL");
        assert!(p.answers.is_empty(), "the script covered every question");
        assert_eq!(a.lanes[0].1.base_url, "http://127.0.0.1:8770/he/v1");
        assert_eq!(
            parse_lane_url("ftp://h/v1"),
            parse_base_url("ftp://h/v1").map(Some)
        );
        assert_eq!(parse_lane_url("  "), Ok(None));
    }

    #[test]
    fn a_probe_and_the_written_file_agree_on_every_key_answered_by_then() {
        let mut probes = Verdicts::new(&[true], &[true]);
        let answers = interview(&mut Script::new(HAPPY), &mut probes).unwrap();
        let written: Config = toml::from_str(&render(&answers).unwrap()).unwrap();

        let probed = &probes.stt_seen[0];
        assert_eq!(probed.stt.base_url, written.stt.base_url);
        assert_eq!(probed.stt.model, written.stt.model);
        assert_eq!(probed.stt.timeout_s, written.stt.timeout_s);
        assert_eq!(
            probed.stt.no_speech_threshold,
            written.stt.no_speech_threshold
        );
        assert_eq!(probed.language.default, written.language.default);

        assert!(
            probed.language.candidates.is_empty(),
            "not answered when the STT probe ran"
        );
        assert_eq!(written.language.candidates, ["en", "ru"]);

        let probed = &probes.polish_seen[0];
        assert_eq!(probed.polish.base_url, written.polish.base_url);
        assert_eq!(probed.polish.model, written.polish.model);
        assert_eq!(probed.polish.min_words, written.polish.min_words);
        assert_eq!(probed.polish.timeout_s, written.polish.timeout_s);
        assert_eq!(probed.polish.prompt_file, written.polish.prompt_file);
    }

    #[test]
    fn declining_polish_asks_no_gateway_questions() {
        let mut p = Script::new(&["1", "http://127.0.0.1:8770/v1", "", "n", "", "", "", ""]);
        let mut probes = Verdicts::new(&[true], &[]);
        let a = interview(&mut p, &mut probes).unwrap();
        assert!(!a.polish_enabled);
        assert!(probes.polish_seen.is_empty(), "no gateway was probed");
        assert!(
            !p.asked.iter().any(|q| q.contains("Chat-completions")),
            "{:?}",
            p.asked
        );
        let cfg: Config = toml::from_str(&render(&a).unwrap()).unwrap();
        assert!(!cfg.polish.enabled);
        assert_eq!(cfg.polish.base_url, "");
    }

    #[test]
    fn a_failed_probe_can_be_retried_kept_or_aborted() {
        let mut p = Script::new(&[
            "1",
            "http://wrong:8770/v1",
            "",
            "r", // retry — the whole group is asked again, the pick included
            "1",
            "http://right:8770/v1",
            "",
            "n", // polish off
            "",
            "",
            "",
            "",
        ]);
        let mut probes = Verdicts::new(&[false, true], &[]);
        let a = interview(&mut p, &mut probes).unwrap();
        assert_eq!(a.stt_base_url, "http://right:8770/v1");
        assert_eq!(probes.stt_seen.len(), 2);

        let mut p = Script::new(&["1", "http://later:8770/v1", "", "k", "n", "", "", "", ""]);
        let mut probes = Verdicts::new(&[false], &[]);
        let a = interview(&mut p, &mut probes).unwrap();
        assert_eq!(a.stt_base_url, "http://later:8770/v1");

        let mut p = Script::new(&["1", "http://nope:8770/v1", "", "a"]);
        let mut probes = Verdicts::new(&[false], &[]);
        let e = interview(&mut p, &mut probes).unwrap_err();
        assert_eq!(e.to_string(), ABORTED);
    }

    #[test]
    fn an_answer_that_does_not_parse_re_asks_the_same_question() {
        let mut p = Script::new(&[
            "1",
            "not-a-url",
            "http://127.0.0.1:8770/v1",
            "",
            "n",
            "Nope", // not a key name
            "Z",    // a bare letter is not one either
            "Insert",
            "",
            "",
            "",
        ]);
        let mut probes = Verdicts::new(&[true], &[]);
        let a = interview(&mut p, &mut probes).unwrap();
        assert_eq!(a.stt_base_url, "http://127.0.0.1:8770/v1");
        assert_eq!(a.hotkey_key, "Insert");
        let url_qs = p
            .asked
            .iter()
            .filter(|q| q.contains("OpenAI-compatible"))
            .count();
        assert_eq!(url_qs, 2, "asked again after the bad URL");
        let key_qs = p
            .asked
            .iter()
            .filter(|q| q.contains("Push-to-talk"))
            .count();
        assert_eq!(key_qs, 3, "asked again after each bad key name");
        let mut p = Script::new(&[
            "1",
            "http://127.0.0.1:8770/v1",
            "",
            "n",
            "ControlLeft+ShiftLeft+Z",
            "",
            "",
            "",
        ]);
        let a = interview(&mut p, &mut Verdicts::new(&[true], &[])).unwrap();
        assert_eq!(a.hotkey_key, "ControlLeft+ShiftLeft+Z");
    }

    #[test]
    fn an_unset_token_variable_offers_the_key_file() {
        let mut p = Script::new(&[
            "1",
            "http://127.0.0.1:8770/v1",
            "BYOVOX_SETUP_NO_SUCH_TOKEN",
            "", // key file: take the default
            "n",
            "",
            "",
            "",
            "",
        ]);
        let a = interview(&mut p, &mut Verdicts::new(&[true], &[])).unwrap();
        assert_eq!(a.stt_api_key_env, "BYOVOX_SETUP_NO_SUCH_TOKEN");
        assert_eq!(a.stt_api_key_file, DEFAULT_KEY_FILE);
        assert!(
            p.asked.iter().any(|q| q.contains("key file")),
            "{:?}",
            p.asked
        );

        // SAFETY: no other test touches this name; the environment API is synchronised on Windows.
        unsafe { std::env::set_var("BYOVOX_SETUP_TEST_TOKEN", "not-a-real-token") };
        let mut p = Script::new(&[
            "1",
            "http://127.0.0.1:8770/v1",
            "BYOVOX_SETUP_TEST_TOKEN",
            "n",
            "",
            "",
            "",
            "",
        ]);
        let a = interview(&mut p, &mut Verdicts::new(&[true], &[])).unwrap();
        // SAFETY: as above.
        unsafe { std::env::remove_var("BYOVOX_SETUP_TEST_TOKEN") };
        assert_eq!(a.stt_api_key_env, "BYOVOX_SETUP_TEST_TOKEN");
        assert_eq!(a.stt_api_key_file, "", "no file question was asked");
        assert!(
            !p.asked.iter().any(|q| q.contains("key file")),
            "{:?}",
            p.asked
        );
    }

    #[test]
    fn a_closed_stdin_aborts_and_names_the_command_that_needs_no_terminal() {
        let e = interview(&mut Script::new(&[]), &mut Verdicts::new(&[], &[])).unwrap_err();
        assert_eq!(e.to_string(), NO_TERMINAL);
        assert!(e.to_string().contains("byovox config --init"));
        let e = interview(
            &mut Script::new(&["http://127.0.0.1:8770/v1"]),
            &mut Verdicts::new(&[true], &[]),
        )
        .unwrap_err();
        assert_eq!(e.to_string(), NO_TERMINAL);
    }

    #[test]
    fn an_existing_config_is_not_overwritten_without_a_yes() {
        let dir =
            std::env::temp_dir().join(format!("byovox-setup-{}-overwrite", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");

        let mut p = Script::new(&[]);
        assert!(confirm_overwrite(&mut p, &path).is_ok());
        assert!(p.asked.is_empty());

        std::fs::write(&path, "[stt]\n").unwrap();
        let mut p = Script::new(&[""]);
        let e = confirm_overwrite(&mut p, &path).unwrap_err();
        assert!(e.to_string().contains("left"), "{e}");
        assert!(
            p.asked[0].contains(&path.display().to_string()),
            "{:?}",
            p.asked
        );
        assert!(confirm_overwrite(&mut Script::new(&["n"]), &path).is_err());
        assert!(
            confirm_overwrite(&mut Script::new(&["y"]), &path).unwrap(),
            "a yes is carried to the write, so it is not asked again"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_config_that_appears_during_the_interview_is_not_replaced_without_a_yes() {
        let dir = std::env::temp_dir().join(format!("byovox-setup-{}-race", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let theirs = "[stt]\nbase_url = \"http://theirs:8770/v1\"\n";

        std::fs::write(&path, theirs).unwrap();
        let mut p = Script::new(&[""]); // Enter = no
        let e = write_config(&mut p, &path, "mine = true\n", false).unwrap_err();
        assert!(e.to_string().contains("were not written"), "{e}");
        assert!(
            p.asked[0].contains(&path.display().to_string()),
            "the question names the file: {:?}",
            p.asked
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            theirs,
            "byte for byte what the other writer put there"
        );

        write_config(&mut Script::new(&["y"]), &path, "mine = true\n", false).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "mine = true\n");

        let mut p = Script::new(&[]);
        write_config(&mut p, &path, "again = true\n", true).unwrap();
        assert!(p.asked.is_empty(), "{:?}", p.asked);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "again = true\n");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_fresh_path_is_written_with_its_parent_directory_and_no_question() {
        let dir = std::env::temp_dir().join(format!("byovox-setup-{}-fresh", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("byovox").join("config.toml");

        let mut p = Script::new(&[]);
        write_config(&mut p, &path, "fresh = true\n", false).unwrap();
        assert!(p.asked.is_empty(), "{:?}", p.asked);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "fresh = true\n");

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
