//! Explicit-selection speech. Text/audio stay in memory; failures never include server bodies or selected text.
use std::collections::BTreeMap;
use std::io::{Cursor, Read};
use std::time::Duration;

use serde::{Deserialize, Serialize};

pub const LANGUAGES: [&str; 3] = ["en", "he", "ru"];
pub const MAX_CHUNK: usize = 500;
pub const MAX_TEXT: usize = 20_000;
pub const MAX_AUDIO: usize = 12 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TtsConfig {
    pub enabled: bool,
    pub hotkey: String,
    pub base_url: String,
    pub model: String,
    pub voice: String,
    pub speed: f32,
    pub timeout_s: u64,
    pub api_key_env: String,
    pub api_key_file: String,
    pub by_language: BTreeMap<String, TtsLane>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TtsLane {
    pub base_url: String,
    pub model: String,
    pub voice: String,
    pub api_key_env: String,
    pub api_key_file: String,
    pub inherit_token: bool,
}

impl Default for TtsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            hotkey: "ControlLeft+AltLeft+R".into(),
            base_url: String::new(),
            model: "kokoro".into(),
            voice: "af_bella".into(),
            speed: 1.0,
            timeout_s: 30,
            api_key_env: String::new(),
            api_key_file: String::new(),
            by_language: ["he", "ru"]
                .into_iter()
                .map(|l| (l.into(), TtsLane::default()))
                .collect(),
        }
    }
}

/// Deliberately no Debug: this contains a resolved bearer token.
pub struct Endpoint {
    pub base_url: String,
    pub model: String,
    pub voice: String,
    token: Option<String>,
}

impl TtsConfig {
    pub fn effective_hotkey(&self) -> &str {
        if self.hotkey.trim().is_empty() {
            "ControlLeft+AltLeft+R"
        } else {
            &self.hotkey
        }
    }
    pub fn validate_hotkeys(&self, dictation: &str, cancel: &str) -> Result<(), String> {
        if !self.enabled {
            return Ok(());
        }
        let read = crate::hotkey::parse_chord(self.effective_hotkey())
            .map_err(|_| "tts.hotkey: invalid chord")?;
        let dictate =
            crate::hotkey::parse_chord(dictation).map_err(|_| "hotkey.key: invalid chord")?;
        if matches!(
            read.trigger.as_str(),
            "ControlLeft"
                | "ControlRight"
                | "AltLeft"
                | "AltRight"
                | "ShiftLeft"
                | "ShiftRight"
                | "MetaLeft"
                | "MetaRight"
        ) {
            return Err("tts.hotkey: use a non-modifier trigger key".into());
        }
        if read.modifiers.iter().any(|name| {
            !matches!(
                name.as_str(),
                "ControlLeft"
                    | "ControlRight"
                    | "AltLeft"
                    | "AltRight"
                    | "ShiftLeft"
                    | "ShiftRight"
                    | "MetaLeft"
                    | "MetaRight"
            )
        }) {
            return Err("tts.hotkey: use Ctrl/Alt/Shift/Win modifiers with a trigger key".into());
        }
        if read.trigger == cancel
            || read.modifiers.iter().any(|m| m == cancel)
            || read.trigger == dictate.trigger
            || read.modifiers.contains(&dictate.trigger)
            || dictate.modifiers.contains(&read.trigger)
        {
            return Err("tts.hotkey must not overlap the dictation trigger or cancel key".into());
        }
        Ok(())
    }
    pub fn validate(&self) -> Result<(), String> {
        if !self.speed.is_finite() || !(0.25..=4.0).contains(&self.speed) {
            return Err("tts.speed: expected 0.25–4.0".into());
        }
        if !(1..=120).contains(&self.timeout_s) {
            return Err("tts.timeout_s: expected 1–120 seconds".into());
        }
        if self.enabled && !self.hotkey.trim().is_empty() {
            crate::hotkey::parse_chord(self.effective_hotkey())
                .map_err(|_| "tts.hotkey: invalid chord")?;
        }
        if self.enabled {
            self.endpoint("en")?;
        }
        for (code, lane) in &self.by_language {
            if !LANGUAGES.contains(&code.as_str()) {
                return Err("tts.by_language: expected en, he or ru".into());
            }
            if lane.inherit_token && (!lane.api_key_env.is_empty() || !lane.api_key_file.is_empty())
            {
                return Err(format!(
                    "tts.by_language.{code}: use an own token or inherit_token, not both"
                ));
            }
            if !lane.voice.trim().is_empty() {
                self.endpoint(code)?;
            }
        }
        Ok(())
    }

    pub fn endpoint(&self, code: &str) -> Result<Endpoint, String> {
        if !LANGUAGES.contains(&code) {
            return Err("Read aloud: unsupported language".into());
        }
        let lane = self.by_language.get(code);
        let voice = lane
            .map(|l| l.voice.trim())
            .filter(|v| !v.is_empty())
            .or_else(|| (code == "en").then_some(self.voice.trim()))
            .unwrap_or("");
        if voice.is_empty() {
            return Err(format!(
                "Read aloud: configure the {code} TTS voice and endpoint first"
            ));
        }
        let base = lane
            .map(|l| l.base_url.trim())
            .filter(|v| !v.is_empty())
            .unwrap_or(self.base_url.trim());
        let uri = base
            .parse::<ureq::http::Uri>()
            .map_err(|_| "tts.base_url: invalid HTTP(S) base URL")?;
        if !matches!(uri.scheme_str(), Some("http" | "https"))
            || uri.authority().is_none()
            || uri.authority().is_some_and(|a| a.as_str().contains('@'))
            || uri.query().is_some()
            || base.contains('#')
        {
            return Err(
                "tts.base_url: use an HTTP(S) base URL without credentials, query or fragment"
                    .into(),
            );
        }
        let model = lane
            .map(|l| l.model.trim())
            .filter(|v| !v.is_empty())
            .unwrap_or(self.model.trim());
        if model.is_empty() {
            return Err("tts.model: name the model your endpoint serves".into());
        }
        let (env, file) = match lane {
            Some(l) if !l.inherit_token => (&l.api_key_env, &l.api_key_file),
            _ => (&self.api_key_env, &self.api_key_file),
        };
        Ok(Endpoint {
            base_url: base.trim_end_matches('/').into(),
            model: model.into(),
            voice: voice.into(),
            token: crate::config::resolve_token(env, file),
        })
    }

    pub fn prepare(&self, text: &str) -> Result<Vec<(String, Endpoint)>, String> {
        if !self.enabled {
            return Err("Read aloud: set tts.enabled and configure the TTS endpoint first".into());
        }
        chunks(text)?
            .into_iter()
            .map(|chunk| {
                let lang = script_language(&chunk)?;
                Ok((chunk, self.endpoint(lang)?))
            })
            .collect()
    }
}

/// A document and its focused child may report the same selection. Different ranges refuse.
pub fn unique_selection(
    selections: impl IntoIterator<Item = Option<String>>,
) -> Result<Option<String>, String> {
    let mut result: Option<String> = None;
    for text in selections.into_iter().flatten() {
        if text.chars().count() > MAX_TEXT {
            return Err("Read aloud: selection exceeds 20,000 characters".into());
        }
        if result.as_ref().is_some_and(|prior| prior != &text) {
            return Err("Read aloud: multiple selections found; select one text range".into());
        }
        result = Some(text);
    }
    Ok(result)
}

/// Dominant supported script per chunk, not keyboard layout. Unknown dominant scripts are refused.
pub fn script_language(text: &str) -> Result<&'static str, String> {
    let mut counts = [0usize; 4];
    for c in text.chars().filter(|c| c.is_alphabetic()) {
        let i = match c as u32 {
            0x0590..=0x05ff => 1,
            0x0400..=0x052f => 2,
            0x0041..=0x024f => 0,
            _ => 3,
        };
        counts[i] += 1;
    }
    let i = (0..4).max_by_key(|i| counts[*i]).unwrap_or(3);
    if counts[i] == 0 || i == 3 {
        return Err("Read aloud: select text in English, Hebrew or Russian".into());
    }
    Ok(LANGUAGES[i])
}

pub fn chunks(text: &str) -> Result<Vec<String>, String> {
    let mut rest = text.trim();
    if rest.is_empty() {
        return Err("Read aloud: select text first".into());
    }
    if rest.chars().count() > MAX_TEXT {
        return Err("Read aloud: selection exceeds 20,000 characters".into());
    }
    let mut chunks = Vec::new();
    while !rest.is_empty() {
        let boundary = rest
            .char_indices()
            .nth(MAX_CHUNK)
            .map(|(i, _)| i)
            .unwrap_or(rest.len());
        let prefix = &rest[..boundary];
        let at = if boundary == rest.len() {
            boundary
        } else {
            prefix
                .char_indices()
                .rev()
                .find(|(i, c)| *i >= boundary / 2 && (c.is_whitespace() || ".!?;".contains(*c)))
                .map(|(i, c)| i + c.len_utf8())
                .unwrap_or(boundary)
        };
        chunks.push(rest[..at].trim().to_owned());
        rest = rest[at..].trim_start();
    }
    Ok(chunks)
}

pub struct SpeechAudio {
    pub samples: Vec<f32>,
    pub rate: u32,
    pub channels: u16,
}

pub fn decode_wav(bytes: &[u8]) -> Result<SpeechAudio, String> {
    if bytes.len() > MAX_AUDIO {
        return Err("Read aloud: audio reply exceeds the limit".into());
    }
    let complete = bounded_wav(bytes)?;
    let mut wav = hound::WavReader::new(Cursor::new(&complete))
        .map_err(|_| "Read aloud: invalid WAV reply")?;
    let spec = wav.spec();
    if spec.bits_per_sample != 16
        || spec.sample_format != hound::SampleFormat::Int
        || !(8_000..=48_000).contains(&spec.sample_rate)
        || !(1..=2).contains(&spec.channels)
        || wav.duration() > spec.sample_rate * 120
    {
        return Err("Read aloud: unsupported or oversized WAV reply".into());
    }
    let samples = wav
        .samples::<i16>()
        .map(|s| s.map(|v| f32::from(v) / 32768.0))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| "Read aloud: truncated WAV reply")?;
    if samples.is_empty() {
        return Err("Read aloud: server returned no audio".into());
    }
    Ok(SpeechAudio {
        samples,
        rate: spec.sample_rate,
        channels: spec.channels,
    })
}

/// Complete only streaming RIFF/data length sentinels from the already byte-bounded HTTP body.
fn bounded_wav(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let bad = || "Read aloud: invalid WAV reply".to_owned();
    if bytes.len() < 12 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err(bad());
    }
    let declared = u32::from_le_bytes(bytes[4..8].try_into().map_err(|_| bad())?);
    if declared != u32::MAX && u64::from(declared) + 8 != bytes.len() as u64 {
        return Err(bad());
    }
    let mut fixed = bytes.to_vec();
    let mut at = 12;
    let mut blocks = 0;
    while at + 8 <= bytes.len() {
        blocks += 1;
        if blocks > 128 {
            return Err(bad());
        }
        let stored = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().map_err(|_| bad())?);
        let size = if stored == u32::MAX && &bytes[at..at + 4] == b"data" && declared == u32::MAX {
            let actual = bytes.len() - at - 8;
            fixed[at + 4..at + 8].copy_from_slice(&(actual as u32).to_le_bytes());
            actual
        } else {
            stored as usize
        };
        if size > bytes.len() - at - 8 {
            return Err(bad());
        }
        at += 8 + size + (size & 1);
    }
    if at != bytes.len() {
        return Err(bad());
    }
    fixed[4..8].copy_from_slice(&((bytes.len() - 8) as u32).to_le_bytes());
    Ok(fixed)
}

pub fn synthesize(
    config: &TtsConfig,
    endpoint: &Endpoint,
    text: &str,
    current: impl Fn() -> bool,
) -> Result<SpeechAudio, String> {
    if !current() {
        return Err("Read aloud cancelled".into());
    }
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(5)))
        .timeout_global(Some(Duration::from_secs(config.timeout_s)))
        .max_redirects(0)
        .http_status_as_error(false)
        .build()
        .into();
    let mut request = agent
        .post(format!("{}/audio/speech", endpoint.base_url))
        .header("Content-Type", "application/json");
    if let Some(token) = &endpoint.token {
        request = request.header("Authorization", format!("Bearer {token}"));
    }
    let body = serde_json::json!({"model":endpoint.model,"voice":endpoint.voice,"input":text,
        "response_format":"wav","stream":false,"speed":config.speed})
    .to_string();
    let mut response = request
        .send(body.as_bytes())
        .map_err(|_| "Read aloud: connection failed; check the TTS endpoint and Tailscale")?;
    if response.status().as_u16() != 200 {
        return Err(format!(
            "Read aloud: TTS returned HTTP {}",
            response.status().as_u16()
        ));
    }
    let mut bytes = Vec::new();
    let mut reader = response.body_mut().as_reader().take((MAX_AUDIO + 1) as u64);
    let mut buffer = [0u8; 8192];
    loop {
        if !current() {
            return Err("Read aloud cancelled".into());
        }
        let n = reader
            .read(&mut buffer)
            .map_err(|_| "Read aloud: incomplete audio reply")?;
        if n == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..n]);
    }
    if !current() {
        return Err("Read aloud cancelled".into());
    }
    decode_wav(&bytes)
}

#[cfg(test)]
mod tests {
    #[test]
    fn narration_copy_fallback_is_default_and_can_be_disabled() {
        let defaults = serde_json::to_value(super::TtsConfig::default()).unwrap();
        assert_eq!(
            defaults.get("copy_fallback"),
            Some(&serde_json::Value::Bool(true))
        );
        let legacy: super::TtsConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(serde_json::to_value(legacy).unwrap()["copy_fallback"], true);
        let opted_out: super::TtsConfig =
            serde_json::from_str(r#"{"copy_fallback":false}"#).unwrap();
        assert_eq!(
            serde_json::to_value(opted_out).unwrap()["copy_fallback"],
            false
        );
    }

    use super::*;
    use std::io::Write;
    #[test]
    fn document_selection_is_not_lost_when_the_focused_composer_has_only_a_caret() {
        assert_eq!(
            unique_selection([None, Some("Selected synthetic chat paragraph".into())]).unwrap(),
            Some("Selected synthetic chat paragraph".into())
        );
        assert_eq!(unique_selection([None, None]).unwrap(), None);
        assert_eq!(
            unique_selection([Some("same".into()), Some("same".into())]).unwrap(),
            Some("same".into())
        );
        assert!(unique_selection([Some("first".into()), Some("second".into())]).is_err());
        assert!(unique_selection([Some("x".repeat(MAX_TEXT + 1))]).is_err());
    }
    #[test]
    fn narration_defaults_to_a_separate_shortcut_and_refuses_the_same_chord() {
        let mut cfg = TtsConfig {
            enabled: true,
            ..Default::default()
        };
        assert_eq!(cfg.hotkey, "ControlLeft+AltLeft+R");
        assert!(
            cfg.validate_hotkeys("ControlLeft+ShiftLeft+Z", "Escape")
                .is_ok()
        );
        cfg.hotkey.clear();
        assert_eq!(cfg.effective_hotkey(), "ControlLeft+AltLeft+R");
        assert!(
            cfg.validate_hotkeys("ControlLeft+ShiftLeft+Z", "Escape")
                .is_ok()
        );
        cfg.hotkey = "ShiftLeft+ControlLeft+Z".into();
        assert!(
            cfg.validate_hotkeys("ControlLeft+ShiftLeft+Z", "Escape")
                .is_err()
        );
    }
    fn server(
        status: u16,
        body: Vec<u8>,
        extra: &str,
    ) -> (
        String,
        std::sync::Arc<std::sync::Mutex<String>>,
        std::thread::JoinHandle<()>,
    ) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let captured = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let out = captured.clone();
        let extra = extra.to_owned();
        let task = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut head = Vec::new();
            let mut one = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                socket.read_exact(&mut one).unwrap();
                head.push(one[0]);
            }
            let head = String::from_utf8(head).unwrap();
            let length = head
                .lines()
                .find_map(|l| {
                    l.split_once(':')
                        .filter(|(n, _)| n.eq_ignore_ascii_case("content-length"))
                        .map(|(_, v)| v.trim().parse::<usize>().unwrap())
                })
                .unwrap();
            let mut request = vec![0u8; length];
            socket.read_exact(&mut request).unwrap();
            *out.lock().unwrap() = head + &String::from_utf8(request).unwrap();
            let response = format!(
                "HTTP/1.1 {status} Result\r\nContent-Type: audio/wav\r\nContent-Length: {}\r\n{extra}Connection: close\r\n\r\n",
                body.len()
            );
            let mut reply = response.into_bytes();
            reply.extend_from_slice(&body);
            socket.write_all(&reply).unwrap();
        });
        (url, captured, task)
    }
    #[test]
    fn actual_tts_request_uses_the_configured_voice_and_tts_token_only() {
        let (url, request, task) = server(200, wav_fixture(), "");
        let cfg = TtsConfig {
            enabled: true,
            base_url: url.clone(),
            ..Default::default()
        };
        let endpoint = Endpoint {
            base_url: url,
            model: "kokoro".into(),
            voice: "af_bella".into(),
            token: Some("synthetic-tts-token".into()),
        };
        assert_eq!(
            synthesize(&cfg, &endpoint, "Harmless fixture.", || true)
                .unwrap()
                .samples
                .len(),
            4
        );
        task.join().unwrap();
        let text = request.lock().unwrap();
        assert!(text.contains("POST /v1/audio/speech"));
        assert!(text.contains("Bearer synthetic-tts-token"));
        let body: serde_json::Value =
            serde_json::from_str(text.split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(body["input"], "Harmless fixture.");
        assert_eq!(body["voice"], "af_bella");
        assert_eq!(body["stream"], false);
    }
    #[test]
    fn server_errors_and_redirects_cannot_echo_or_forward_selected_text_and_credentials() {
        for status in [302, 403] {
            let (url, _, task) = server(
                status,
                b"PRIVATE-FIXTURE synthetic-token".to_vec(),
                "Location: http://127.0.0.1:9/never\r\n",
            );
            let cfg = TtsConfig {
                enabled: true,
                base_url: url.clone(),
                ..Default::default()
            };
            let endpoint = Endpoint {
                base_url: url,
                model: "kokoro".into(),
                voice: "af_bella".into(),
                token: Some("synthetic-token".into()),
            };
            let err = synthesize(&cfg, &endpoint, "PRIVATE-FIXTURE", || true)
                .err()
                .unwrap();
            task.join().unwrap();
            assert!(err.contains(&format!("HTTP {status}")));
            assert!(!err.contains("PRIVATE-FIXTURE"));
            assert!(!err.contains("synthetic-token"));
        }
    }
    #[test]
    fn cancelled_reply_produces_no_audio() {
        let (url, _, task) = server(200, wav_fixture(), "");
        let cfg = TtsConfig {
            enabled: true,
            base_url: url,
            ..Default::default()
        };
        let endpoint = cfg.endpoint("en").unwrap();
        let calls = std::sync::atomic::AtomicUsize::new(0);
        assert!(
            synthesize(&cfg, &endpoint, "Harmless fixture.", || calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                == 0)
            .is_err()
        );
        task.join().unwrap();
    }
    #[test]
    fn language_credentials_require_explicit_inheritance() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let file =
            std::env::temp_dir().join(format!("byovox-tts-{}-{stamp}.env", std::process::id()));
        let root_name = format!("BYOVOX_TTS_ROOT_{stamp}");
        let lane_name = format!("BYOVOX_TTS_LANE_{stamp}");
        std::fs::write(
            &file,
            format!("{root_name}=synthetic-root\n{lane_name}=synthetic-lane\n"),
        )
        .unwrap();
        let mut cfg = TtsConfig {
            enabled: true,
            base_url: "https://example.invalid/v1".into(),
            api_key_env: root_name,
            api_key_file: file.to_string_lossy().into_owned(),
            ..Default::default()
        };
        let lane = cfg.by_language.get_mut("he").unwrap();
        lane.voice = "he-voice".into();
        lane.base_url = "https://other.invalid/v1".into();
        assert!(cfg.endpoint("he").unwrap().token.is_none());
        cfg.by_language.get_mut("he").unwrap().inherit_token = true;
        assert_eq!(
            cfg.endpoint("he").unwrap().token.as_deref(),
            Some("synthetic-root")
        );
        let lane = cfg.by_language.get_mut("he").unwrap();
        lane.inherit_token = false;
        lane.api_key_env = lane_name;
        lane.api_key_file = file.to_string_lossy().into_owned();
        assert_eq!(
            cfg.endpoint("he").unwrap().token.as_deref(),
            Some("synthetic-lane")
        );
        cfg.by_language.get_mut("he").unwrap().inherit_token = true;
        assert!(cfg.validate().is_err());
        std::fs::remove_file(file).unwrap();
    }

    #[test]
    fn overlapping_read_dictation_and_cancel_keys_are_rejected() {
        let mut cfg = TtsConfig {
            enabled: true,
            ..Default::default()
        };
        assert!(cfg.validate_hotkeys("ControlRight", "Escape").is_ok());
        cfg.hotkey = "ControlRight+R".into();
        assert!(cfg.validate_hotkeys("ControlRight", "Escape").is_err());
        cfg.hotkey = "ControlLeft+Escape".into();
        assert!(cfg.validate_hotkeys("ControlRight", "Escape").is_err());
    }
    fn wav_fixture() -> Vec<u8> {
        let mut buffer = Cursor::new(Vec::new());
        {
            let mut out = hound::WavWriter::new(
                &mut buffer,
                hound::WavSpec {
                    channels: 1,
                    sample_rate: 24_000,
                    bits_per_sample: 16,
                    sample_format: hound::SampleFormat::Int,
                },
            )
            .unwrap();
            for sample in [0i16, 100, -100, 0] {
                out.write_sample(sample).unwrap();
            }
            out.finalize().unwrap();
        }
        buffer.into_inner()
    }
    #[test]
    fn kokoro_streaming_wav_lengths_use_the_bounded_received_body() {
        let mut bytes = wav_fixture();
        bytes[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
        let data = bytes.windows(4).position(|w| w == b"data").unwrap();
        bytes[data + 4..data + 8].copy_from_slice(&u32::MAX.to_le_bytes());
        let decoded = decode_wav(&bytes).unwrap();
        assert_eq!(decoded.samples.len(), 4);
    }
    #[test]
    fn all_three_scripts_have_explicit_routes() {
        assert_eq!(script_language("Hello world").unwrap(), "en");
        assert_eq!(script_language("שלום עולם").unwrap(), "he");
        assert_eq!(script_language("Привет мир").unwrap(), "ru");
        assert!(script_language("مرحبا").is_err());
        assert!(script_language("123").is_err());
    }
    #[test]
    fn english_is_ready_to_configure_and_other_voices_never_fall_back_to_it() {
        let cfg = TtsConfig {
            enabled: true,
            base_url: "http://127.0.0.1:8770/v1".into(),
            ..Default::default()
        };
        assert_eq!(cfg.endpoint("en").unwrap().voice, "af_bella");
        for lang in ["he", "ru"] {
            assert!(cfg.endpoint(lang).is_err());
        }
        assert!(cfg.prepare("שלום").is_err());
    }
    #[test]
    fn a_language_can_have_its_own_model_voice_and_endpoint() {
        let mut cfg = TtsConfig::default();
        cfg.by_language.insert(
            "he".into(),
            TtsLane {
                base_url: "https://example.invalid/v1".into(),
                model: "he-model".into(),
                voice: "he-voice".into(),
                ..Default::default()
            },
        );
        let lane = cfg.endpoint("he").unwrap();
        assert_eq!(lane.model, "he-model");
        assert_eq!(lane.voice, "he-voice");
        assert_eq!(lane.base_url, "https://example.invalid/v1");
    }
    #[test]
    fn chunks_are_bounded_unicode_and_do_not_drop_text() {
        let input = "שלום ".repeat(500);
        let parts = chunks(&input).unwrap();
        assert!(parts.iter().all(|p| p.chars().count() <= MAX_CHUNK));
        assert_eq!(
            parts.join(" ").split_whitespace().collect::<Vec<_>>(),
            input.split_whitespace().collect::<Vec<_>>()
        );
        assert!(chunks("").is_err());
        assert!(chunks(&"x".repeat(MAX_TEXT + 1)).is_err());
    }
    #[test]
    fn malformed_audio_and_unsafe_configuration_are_rejected() {
        assert!(decode_wav(b"not audio").is_err());
        let mut cfg = TtsConfig {
            enabled: true,
            base_url: "https://user:password@example.invalid/v1".into(),
            ..Default::default()
        };
        assert!(cfg.validate().is_err());
        cfg.base_url = "http://127.0.0.1/v1".into();
        cfg.speed = f32::NAN;
        assert!(cfg.validate().is_err());
    }
}
