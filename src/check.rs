//! `byovox check`: exercise every stage and say which rung each backend is on.
//! Layout and inject are reported, never failed; polish and the local server are required only when configured.

use std::path::Path;
use std::time::{Duration, Instant};

use crate::audio::{Audio, SAMPLE_RATE};
use crate::capture::{Capture, CpalCapture, DeviceInfo, describe_device, input_names};
use crate::config::{
    CLEARTEXT_WARNING, Config, HotkeyConfig, PolishConfig, SttConfig, SttLane, SttServer,
    UpdateConfig, expand_home, host_of, is_cleartext_remote, redact_userinfo, resolve_token,
};
use crate::hotkey::{HotkeyMode, parse_chord, validate_key_name};
use crate::lang::{Lang, LanguagePolicy, SttLanguage};
use crate::pipeline::no_speech;
use crate::polish::{self, PolishClient, Polisher};
use crate::stt::{SttClient, Transcriber};

const SAMPLE: Duration = Duration::from_secs(1);

const MIN_USABLE: Duration = Duration::from_millis(500);

const PREFIX_CHARS: usize = 60;

fn line(stage: &str, ok: Option<bool>, detail: &str) {
    let mark = match ok {
        Some(true) => "ok  ",
        Some(false) => "FAIL",
        None => "    ",
    };
    println!("{mark} {stage:<9} {detail}");
}

/// Configured and working, but worth saying; not a FAIL, so the exit code keeps meaning "byovox can dictate".
fn warn_line(stage: &str, detail: &str) {
    println!("warn {stage:<9} {detail}");
}

fn note_line(stage: &str, detail: &str) {
    println!("note {stage:<9} {detail}");
}

/// Polish is skipped when disabled: the daemon never calls it, so warning about its URL is noise.
fn cleartext_endpoints(cfg: &Config) -> Vec<(String, &str)> {
    let mut out = Vec::new();
    if is_cleartext_remote(&cfg.stt.base_url) {
        out.push(("stt.base_url".to_string(), cfg.stt.base_url.as_str()));
    }
    for (code, lane) in &cfg.stt.by_language {
        if is_cleartext_remote(&lane.base_url) {
            out.push((
                format!("stt.by_language.{code}.base_url"),
                lane.base_url.as_str(),
            ));
        }
    }
    if cfg.polish.enabled && is_cleartext_remote(&cfg.polish.base_url) {
        out.push(("polish.base_url".to_string(), cfg.polish.base_url.as_str()));
    }
    out
}

/// Control characters become spaces so a newline cannot wrap the row or overwrite the `ok`/`FAIL` mark.
fn prefix(s: &str) -> String {
    s.chars()
        .take(PREFIX_CHARS)
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// Cuts the response body (up to 200 chars, possibly transcript text) after `HTTP <status>: `, keeping the cause.
fn strip_body(e: &str) -> &str {
    const MARK: &str = " HTTP ";
    let Some(at) = e.find(MARK) else { return e };
    let after = &e[at + MARK.len()..];
    let status = after.len() - after.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    if status == 0 || !after[status..].starts_with(": ") {
        return e;
    }
    &e[..at + MARK.len() + status]
}

/// Validated here because `platform::detect` rejects key names only a row later, after an `ok`.
fn hotkey_error(h: &HotkeyConfig) -> Option<String> {
    if let Err(e) = parse_chord(&h.key) {
        return Some(format!("hotkey.key: {e}"));
    }
    if let Err(e) = validate_key_name(&h.cancel_key) {
        return Some(format!("hotkey.cancel_key: {e}"));
    }
    if HotkeyMode::parse(&h.mode).is_none() {
        return Some(format!("hotkey.mode `{}`: expected hold | toggle", h.mode));
    }
    None
}

fn hotkey_row(h: &HotkeyConfig) -> String {
    format!("{} {}, cancel {}", h.key, h.mode, h.cancel_key)
}

/// `Err` when too little audio arrived to judge.
fn enough_audio(a: Audio) -> Result<Audio, String> {
    let min = (SAMPLE_RATE as f64 * MIN_USABLE.as_secs_f64()) as usize;
    if a.samples.len() < min {
        return Err(format!(
            "mic delivered {} usable samples in {:.0} s",
            a.samples.len(),
            SAMPLE.as_secs_f32()
        ));
    }
    Ok(a)
}

/// An `api_key_env` resolving to nothing is a misconfiguration: an auth-free endpoint would answer and print `ok`.
fn stage_token(env_name: &str, file: &str) -> Result<Option<String>, String> {
    let key = resolve_token(env_name, file);
    if key.is_none() && !env_name.is_empty() {
        let from_file = if file.is_empty() {
            ""
        } else {
            " and not found in api_key_file"
        };
        return Err(format!("token: env var {env_name} unset{from_file}"));
    }
    Ok(key)
}

const LANE_401_HINT: &str = concat!(
    ": [stt] has a token and this lane sends none; ",
    "set inherit_token = true or the lane's own api_key_env"
);

/// Appends `LANE_401_HINT` when a lane 401s with no token of its own while `[stt]`'s resolves.
fn lane_error(e: String, lane: &SttLane, stt_has_token: bool) -> String {
    if e == "stt HTTP 401" && lane.api_key_env.is_empty() && !lane.inherit_token && stt_has_token {
        return e + LANE_401_HINT;
    }
    e
}

fn lane_row(
    section: &SttConfig,
    lane: &SttLane,
    audio: Option<&Audio>,
    lang: Option<SttLanguage>,
) -> Option<Result<String, String>> {
    let lane_cfg = section.lane_config(lane);
    match (
        stage_token(&lane_cfg.api_key_env, &lane_cfg.api_key_file),
        audio,
        lang,
    ) {
        (Err(e), _, _) => Some(Err(e)),
        (Ok(key), Some(a), Some(l)) => Some(stt_round_trip(&lane_cfg, key, a, &l).map_err(|e| {
            let stt_has_token =
                resolve_token(&section.api_key_env, &section.api_key_file).is_some();
            lane_error(e, lane, stt_has_token)
        })),
        _ => None,
    }
}

/// Reading `polish.prompt_file` here makes a path typo fail the check, not the first dictation.
fn prompt_text(prompt_file: &str, capitalize_first_word: bool) -> Result<String, String> {
    if prompt_file.is_empty() {
        return Ok(crate::polish::built_in(capitalize_first_word));
    }
    let path = expand_home(prompt_file);
    std::fs::read_to_string(&path)
        .map_err(|e| format!("polish.prompt_file {}: {e}", path.display()))
}

/// `byovox setup` does not call it: it ends by running `check`, which would repeat them.
pub fn warn_cleartext(cfg: &Config) {
    for (key, url) in cleartext_endpoints(cfg) {
        // Redacted: these rows get pasted into bug reports and a `base_url` can carry `user:pass@`.
        let url = redact_userinfo(url);
        warn_line("network", &format!("{key} {url} is {CLEARTEXT_WARNING}"));
    }
}

/// Driven by the `hosted` key, never the URL, which cannot tell a hosted API from one you run.
fn hosted_endpoints(cfg: &Config) -> Vec<(&'static str, &str, &'static str)> {
    let mut out = Vec::new();
    if cfg.stt.hosted {
        out.push(("stt.base_url", cfg.stt.base_url.as_str(), "your audio"));
    }
    if cfg.polish.enabled && cfg.polish.hosted {
        out.push((
            "polish.base_url",
            cfg.polish.base_url.as_str(),
            "every dictation's text",
        ));
    }
    out
}

/// A `note`, not a `warn`: it is a choice byovox allows.
pub fn note_hosted(cfg: &Config) {
    for (key, url, what) in hosted_endpoints(cfg) {
        note_line(
            "hosted",
            &format!("{key} {} — {what} is sent to it", host_of(url)),
        );
    }
}

/// Read off the config alone; `check` makes no request of its own.
fn update_row(u: &UpdateConfig) -> &'static str {
    if u.check {
        "on: one anonymous version request per day to api.github.com (update.check = false turns it off)"
    } else {
        "off: no version request is made (update.check = false)"
    }
}

fn server_row(s: &SttServer) -> Option<Result<String, String>> {
    if s.command.is_empty() {
        return None;
    }
    Some(crate::server::validate(s).map(|()| format!("{} (started by the daemon)", s.command)))
}

/// A second of digital silence: `byovox setup` has no microphone yet.
fn silence() -> Audio {
    Audio {
        samples: vec![0; SAMPLE_RATE as usize],
    }
}

/// The live keyboard layout cannot be read here, so it resolves as an unmapped layout.
pub fn probe_stt(cfg: &Config) -> Result<(), String> {
    let language = match LanguagePolicy::from_config(&cfg.language, cfg.stt.by_language.keys()) {
        Ok(policy) => policy.resolve(None),
        Err(e) => return report("stt", Err(e.to_string())),
    };
    let row = stage_token(&cfg.stt.api_key_env, &cfg.stt.api_key_file)
        .and_then(|key| stt_round_trip(&cfg.stt, key, &silence(), &language));
    report("stt", row)
}

pub fn probe_polish(cfg: &Config) -> bool {
    report("polish", polish_round_trip(&cfg.polish, &cfg.stt)).is_ok()
}

fn report(stage: &str, row: Result<String, String>) -> Result<(), String> {
    match row {
        Ok(detail) => {
            line(stage, Some(true), &detail);
            Ok(())
        }
        Err(e) => {
            line(stage, Some(false), &e);
            Err(e)
        }
    }
}

pub fn run(cfg: &Config, config_path: &Path) -> bool {
    let mut all_ok = true;

    let source = if config_path.exists() {
        config_path.display().to_string()
    } else {
        "defaults (no file yet — `byovox config --init`)".to_string()
    };
    line("config", Some(true), &source);

    warn_cleartext(cfg);
    note_hosted(cfg);
    line("update", None, update_row(&cfg.update));
    if let Some(row) = server_row(&cfg.stt.server) {
        all_ok &= report("server", row).is_ok();
    }

    let policy = match LanguagePolicy::from_config(&cfg.language, cfg.stt.by_language.keys()) {
        Ok(p) => p,
        Err(e) => {
            line("language", Some(false), &e.to_string());
            return false;
        }
    };

    // `hotkey.mode` is unvalidated until the daemon runs and `detect` rejects key names a row late, so check here.
    match hotkey_error(&cfg.hotkey) {
        None => line("hotkey", Some(true), &hotkey_row(&cfg.hotkey)),
        Some(e) => {
            line("hotkey", Some(false), &e);
            all_ok = false;
        }
    }

    let backends = match crate::platform::detect(cfg) {
        Ok(b) => b,
        Err(e) => {
            line("backends", Some(false), &e.to_string());
            return false;
        }
    };
    line(
        "backends",
        Some(true),
        &format!(
            "hotkey={} layout={} inject={}",
            backends.names.hotkey,
            backends.names.layout,
            backends.names.rungs.join(",")
        ),
    );

    // Before anything is recorded, so a `capture.device` naming no device fails as that key.
    let device = describe_device(&cfg.capture.device);
    let audio = match &device {
        Ok(info) => match sample_microphone(&cfg.capture.device, info) {
            Ok(a) => {
                line(
                    "mic",
                    Some(true),
                    &format!("{info}  {}", mic_row_detail(&a)),
                );
                Some(a)
            }
            Err(e) => {
                line("mic", Some(false), &e);
                all_ok = false;
                None
            }
        },
        Err(e) => {
            line("mic", Some(false), e);
            all_ok = false;
            None
        }
    };
    let hands_free = device
        .as_ref()
        .is_ok_and(|info| is_hands_free(&info.name, info.rate));
    if hands_free {
        warn_line("mic", HANDS_FREE_WARNING);
    }
    if audio.is_none() || hands_free || !cfg.capture.device.trim().is_empty() {
        line("inputs", None, &input_row());
    }

    let layout = backends.layout.current();
    let language = policy.resolve(layout);
    line("layout", None, &layout_row(layout, &language));

    let stt_row = match stage_token(&cfg.stt.api_key_env, &cfg.stt.api_key_file) {
        Err(e) => Some(Err(e)),
        Ok(key) => audio
            .as_ref()
            .map(|a| stt_round_trip(&cfg.stt, key, a, &language)),
    };
    match stt_row {
        Some(Ok(detail)) => line("stt", Some(true), &detail),
        Some(Err(e)) => {
            line("stt", Some(false), &e);
            all_ok = false;
        }
        None => line("stt", None, "skipped (no usable microphone capture)"),
    }
    for (code, lane) in &cfg.stt.by_language {
        let name = format!("stt[{code}]");
        let lang = crate::lang::Lang::parse(code).map(SttLanguage::Explicit);
        match lane_row(&cfg.stt, lane, audio.as_ref(), lang) {
            Some(Ok(detail)) => line(&name, Some(true), &detail),
            Some(Err(e)) => {
                line(&name, Some(false), &e);
                all_ok = false;
            }
            None => line(&name, None, "skipped (no usable microphone capture)"),
        }
    }

    if cfg.polish.enabled {
        match polish_round_trip(&cfg.polish, &cfg.stt) {
            Ok(detail) => line("polish", Some(true), &detail),
            Err(e) => {
                line("polish", Some(false), &e);
                all_ok = false;
            }
        }
    } else {
        line("polish", None, "disabled");
    }

    line(
        "inject",
        None,
        &format!(
            "dry-run: would use `{}` first",
            backends.names.rungs.first().copied().unwrap_or("none")
        ),
    );
    println!(
        "{}",
        if all_ok {
            "\nall required stages passed"
        } else {
            "\nsome stages FAILED — see above"
        }
    );
    all_ok
}

/// Warns on `a.is_silent()`, `Pipeline::finish`'s own test, so the row cannot drift from it.
fn mic_row_detail(a: &Audio) -> String {
    let warn = if a.is_silent() {
        "  ← a dictation this quiet is discarded as silent (no 10 ms window above −40 dBFS): \
         muted, or an OS audio enhancement attenuating the mic"
    } else {
        ""
    };
    format!(
        "peak {:.1} dBFS, loudest 10 ms {:.2} dBFS RMS over {:.1}s{warn}",
        a.peak_dbfs(),
        a.loudest_window_dbfs(),
        a.duration_secs()
    )
}

fn layout_row(layout: Option<Lang>, language: &SttLanguage) -> String {
    let fields = language.form_fields();
    let shown = if fields.is_empty() {
        "auto (no language field)".to_string()
    } else {
        fields
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(" ")
    };
    let prior = match language {
        SttLanguage::Auto { prior: Some(p), .. } => format!(" (prior {p})"),
        _ => String::new(),
    };
    format!(
        "{} → {shown}{prior}",
        layout
            .map(|l| l.to_string())
            .unwrap_or_else(|| "unreadable".into())
    )
}

/// The only place `check` puts a microphone live; minus the start click `CpalCapture::stop` cuts.
fn sample_microphone(selector: &str, info: &DeviceInfo) -> Result<Audio, String> {
    record(selector, SAMPLE)
        .and_then(enough_audio)
        .map_err(|e| format!("{info}: {e}"))
}

fn input_row() -> String {
    match input_names() {
        Ok(names) if names.is_empty() => "none".to_string(),
        Ok(names) => names.join(" | "),
        Err(e) => e,
    }
}

const HANDS_FREE_WARNING: &str = "this looks like a Bluetooth hands-free profile: dictating through one switches the \
     headset out of stereo for the duration — pin `capture.device` to another microphone";

fn is_hands_free(name: &str, rate: u32) -> bool {
    let name = name.to_lowercase();
    name.contains("hands-free") || name.contains("headset") || rate <= 16_000
}

fn stt_round_trip(
    cfg: &SttConfig,
    key: Option<String>,
    audio: &Audio,
    language: &SttLanguage,
) -> Result<String, String> {
    let client = SttClient::new(
        &cfg.base_url,
        &cfg.model,
        key,
        Duration::from_secs(cfg.timeout_s),
        Duration::from_secs(cfg.connect_timeout_s),
        cfg.no_speech_threshold > 0.0,
    );
    let prompt = Some(cfg.prompt.as_str()).filter(|p| !p.is_empty());
    let t = Instant::now();
    let transcript = client
        .transcribe(&audio.to_wav(), language, prompt)
        .map_err(|e| strip_body(&e).to_string())?;
    Ok(format!(
        "{:.2}s  {}\"{}\"",
        t.elapsed().as_secs_f32(),
        no_speech_row(
            transcript.no_speech_mean,
            transcript.no_speech_prob,
            cfg.no_speech_threshold,
            cfg.no_speech_warn
        ),
        prefix(&transcript.text)
    ))
}

/// Printed before the transcript. `check` records room tone, so the score is usually high and the text invented.
/// Uses `pipeline::no_speech` with the `f64` narrowed exactly as the daemon narrows it.
fn no_speech_row(mean: Option<f32>, max: Option<f32>, threshold: f64, warn: f64) -> String {
    let (Some(m), Some(x)) = (mean, max) else {
        return String::new();
    };
    let (p, gated) = match no_speech(mean, threshold as f32) {
        Some(_) => (m, " (would be dropped as silence)"),
        None => match no_speech(max, warn as f32) {
            Some(_) => (x, " (would play the warning cue)"),
            None => (m, ""),
        },
    };
    format!("p_nospeech={p:.2}{gated}  ")
}

/// The token and `prompt_file` are proven before the request, so neither fails silently behind a gateway that answers anyway.
fn polish_round_trip(cfg: &PolishConfig, stt: &SttConfig) -> Result<String, String> {
    let key = stage_token(&cfg.api_key_env, &cfg.api_key_file)?;
    let prompt = polish::prompt_for(
        &prompt_text(&cfg.prompt_file, cfg.capitalize_first_word)?,
        stt,
    );
    let client = PolishClient::new(
        &cfg.base_url,
        &cfg.model,
        key,
        Duration::from_secs(cfg.timeout_s),
        Duration::from_secs(cfg.connect_timeout_s),
        prompt,
    );
    let t = Instant::now();
    let text = client
        .polish("um so this is uh a test")
        .map_err(|e| strip_body(&e).to_string())?;
    Ok(format!(
        "{:.2}s  \"{}\"",
        t.elapsed().as_secs_f32(),
        prefix(&text)
    ))
}

/// `pub(crate)` for `setup`'s microphone level step; minus the start click `CpalCapture::stop` cuts.
pub(crate) fn record(selector: &str, dur: Duration) -> Result<Audio, String> {
    let mut cap = CpalCapture::open(selector)?;
    cap.start()?;
    std::thread::sleep(dur);
    cap.stop()
}

#[cfg(test)]
mod tests {
    use crate::polish::BUILT_IN_PROMPT;

    use super::{
        Audio, HotkeyConfig, LANE_401_HINT, PREFIX_CHARS, SAMPLE_RATE, SttConfig, SttLanguage,
        cleartext_endpoints, enough_audio, hosted_endpoints, hotkey_error, hotkey_row,
        is_hands_free, lane_error, lane_row, layout_row, mic_row_detail, no_speech_row, prefix,
        prompt_text, server_row, silence, stage_token, strip_body, update_row,
    };
    use crate::config::{LanguageConfig, SttLane};
    use crate::lang::{Lang, LanguagePolicy};
    use crate::testutil::{MockServer, auth_header};

    #[test]
    fn a_probe_that_times_out_returns_the_timeout() {
        let srv = MockServer::start_stalling();
        let mut cfg = crate::config::Config::default();
        cfg.stt.base_url = srv.url().into();
        cfg.stt.timeout_s = 3;
        cfg.stt.connect_timeout_s = 1;
        let err = super::probe_stt(&cfg).unwrap_err();
        assert!(crate::stt::is_timeout(&err), "{err}");
    }

    #[test]
    fn the_layout_row_shows_the_candidates_sent_and_the_prior() {
        let lc = LanguageConfig {
            candidates: vec!["en".into(), "ru".into()],
            by_layout: [("he".to_string(), "he".to_string())].into(),
            ..Default::default()
        };
        let policy = LanguagePolicy::from_config(&lc, &["he".to_string()]).unwrap();
        let he = Lang::parse("he");
        assert_eq!(
            layout_row(he, &policy.resolve(he)),
            "he → language_candidates=en,ru,he (prior he)"
        );
        let ru = Lang::parse("ru");
        assert_eq!(
            layout_row(ru, &policy.resolve(ru)),
            "ru → language_candidates=en,ru,he"
        );
    }

    #[test]
    fn a_bluetooth_hands_free_endpoint_is_warned_about_by_name_or_by_rate() {
        assert!(is_hands_free("Headset (Example Buds Hands-Free)", 16_000));
        assert!(is_hands_free("headset (pamu slide hands-free)", 16_000));
        assert!(is_hands_free("Headset Microphone (Some Dongle)", 48_000));
        assert!(is_hands_free("Example Buds (Bluetooth SCO)", 8_000));
        assert!(is_hands_free("Some Microphone", 16_000));

        assert!(!is_hands_free("Microphone Array (Example Audio)", 48_000));
        assert!(!is_hands_free("Microphone (USB Audio Device)", 44_100));
        assert!(!is_hands_free("Headphones (Example Buds Stereo)", 48_000));
    }

    #[test]
    fn the_stt_row_says_what_the_no_speech_gate_would_do() {
        let one = |p: f32| (Some(p), Some(p));
        let row = |(mean, max): (Option<f32>, Option<f32>), t, w| no_speech_row(mean, max, t, w);
        assert_eq!(no_speech_row(None, None, 0.3, 0.08), "");
        assert_eq!(row(one(0.04), 0.3, 0.08), "p_nospeech=0.04  ");
        assert_eq!(
            row(one(0.76), 0.3, 0.08),
            "p_nospeech=0.76 (would be dropped as silence)  "
        );
        assert_eq!(
            row(one(0.19), 0.3, 0.08),
            "p_nospeech=0.19 (would play the warning cue)  "
        );
        assert_eq!(row(one(0.99), 0.0, 0.0), "p_nospeech=0.99  ");
    }

    #[test]
    fn the_stt_row_drops_on_the_mean_and_warns_on_the_max() {
        assert_eq!(
            no_speech_row(Some(0.264), Some(0.494), 0.3, 0.08),
            "p_nospeech=0.49 (would play the warning cue)  "
        );
        assert_eq!(
            no_speech_row(Some(0.39), Some(0.74), 0.3, 0.08),
            "p_nospeech=0.39 (would be dropped as silence)  "
        );
        assert_eq!(
            no_speech_row(Some(0.02), Some(0.05), 0.3, 0.08),
            "p_nospeech=0.02  "
        );
    }

    // An f32 threshold widened to f64 sits above the f64 threshold; compare in f32 or the row promises a drop the pipeline does not perform.
    #[test]
    fn the_row_and_the_pipeline_agree_at_the_threshold_itself() {
        let threshold = 0.3_f64;
        let p = threshold as f32;
        assert!(
            f64::from(p) > threshold,
            "the widths must genuinely differ, or this test proves nothing"
        );
        assert_eq!(crate::pipeline::no_speech(Some(p), threshold as f32), None);
        assert_eq!(
            no_speech_row(Some(p), Some(p), threshold, 0.0),
            "p_nospeech=0.30  "
        );
        assert_eq!(
            no_speech_row(Some(0.08), Some(0.08), 0.3, 0.08_f32 as f64),
            "p_nospeech=0.08  "
        );
    }

    #[test]
    fn a_transcript_is_reported_only_as_a_short_prefix() {
        let long = "ы".repeat(200);
        let p = prefix(&long);
        assert_eq!(p.chars().count(), PREFIX_CHARS);
        assert!(long.starts_with(&p));
        assert_eq!(prefix("short"), "short");
    }

    #[test]
    fn a_reply_can_neither_wrap_nor_rewind_its_row() {
        let p = prefix("done\r\nnext\u{7}");
        assert_eq!(p, "done  next ");
        assert!(!p.chars().any(char::is_control));
    }

    #[test]
    fn a_failed_stage_row_never_carries_a_response_body() {
        let e = r#"stt HTTP 401: {"error":"Incorrect API key provided: sk-live-SECRET"}"#;
        assert_eq!(strip_body(e), "stt HTTP 401");
        assert!(!strip_body(e).contains("sk-live-SECRET"));
        assert_eq!(
            strip_body(r#"polish HTTP 404: {"detail":"model not found"}"#),
            "polish HTTP 404"
        );
    }

    #[test]
    fn an_error_without_a_response_body_reaches_the_row_whole() {
        for e in [
            "stt transport: io: Connection refused",
            "polish transport: io: Connection refused",
            // An io cause with no body is the whole diagnosis; `stt.rs` keeps the colon off the status so this survives.
            "stt HTTP 500 body unreadable: io: unexpected end of file",
            "stt JSON: expected value at line 1 column 1",
            "Microphone Array (48000 Hz, 2 ch, F32): microphone did not start within 5 s",
            "token: env var EXAMPLE_API_KEY unset",
        ] {
            assert_eq!(strip_body(e), e);
        }
    }

    #[test]
    fn a_start_click_cannot_mask_a_quiet_microphone() {
        let mut samples = vec![i16::MAX; 2_000];
        samples.resize(SAMPLE_RATE as usize, 100);
        let steady =
            enough_audio(Audio { samples }.without_start_click()).expect("0.87 s is enough");
        assert!(steady.is_silent(), "{}", steady.peak_dbfs());
        assert!((steady.duration_secs() - 0.87).abs() < 0.01);
    }

    #[test]
    fn the_mic_row_warns_exactly_when_the_pipeline_would_discard_the_capture() {
        let probe = Audio {
            samples: (0..SAMPLE_RATE as usize)
                .map(|i| {
                    (460.0
                        * (2.0 * std::f32::consts::PI * 200.0 * i as f32 / SAMPLE_RATE as f32)
                            .sin())
                    .round() as i16
                })
                .collect(),
        };
        assert!(probe.peak_dbfs() > -40.0, "{}", probe.peak_dbfs());
        assert!(probe.is_silent());
        assert!(
            mic_row_detail(&probe).contains('\u{2190}'),
            "{}",
            mic_row_detail(&probe)
        );

        let mut samples = vec![i16::MAX; 2_000];
        samples.resize(SAMPLE_RATE as usize, 100);
        let steady = Audio { samples }.without_start_click();
        assert!(
            mic_row_detail(&steady).contains('\u{2190}'),
            "{}",
            mic_row_detail(&steady)
        );

        let loud = Audio {
            samples: vec![1_036; SAMPLE_RATE as usize],
        };
        assert!(!loud.is_silent(), "{}", loud.loudest_window_dbfs());
        assert!(
            !mic_row_detail(&loud).contains('\u{2190}'),
            "{}",
            mic_row_detail(&loud)
        );

        // Two decimals: at one both amplitudes round to -40.0, hiding which side of the silence boundary the capture fell on.
        let amp_328 = Audio {
            samples: vec![328; SAMPLE_RATE as usize],
        };
        assert!(
            mic_row_detail(&amp_328).contains("-39.99"),
            "{}",
            mic_row_detail(&amp_328)
        );
        let amp_327 = Audio {
            samples: vec![327; SAMPLE_RATE as usize],
        };
        assert!(
            mic_row_detail(&amp_327).contains("-40.02"),
            "{}",
            mic_row_detail(&amp_327)
        );
    }

    #[test]
    fn a_capture_too_short_to_judge_fails_instead_of_reading_as_silence() {
        let empty = Audio { samples: vec![] };
        assert!(empty.peak_dbfs() <= -120.0);
        assert_eq!(
            enough_audio(empty).unwrap_err(),
            "mic delivered 0 usable samples in 1 s"
        );
        assert!(
            enough_audio(Audio {
                samples: vec![0; 4_800]
            })
            .is_err()
        );
        assert!(
            enough_audio(Audio {
                samples: vec![0; 8_000]
            })
            .is_ok()
        );
    }

    #[test]
    fn a_named_but_unresolvable_token_fails_instead_of_going_out_anonymous() {
        assert_eq!(
            stage_token("BYOVOX_CHECK_NO_SUCH_TOKEN", "").unwrap_err(),
            "token: env var BYOVOX_CHECK_NO_SUCH_TOKEN unset"
        );
        let with_file = stage_token("BYOVOX_CHECK_NO_SUCH_TOKEN", "no-such-dir/env").unwrap_err();
        assert!(
            with_file.ends_with("unset and not found in api_key_file"),
            "{with_file}"
        );
        assert_eq!(stage_token("", "").unwrap(), None);
    }
    #[test]
    fn a_lane_401_for_want_of_the_stt_token_names_the_fix() {
        let e401 = "stt HTTP 401";
        assert_eq!(
            lane_error(e401.into(), &SttLane::default(), true),
            format!("{e401}{LANE_401_HINT}")
        );
        assert!(
            LANE_401_HINT.ends_with(
                ": [stt] has a token and this lane sends none; set inherit_token = true or the \
                 lane's own api_key_env"
            ),
            "{LANE_401_HINT}"
        );
        let own = SttLane {
            api_key_env: "HE_TOKEN".into(),
            ..Default::default()
        };
        let inherits = SttLane {
            inherit_token: true,
            ..Default::default()
        };
        for (e, lane, stt_has_token, why) in [
            (e401, &own, true, "its own token"),
            (e401, &inherits, true, "inherit_token on"),
            (e401, &SttLane::default(), false, "no [stt] token"),
            ("stt HTTP 403", &SttLane::default(), true, "not a 401"),
        ] {
            assert_eq!(lane_error(e.into(), lane, stt_has_token), e, "{why}");
        }
    }

    #[test]
    fn a_lane_row_sends_the_token_its_lane_asks_for() {
        let dir = std::env::temp_dir().join(format!("byovox-check-{}-lane", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let keys = dir.join("keys.env");
        std::fs::write(
            &keys,
            "BYOVOX_CHECK_TEST_STT=stt-tok\nBYOVOX_CHECK_TEST_HE_FILE=he-file-tok\n",
        )
        .unwrap();
        let keys = keys.to_str().unwrap();
        // SAFETY: no other test touches this variable; the environment API is internally synchronised on Windows.
        unsafe { std::env::set_var("BYOVOX_CHECK_TEST_HE_ENV", "he-env-tok") };
        let section = SttConfig {
            api_key_env: "BYOVOX_CHECK_TEST_STT".into(),
            api_key_file: keys.into(),
            ..Default::default()
        };
        let row = |status: u16, lane: SttLane| {
            let srv = MockServer::start(status, r#"{"text":"x"}"#);
            let lane = SttLane {
                base_url: srv.url().into(),
                ..lane
            };
            let he = SttLanguage::Explicit(Lang::parse("he").unwrap());
            let row = lane_row(&section, &lane, Some(&silence()), Some(he)).unwrap();
            let sent = srv.requests().first().and_then(|r| auth_header(r));
            (row, sent)
        };
        for (lane, sent) in [
            (SttLane::default(), None),
            (
                SttLane {
                    inherit_token: true,
                    ..Default::default()
                },
                Some("Bearer stt-tok"),
            ),
            (
                SttLane {
                    api_key_env: "BYOVOX_CHECK_TEST_HE_ENV".into(),
                    ..Default::default()
                },
                Some("Bearer he-env-tok"),
            ),
            (
                SttLane {
                    api_key_env: "BYOVOX_CHECK_TEST_HE_FILE".into(),
                    api_key_file: keys.into(),
                    ..Default::default()
                },
                Some("Bearer he-file-tok"),
            ),
        ] {
            let why = format!("{lane:?}");
            let (result, got) = row(200, lane);
            assert!(result.is_ok(), "{why}: {result:?}");
            assert_eq!(got.as_deref(), sent, "{why}");
        }

        let (result, _) = row(401, SttLane::default());
        assert_eq!(
            result.unwrap_err(),
            format!("stt HTTP 401{LANE_401_HINT}"),
            "a bare lane's 401"
        );
        let (result, got) = row(
            200,
            SttLane {
                api_key_env: "BYOVOX_CHECK_TEST_NO_SUCH_TOKEN".into(),
                ..Default::default()
            },
        );
        assert_eq!(
            result.unwrap_err(),
            "token: env var BYOVOX_CHECK_TEST_NO_SUCH_TOKEN unset"
        );
        assert_eq!(got, None, "no request was sent");
        // SAFETY: as above.
        unsafe { std::env::remove_var("BYOVOX_CHECK_TEST_HE_ENV") };
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_configured_prompt_is_what_check_reads_and_sends() {
        assert_eq!(prompt_text("", true).unwrap(), BUILT_IN_PROMPT);
        let lowered = prompt_text("", false).unwrap();
        assert_ne!(lowered, BUILT_IN_PROMPT);
        assert!(lowered.contains(crate::polish::LOWERCASE_FIRST_RULE));
        let manifest = concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml");
        let text = prompt_text(manifest, true).expect("a readable file");
        assert_eq!(
            prompt_text(manifest, false).unwrap(),
            text,
            "a prompt_file owns its own rule 1 and is never rewritten"
        );
        assert!(
            text.contains("byovox"),
            "the file's own text, not the built-in"
        );
        assert_ne!(text, BUILT_IN_PROMPT);
        let e = prompt_text("no-such-dir/no-such-prompt.txt", true).unwrap_err();
        let (path, io) = e
            .strip_prefix("polish.prompt_file ")
            .and_then(|rest| rest.split_once(": "))
            .expect("`polish.prompt_file <path>: <error>`");
        assert_eq!(path, "no-such-dir/no-such-prompt.txt");
        assert!(!io.is_empty());
    }

    #[test]
    fn every_cleartext_endpoint_that_will_be_used_gets_a_row() {
        use crate::config::{Config, PolishConfig, SttConfig};

        let remote = |url: &str| Config {
            stt: SttConfig {
                base_url: url.into(),
                ..Default::default()
            },
            polish: PolishConfig {
                base_url: url.into(),
                ..Default::default()
            },
            ..Default::default()
        };

        let both = remote("http://10.0.0.5:8770/v1");
        assert_eq!(
            cleartext_endpoints(&both)
                .iter()
                .map(|(k, _)| k.as_str())
                .collect::<Vec<_>>(),
            ["stt.base_url", "polish.base_url"]
        );

        let mut stt_only = remote("http://10.0.0.5:8770/v1");
        stt_only.polish.enabled = false;
        assert_eq!(
            cleartext_endpoints(&stt_only)
                .iter()
                .map(|(k, _)| k.as_str())
                .collect::<Vec<_>>(),
            ["stt.base_url"]
        );

        let mut with_lane = stt_only.clone();
        with_lane.stt.by_language.insert(
            "he".into(),
            crate::config::SttLane {
                base_url: "http://10.0.0.5:8770/he/v1".into(),
                ..Default::default()
            },
        );
        assert_eq!(
            cleartext_endpoints(&with_lane)
                .iter()
                .map(|(k, _)| k.as_str())
                .collect::<Vec<_>>(),
            ["stt.base_url", "stt.by_language.he.base_url"]
        );

        assert!(cleartext_endpoints(&remote("http://127.0.0.1:8770/v1")).is_empty());
        assert!(cleartext_endpoints(&remote("https://api.example.com/v1")).is_empty());
    }

    #[test]
    fn a_hosted_stage_gets_a_note_row_from_its_key_not_its_url() {
        use crate::config::{Config, PolishConfig, SttConfig};

        let cfg = |stt_url: &str, stt_hosted: bool, polish_url: &str, polish_hosted: bool| Config {
            stt: SttConfig {
                base_url: stt_url.into(),
                hosted: stt_hosted,
                ..Default::default()
            },
            polish: PolishConfig {
                base_url: polish_url.into(),
                hosted: polish_hosted,
                ..Default::default()
            },
            ..Default::default()
        };

        let one = cfg(
            "http://127.0.0.1:8770/v1",
            false,
            "https://api.example.com/v1",
            true,
        );
        assert_eq!(
            hosted_endpoints(&one)
                .iter()
                .map(|(k, _, _)| *k)
                .collect::<Vec<_>>(),
            ["polish.base_url"]
        );
        assert_eq!(hosted_endpoints(&one)[0].1, "https://api.example.com/v1");
        assert_eq!(hosted_endpoints(&one)[0].2, "every dictation's text");

        let speech = cfg(
            "https://api.example.com/v1",
            true,
            "http://127.0.0.1:4000/v1",
            false,
        );
        assert_eq!(hosted_endpoints(&speech).len(), 1);
        assert_eq!(hosted_endpoints(&speech)[0].2, "your audio");

        let own = cfg(
            "https://whisper.tail1234.ts.net/v1",
            false,
            "https://llm.tail1234.ts.net/v1",
            false,
        );
        assert!(hosted_endpoints(&own).is_empty());
        assert!(cleartext_endpoints(&own).is_empty());

        let both = cfg(
            "http://10.0.0.5:8770/v1",
            true,
            "http://127.0.0.1:4000/v1",
            false,
        );
        assert_eq!(hosted_endpoints(&both).len(), 1);
        assert_eq!(cleartext_endpoints(&both).len(), 1);

        let mut off = cfg(
            "http://127.0.0.1:8770/v1",
            false,
            "https://api.example.com/v1",
            true,
        );
        off.polish.enabled = false;
        assert!(hosted_endpoints(&off).is_empty());
    }

    #[test]
    fn the_update_row_says_whether_the_daily_request_is_made() {
        use crate::config::{Config, UpdateConfig};

        let on = update_row(&UpdateConfig { check: true });
        assert!(on.starts_with("on: "), "{on}");
        assert!(on.contains("api.github.com"), "{on}");
        let off = update_row(&UpdateConfig { check: false });
        assert!(off.starts_with("off: "), "{off}");
        assert_eq!(update_row(&Config::default().update), on);
    }

    #[test]
    fn the_server_row_names_the_command_and_fails_on_a_missing_file() {
        use crate::config::SttServer;

        assert_eq!(server_row(&SttServer::default()), None);

        let exe = std::env::current_exe().unwrap();
        let exe = exe.to_str().unwrap();
        let ok = server_row(&SttServer {
            command: exe.into(),
            args: Vec::new(),
        });
        assert_eq!(ok, Some(Ok(format!("{exe} (started by the daemon)"))));

        let missing = std::env::temp_dir().join("byovox-no-such-server.exe");
        let missing = missing.to_str().unwrap();
        let fail = server_row(&SttServer {
            command: missing.into(),
            args: Vec::new(),
        })
        .unwrap()
        .unwrap_err();
        assert!(fail.contains(missing), "{fail}");
    }

    #[test]
    fn a_key_name_the_daemon_would_reject_fails_the_hotkey_row() {
        assert_eq!(hotkey_error(&HotkeyConfig::default()), None);
        let bad_key = HotkeyConfig {
            key: "Nope".into(),
            ..Default::default()
        };
        assert!(
            hotkey_error(&bad_key)
                .expect("bad key")
                .starts_with("hotkey.key: unknown key `Nope`")
        );
        let bad_cancel = HotkeyConfig {
            cancel_key: "Nope".into(),
            ..Default::default()
        };
        assert!(
            hotkey_error(&bad_cancel)
                .expect("bad cancel key")
                .starts_with("hotkey.cancel_key: unknown key `Nope`")
        );
        let bad_mode = HotkeyConfig {
            mode: "sometimes".into(),
            ..Default::default()
        };
        assert_eq!(
            hotkey_error(&bad_mode).expect("bad mode"),
            "hotkey.mode `sometimes`: expected hold | toggle"
        );
    }

    #[test]
    fn the_hotkey_row_takes_a_chord_and_still_refuses_a_bare_letter() {
        let chord = HotkeyConfig {
            key: "ControlLeft+ShiftLeft+Z".into(),
            ..Default::default()
        };
        assert_eq!(hotkey_error(&chord), None);
        assert_eq!(
            hotkey_row(&chord),
            "ControlLeft+ShiftLeft+Z hold, cancel Escape"
        );
        assert_eq!(
            hotkey_row(&HotkeyConfig::default()),
            "ControlRight hold, cancel Escape"
        );

        let bare = HotkeyConfig {
            key: "Z".into(),
            ..Default::default()
        };
        assert!(
            hotkey_error(&bare)
                .expect("a bare letter")
                .starts_with("hotkey.key: `Z` needs a modifier")
        );
    }
}
