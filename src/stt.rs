//! Speech-to-text client: one multipart POST to `{base_url}/audio/transcriptions`, and `Routed`,
//! which picks the client for a dictation's language when `[stt.by_language]` has a lane.

use std::time::Duration;

use crate::lang::{Lang, SttLanguage};
use crate::multipart::Multipart;

/// One transcription: the trimmed text (segment line breaks joined on a space) and whisper's
/// no-speech score. The score is `None` when the reply carried no `segments`.
#[derive(Debug, Clone, PartialEq)]
pub struct Transcript {
    pub text: String,
    /// The strongest `no_speech_prob` across segments: drives the warning cue and the capture
    /// log, not the drop.
    pub no_speech_prob: Option<f32>,
    /// The duration-weighted mean `no_speech_prob`; what the no-speech gate drops on. `None`
    /// exactly when `no_speech_prob` is.
    pub no_speech_mean: Option<f32>,
    /// The `language=` the request that produced this reply carried; `None` for an
    /// auto-detect request.
    pub language: Option<Lang>,
    /// The language whisper detected on the auto-detect request, with its probability; `None`
    /// when that request was not scored or its reply named no language.
    pub detected: Option<(String, f32)>,
}

/// The detection probability at or above which `Routed` re-sends an auto-detect dictation to
/// the detected language's lane.
pub const ROUTE_P: f32 = 0.5;

/// A take longer than this many seconds asks the server for its VAD (`vad=true`); without it
/// whisper can collapse long, mostly silent audio into one repeated line.
const LONG_TAKE_S: u32 = 60;

/// Whether `wav` holds more than `LONG_TAKE_S` seconds of audio, by its header's sample
/// rate. Bytes that do not parse as a WAV are not a long take.
fn is_long_take(wav: &[u8]) -> bool {
    hound::WavReader::new(wav).is_ok_and(|r| {
        u64::from(r.duration()) > u64::from(LONG_TAKE_S) * u64::from(r.spec().sample_rate)
    })
}

pub trait Transcriber: Send {
    fn transcribe(
        &self,
        wav: &[u8],
        language: &SttLanguage,
        prompt: Option<&str>,
    ) -> Result<Transcript, String>;
}

pub struct SttClient {
    url: String,
    model: String,
    api_key: Option<String>,
    scored: bool,
    timeout: Duration,
    connect_timeout: Duration,
    agent: ureq::Agent,
}

impl SttClient {
    /// `scored` asks for `verbose_json`, the only format carrying `segments[].no_speech_prob`.
    /// Some OpenAI-compatible servers (`gpt-4o-transcribe`) reject it, so it is on only when a
    /// score will be read (`stt.no_speech_threshold > 0`).
    pub fn new(
        base_url: &str,
        model: &str,
        api_key: Option<String>,
        timeout: Duration,
        connect_timeout: Duration,
        scored: bool,
    ) -> SttClient {
        let config = ureq::Agent::config_builder()
            .timeout_connect(Some(connect_timeout))
            .timeout_global(Some(timeout))
            .http_status_as_error(false)
            .build();
        SttClient {
            url: format!("{}/audio/transcriptions", base_url.trim_end_matches('/')),
            model: model.to_string(),
            api_key,
            scored,
            timeout,
            connect_timeout,
            agent: config.into(),
        }
    }

    fn form(
        &self,
        wav: &[u8],
        language: &SttLanguage,
        prompt: Option<&str>,
        vad: bool,
    ) -> (String, Vec<u8>) {
        let mut m = Multipart::new();
        m.file("file", "clip.wav", "audio/wav", wav);
        m.text("model", &self.model);
        // `verbose_json` carries the segment scores but some servers refuse it: ask only when
        // a score is wanted.
        m.text(
            "response_format",
            if self.scored { "verbose_json" } else { "json" },
        );
        for (name, value) in language.form_fields() {
            m.text(name, &value);
        }
        if let Some(p) = prompt.filter(|p| !p.trim().is_empty()) {
            m.text("prompt", p);
        }
        if vad {
            m.text("vad", "true");
        }
        m.finish()
    }

    /// The reply's status and body, or the error string when no readable reply came back.
    fn send(&self, (content_type, body): (String, Vec<u8>)) -> Result<(u16, String), String> {
        let attempt = match self.post_once(&content_type, &body) {
            Attempt::SendFailed(first) => {
                tracing::warn!(error = %first, "stt transport error, retrying once");
                self.post_once(&content_type, &body)
            }
            answered => answered,
        };
        match attempt {
            Attempt::Response { status, body } => Ok((status, body)),
            // No colon straight after the status: `check::strip_body` cuts everything past
            // `HTTP <status>: ` as response body, and here what follows is the io cause,
            // which is the whole diagnosis.
            Attempt::BodyUnreadable { status, error } => {
                Err(format!("stt HTTP {status} body unreadable: {error}"))
            }
            // The stage names itself: `pipeline::summary` keeps only the head before the
            // first colon, so a bare `transport` would hide the cause.
            Attempt::SendFailed(e) => Err(format!("stt transport: {e}")),
            Attempt::TimedOut => Err(timed_out(self.timeout)),
        }
    }

    fn post_once(&self, content_type: &str, body: &[u8]) -> Attempt {
        let mut req = self
            .agent
            .post(&self.url)
            .header("Content-Type", content_type);
        if let Some(k) = &self.api_key {
            req = req.header("Authorization", &format!("Bearer {k}"));
        }
        let mut resp = match req.send(body) {
            Ok(resp) => resp,
            Err(ureq::Error::Timeout(t))
                if after_connecting(t, self.timeout, self.connect_timeout) =>
            {
                return Attempt::TimedOut;
            }
            Err(e) => return Attempt::SendFailed(redact(&e.to_string(), self.api_key.as_deref())),
        };
        let status = resp.status().as_u16();
        match resp.body_mut().read_to_string() {
            Ok(body) => Attempt::Response { status, body },
            Err(e) => Attempt::BodyUnreadable {
                status,
                error: redact(&e.to_string(), self.api_key.as_deref()),
            },
        }
    }
}

/// Whether timeout `t` fired after connecting: never `Resolve`/`Connect`; a `Global` one only when
/// `timeout` leaves `connect_timeout` for connecting plus as much again for lookup and redirects.
/// `Timeout` is `#[non_exhaustive]`; any other variant fires after connecting.
fn after_connecting(t: ureq::Timeout, timeout: Duration, connect_timeout: Duration) -> bool {
    match t {
        ureq::Timeout::Resolve | ureq::Timeout::Connect => false,
        ureq::Timeout::Global => timeout > connect_timeout * 2,
        _ => true,
    }
}

/// The outcome of one POST attempt. Only `SendFailed` may be retried: once a status has come
/// back a replay risks a second transcription.
enum Attempt {
    /// A response arrived and its body was read.
    Response { status: u16, body: String },
    /// A status arrived but the body could not be read.
    BodyUnreadable { status: u16, error: String },
    /// No response arrived, and the connection may never have opened.
    SendFailed(String),
    /// A timeout fired after the connection opened (`after_connecting`), before a status.
    TimedOut,
}

/// The head of a timed-out request's error; `is_timeout` is its one reader.
const TIMED_OUT: &str = "stt timeout: no reply within";

fn timed_out(timeout: Duration) -> String {
    format!("{TIMED_OUT} {} s", timeout.as_secs())
}

pub fn is_timeout(error: &str) -> bool {
    error.starts_with(TIMED_OUT)
}

/// What an error message carries in place of the bearer token the request sent.
const REDACTED: &str = "[redacted]";

/// `text` with every occurrence of `token` replaced by `[redacted]`. No token, or an empty
/// one, leaves `text` as it is. Used by the polish client too.
pub(crate) fn redact(text: &str, token: Option<&str>) -> String {
    match token.filter(|t| !t.is_empty()) {
        Some(t) => text.replace(t, REDACTED),
        None => text.to_string(),
    }
}

/// The leading 200 characters of a response body, for error messages, with `token` redacted
/// first so one straddling the cut leaves no fragment. Also used by the polish client.
pub(crate) fn body_prefix(body: &str, token: Option<&str>) -> String {
    redact(body, token).chars().take(200).collect()
}

/// The `(mean, max)` `no_speech_prob` of a reply's segments, the mean weighted by `end âˆ’ start`
/// (unweighted when durations are unusable). `None` when `segments` is absent, null or
/// unscored; any other malformed shape is an error, never a silent `None`.
fn no_speech_scores(
    v: &serde_json::Value,
    body: &str,
    token: Option<&str>,
) -> Result<Option<(f32, f32)>, String> {
    let Some(segments) = v.get("segments").filter(|s| !s.is_null()) else {
        return Ok(None);
    };
    let segments = segments.as_array().ok_or_else(|| {
        format!(
            "stt response segments is not an array: {}",
            body_prefix(body, token)
        )
    })?;
    let mut scores: Vec<(f32, Option<f64>)> = Vec::new();
    for s in segments {
        let Some(p) = s.get("no_speech_prob") else {
            continue;
        };
        let p = p.as_f64().ok_or_else(|| {
            format!(
                "stt response no_speech_prob is not a number: {}",
                body_prefix(body, token)
            )
        })? as f32;
        let at = |k: &str| s.get(k).and_then(serde_json::Value::as_f64);
        scores.push((p, at("start").zip(at("end")).map(|(a, b)| b - a)));
    }
    if scores.is_empty() {
        return Ok(None);
    }
    let max = scores.iter().map(|(p, _)| *p).fold(f32::MIN, f32::max);
    let weights: Option<Vec<f64>> = scores.iter().map(|(_, d)| *d).collect();
    let weights = weights
        .filter(|w| w.iter().all(|d| *d >= 0.0))
        .filter(|w| {
            let sum = w.iter().sum::<f64>();
            sum.is_finite() && sum > 0.0
        })
        .unwrap_or_else(|| vec![1.0; scores.len()]);
    let total: f64 = weights.iter().sum();
    let mean = scores
        .iter()
        .zip(&weights)
        .map(|((p, _), w)| f64::from(*p) * w)
        .sum::<f64>()
        / total;
    Ok(Some((mean as f32, max)))
}

/// The detected language of a `verbose_json` reply and its probability: the sent candidate with the
/// highest `language_probabilities` value (any code when none were sent). The server omits values
/// below 0.001, so a candidate missing from the map is not a pick; `None` when nothing qualifies.
fn detected_language(v: &serde_json::Value, candidates: &[Lang]) -> Option<(String, f32)> {
    v.get("language_probabilities")?
        .as_object()?
        .iter()
        .filter(|(code, _)| candidates.is_empty() || candidates.iter().any(|c| c.as_str() == *code))
        .filter_map(|(code, p)| Some((code.clone(), p.as_f64()?)))
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(code, p)| (code, p as f32))
}

impl Transcriber for SttClient {
    fn transcribe(
        &self,
        wav: &[u8],
        language: &SttLanguage,
        prompt: Option<&str>,
    ) -> Result<Transcript, String> {
        let vad = is_long_take(wav);
        let (mut status, mut text) = self.send(self.form(wav, language, prompt, vad))?;
        // A whisper-server started without `--vad-model` answers `vad=true` with HTTP 500,
        // and a hosted API may refuse a field it does not know: the take goes again without it.
        if vad && !(200..300).contains(&status) {
            tracing::warn!(
                status,
                "stt refused vad=true for a long take; re-sending without it"
            );
            (status, text) = self.send(self.form(wav, language, prompt, false))?;
        }
        if !(200..300).contains(&status) {
            return Err(format!(
                "stt HTTP {status}: {}",
                body_prefix(&text, self.api_key.as_deref())
            ));
        }
        let v: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| format!("stt JSON: {e}"))?;
        let transcript = v.get("text").and_then(|t| t.as_str()).ok_or_else(|| {
            format!(
                "stt response has no text field: {}",
                body_prefix(&text, self.api_key.as_deref())
            )
        })?;
        // whisper breaks `text` at pauses, not spoken line breaks: join on a space.
        let joined = transcript
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        // An unscored client reads no segments, so no shape in them can fail the request.
        let scores = if self.scored {
            no_speech_scores(&v, &text, self.api_key.as_deref())?
        } else {
            None
        };
        let (sent, detected) = match language {
            SttLanguage::Explicit(l) => (Some(*l), None),
            SttLanguage::Auto { candidates, .. } => (
                None,
                self.scored
                    .then(|| detected_language(&v, candidates))
                    .flatten(),
            ),
        };
        Ok(Transcript {
            text: joined,
            no_speech_prob: scores.map(|(_, max)| max),
            no_speech_mean: scores.map(|(mean, _)| mean),
            language: sent,
            detected,
        })
    }
}

/// A `Transcriber` routing a dictation to its language's lane (`[stt.by_language.<code>]`), else the default.
/// `Auto` goes to the default first (lane fine-tunes lose detection), then to the detected lane at `ROUTE_P`+;
/// otherwise, or when the default fails, a `prior` is sent. An empty lane reply retries on the default; a lane error is returned.
pub struct Routed {
    default: Box<dyn Transcriber>,
    lanes: Vec<(Lang, Box<dyn Transcriber>, Option<String>)>,
    detects: bool,
}

impl Routed {
    pub fn new(default: Box<dyn Transcriber>) -> Routed {
        Routed {
            default,
            lanes: Vec::new(),
            detects: true,
        }
    }

    /// Whether the default client reads a detection (`SttClient`'s `scored`). On unless set.
    pub fn detecting(mut self, detects: bool) -> Routed {
        self.detects = detects;
        self
    }

    /// Route `lang` to `client`, with `prompt` replacing the caller's when `Some`.
    pub fn lane(
        mut self,
        lang: Lang,
        client: Box<dyn Transcriber>,
        prompt: Option<String>,
    ) -> Routed {
        self.lanes.push((lang, client, prompt));
        self
    }

    /// `language=<lang>` to `lang`'s lane when it has one, and to the default otherwise.
    fn explicit(&self, wav: &[u8], lang: Lang, prompt: Option<&str>) -> Result<Transcript, String> {
        let language = SttLanguage::Explicit(lang);
        let lane = self.lanes.iter().find(|(l, _, _)| *l == lang);
        if let Some((lang, client, lane_prompt)) = lane {
            // `stt[he] HTTP 502`, not `stt HTTP 502`: `pipeline::summary` keeps this string's
            // head, so a dead lane must name itself.
            let transcript = client
                .transcribe(wav, &language, lane_prompt.as_deref().or(prompt))
                .map_err(|e| match e.strip_prefix("stt") {
                    Some(rest) => format!("stt[{lang}]{rest}"),
                    None => format!("stt[{lang}] {e}"),
                })?;
            if !transcript.text.trim().is_empty() {
                return Ok(transcript);
            }
            tracing::info!(lang = %lang, "stt lane returned nothing; retrying the default endpoint");
        }
        self.default.transcribe(wav, &language, prompt)
    }
}

impl Transcriber for Routed {
    fn transcribe(
        &self,
        wav: &[u8],
        language: &SttLanguage,
        prompt: Option<&str>,
    ) -> Result<Transcript, String> {
        let prior = match language {
            SttLanguage::Explicit(lang) => return self.explicit(wav, *lang, prompt),
            SttLanguage::Auto { prior, .. } => *prior,
        };
        if let (false, Some(lang)) = (self.detects, prior) {
            return self.explicit(wav, lang, prompt);
        }
        let first = match (self.default.transcribe(wav, language, prompt), prior) {
            (Ok(first), _) => first,
            // A failing default does not cost the layout's language its own lane; a prior
            // without one would only ask the failed default again.
            (Err(e), Some(lang)) if self.lanes.iter().any(|(l, _, _)| *l == lang) => {
                tracing::warn!(error = crate::pipeline::summary(&e), lang = %lang, "stt failed; sending the layout's language to its lane");
                tracing::debug!(error = %e, "stt failure in full");
                return self.explicit(wav, lang, prompt);
            }
            (Err(e), _) => return Err(e),
        };
        let resend = match &first.detected {
            Some((code, p)) if *p >= ROUTE_P => self
                .lanes
                .iter()
                .map(|(l, _, _)| *l)
                .find(|l| l.as_str() == code),
            _ => prior,
        };
        let Some(lang) = resend else {
            return Ok(first);
        };
        let mut transcript = self.explicit(wav, lang, prompt)?;
        transcript.detected = first.detected;
        Ok(transcript)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lang::{Lang, SttLanguage};
    use crate::testutil::{MockServer, auth_header};
    use std::time::Duration;

    fn undetermined() -> SttLanguage {
        SttLanguage::Auto {
            candidates: vec![],
            prior: None,
        }
    }

    fn client(url: &str) -> SttClient {
        SttClient::new(
            url,
            "whisper-1",
            None,
            Duration::from_secs(5),
            Duration::from_secs(5),
            true,
        )
    }

    fn unscored_client(url: &str) -> SttClient {
        SttClient::new(
            url,
            "whisper-1",
            None,
            Duration::from_secs(5),
            Duration::from_secs(5),
            false,
        )
    }

    #[test]
    fn explicit_language_and_prompt_are_sent_and_text_is_trimmed() {
        let srv = MockServer::start(200, r#"{"text":" hello world \n"}"#);
        let he = SttLanguage::Explicit(Lang::parse("he").unwrap());
        let out = client(&format!("{}/v1", srv.url()))
            .transcribe(b"RIFFxxxx", &he, Some("Glossary: Acme"))
            .unwrap();
        assert_eq!(out.text, "hello world");
        let req = srv.requests().remove(0);
        assert!(req.starts_with("POST /v1/audio/transcriptions "), "{req}");
        assert!(req.contains("name=\"language\"\r\n\r\nhe\r\n"));
        assert!(req.contains("name=\"prompt\"\r\n\r\nGlossary: Acme\r\n"));
        assert!(req.contains("name=\"response_format\"\r\n\r\nverbose_json\r\n"));
        assert!(req.contains("name=\"model\"\r\n\r\nwhisper-1\r\n"));
        assert!(
            req.contains(
                "name=\"file\"; filename=\"clip.wav\"\r\nContent-Type: audio/wav\r\n\r\nRIFFxxxx\r\n"
            ),
            "{req}"
        );
        assert_eq!(auth_header(&req), None, "{req}");
    }

    #[test]
    fn the_gate_being_off_asks_for_plain_json_and_reads_no_score() {
        let srv = MockServer::start(
            200,
            r#"{"text":"hello","segments":[{"id":0,"no_speech_prob":0.9}]}"#,
        );
        let out = unscored_client(srv.url())
            .transcribe(b"RIFF", &undetermined(), None)
            .unwrap();
        assert_eq!(out.text, "hello");
        assert_eq!(out.no_speech_prob, None);
        let req = srv.requests().remove(0);
        assert!(
            req.contains("name=\"response_format\"\r\n\r\njson\r\n"),
            "{req}"
        );
        assert!(!req.contains("verbose_json"), "{req}");
    }

    #[test]
    fn the_gate_being_off_survives_a_reply_a_scoring_client_would_refuse() {
        let srv = MockServer::start(
            200,
            r#"{"text":"hello","segments":[{"id":0,"no_speech_prob":"high"}]}"#,
        );
        let out = unscored_client(srv.url())
            .transcribe(b"RIFF", &undetermined(), None)
            .unwrap();
        assert_eq!(out.text, "hello");
        assert_eq!(out.no_speech_prob, None);
    }

    #[test]
    fn auto_sends_candidates_and_no_language_field() {
        let srv = MockServer::start(200, r#"{"text":"x"}"#);
        let auto = SttLanguage::Auto {
            candidates: vec![Lang::parse("en").unwrap(), Lang::parse("ru").unwrap()],
            prior: Some(Lang::parse("he").unwrap()),
        };
        client(srv.url()).transcribe(b"RIFF", &auto, None).unwrap();
        let req = srv.requests().remove(0);
        assert!(req.starts_with("POST /audio/transcriptions "), "{req}");
        assert!(req.contains("name=\"language_candidates\"\r\n\r\nen,ru\r\n"));
        assert!(!req.contains("name=\"language\"\r\n"));
        assert!(!req.contains("name=\"prompt\""));
    }

    fn wav_of(n: usize) -> Vec<u8> {
        crate::audio::Audio {
            samples: vec![0; n],
        }
        .to_wav()
    }

    const SIXTY_S: usize = 60 * crate::audio::SAMPLE_RATE as usize;

    #[test]
    fn a_take_over_sixty_seconds_sends_vad() {
        let srv = MockServer::start(200, r#"{"text":"x"}"#);
        let he = SttLanguage::Explicit(Lang::parse("he").unwrap());
        for language in [undetermined(), he] {
            client(srv.url())
                .transcribe(&wav_of(SIXTY_S + 1), &language, None)
                .unwrap();
        }
        let reqs = srv.requests();
        assert_eq!(reqs.len(), 2);
        for req in reqs {
            assert!(req.contains("name=\"vad\"\r\n\r\ntrue\r\n"), "no vad field");
        }
    }

    #[test]
    fn a_take_of_sixty_seconds_sends_no_vad() {
        let srv = MockServer::start(200, r#"{"text":"x"}"#);
        client(srv.url())
            .transcribe(&wav_of(SIXTY_S), &undetermined(), None)
            .unwrap();
        assert!(
            !srv.requests()[0].contains("name=\"vad\""),
            "vad field sent"
        );
    }

    #[test]
    fn a_refused_vad_is_sent_again_without_it() {
        let srv = MockServer::start_then(
            (500, r#"{"error":"failed to process audio"}"#),
            (200, r#"{"text":"x"}"#),
        );
        let out = client(srv.url())
            .transcribe(&wav_of(SIXTY_S + 1), &undetermined(), None)
            .unwrap();
        assert_eq!(out.text, "x");
        let reqs = srv.requests();
        assert_eq!(reqs.len(), 2, "not sent again");
        assert!(reqs[0].contains("name=\"vad\""), "first request had no vad");
        assert!(!reqs[1].contains("name=\"vad\""), "second request kept vad");
    }

    #[test]
    fn a_refused_short_take_is_not_sent_again() {
        let srv = MockServer::start(500, r#"{"error":"x"}"#);
        let err = client(srv.url())
            .transcribe(&wav_of(SIXTY_S), &undetermined(), None)
            .unwrap_err();
        assert!(err.starts_with("stt HTTP 500"), "{err}");
        assert_eq!(srv.requests().len(), 1, "sent again");
    }

    #[test]
    fn the_agent_takes_the_given_timeouts() {
        let c = SttClient::new(
            "http://x/v1",
            "m",
            None,
            Duration::from_secs(30),
            Duration::from_secs(12),
            true,
        );
        let t = c.agent.config().timeouts();
        assert_eq!(t.connect, Some(Duration::from_secs(12)));
        assert_eq!(t.global, Some(Duration::from_secs(30)));
    }

    #[test]
    fn bearer_header_when_key_given() {
        let srv = MockServer::start(200, r#"{"text":"x"}"#);
        let c = SttClient::new(
            srv.url(),
            "m",
            Some("tok".into()),
            Duration::from_secs(5),
            Duration::from_secs(5),
            true,
        );
        c.transcribe(b"RIFF", &undetermined(), None).unwrap();
        let req = srv.requests().remove(0);
        assert_eq!(auth_header(&req).as_deref(), Some("Bearer tok"), "{req}");
    }

    #[test]
    fn http_error_is_an_error_with_status_and_no_retry() {
        let srv = MockServer::start(500, r#"{"error":"boom"}"#);
        let err = client(srv.url())
            .transcribe(b"RIFF", &undetermined(), None)
            .unwrap_err();
        assert!(err.contains("500"), "{err}");
        assert_eq!(srv.requests().len(), 1, "4xx/5xx must not be retried");
    }

    #[test]
    fn a_failed_connection_is_an_error_that_names_the_stage() {
        let c = client("http://127.0.0.1:0");
        let err = c.transcribe(b"RIFF", &undetermined(), None).unwrap_err();
        assert!(err.starts_with("stt transport: "), "{err}");
    }

    #[test]
    fn a_truncated_body_keeps_the_status_and_is_not_retried() {
        let srv = MockServer::start_raw(
            "HTTP/1.1 500 X\r\nContent-Length: 20\r\nConnection: close\r\n\r\n",
        );
        let err = client(srv.url())
            .transcribe(b"RIFF", &undetermined(), None)
            .unwrap_err();
        assert!(err.contains("500"), "{err}");
        assert!(err.starts_with("stt HTTP 500 body unreadable: "), "{err}");
        assert_eq!(
            srv.requests().len(),
            1,
            "a received status must not be retried"
        );
    }

    #[test]
    fn a_send_failure_is_retried_once_and_succeeds() {
        let srv = MockServer::start_closing_first(200, r#"{"text":"second try"}"#);
        let out = client(srv.url())
            .transcribe(b"RIFF", &undetermined(), None)
            .unwrap();
        assert_eq!(out.text, "second try");
        assert_eq!(
            srv.requests().len(),
            1,
            "only the retry's request is read and recorded"
        );
    }

    #[test]
    fn only_a_timeout_after_connecting_counts_as_one() {
        use ureq::Timeout::*;
        let s = Duration::from_secs;
        assert!(!after_connecting(Resolve, s(30), s(5)), "resolve");
        assert!(!after_connecting(Connect, s(30), s(5)), "connect");
        assert!(
            after_connecting(Global, s(30), s(5)),
            "global, with room to connect"
        );
        assert!(
            !after_connecting(Global, s(10), s(5)),
            "global, maybe still connecting"
        );
        assert!(
            !after_connecting(Global, s(2), s(5)),
            "global, shorter than connect"
        );
        assert!(after_connecting(RecvResponse, s(2), s(5)), "a later phase");
    }

    #[test]
    fn a_timed_out_request_is_not_retried() {
        let srv = MockServer::start_stalling();
        let c = SttClient::new(
            srv.url(),
            "m",
            None,
            Duration::from_secs(1),
            Duration::from_millis(400),
            true,
        );
        let err = c.transcribe(b"RIFF", &undetermined(), None).unwrap_err();
        assert!(err.starts_with("stt timeout: no reply within 1 s"), "{err}");
        assert!(is_timeout(&err), "{err}");
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(
            srv.requests().len(),
            1,
            "a timed-out request was sent again"
        );
    }

    const KEY: &str = "sk-live-SECRET-token";

    fn keyed_client(url: &str, key: Option<&str>) -> SttClient {
        SttClient::new(
            url,
            "whisper-1",
            key.map(str::to_string),
            Duration::from_secs(5),
            Duration::from_secs(5),
            true,
        )
    }

    #[test]
    fn a_reflected_bearer_token_is_redacted_from_every_body_quoting_error() {
        let echo = format!("Authorization: Bearer {KEY}");
        for (status, body) in [
            (500, format!(r#"{{"error":"{echo}"}}"#)),
            (200, format!(r#"{{"error":"{echo}"}}"#)),
            (200, format!(r#"{{"text":"x","segments":"{echo}"}}"#)),
            (
                200,
                format!(r#"{{"text":"x","segments":[{{"no_speech_prob":"{echo}"}}]}}"#),
            ),
        ] {
            let srv = MockServer::start(status, &body);
            let err = keyed_client(srv.url(), Some(KEY))
                .transcribe(b"RIFF", &undetermined(), None)
                .unwrap_err();
            assert!(!err.contains(KEY), "token leaked: {err}");
            assert!(err.contains("Bearer [redacted]"), "{err}");
        }
    }

    #[test]
    fn a_token_across_the_cut_leaves_no_fragment() {
        let body = format!("{}{KEY}{}", ".".repeat(195), ".".repeat(50));
        let srv = MockServer::start(500, &body);
        let err = keyed_client(srv.url(), Some(KEY))
            .transcribe(b"RIFF", &undetermined(), None)
            .unwrap_err();
        assert_eq!(
            err,
            format!("stt HTTP 500: {}[reda", ".".repeat(195)),
            "{err}"
        );
    }

    #[test]
    fn a_body_without_the_token_is_cut_at_200_characters() {
        let srv = MockServer::start(500, &"a".repeat(300));
        for key in [Some(KEY), None] {
            let err = keyed_client(srv.url(), key)
                .transcribe(b"RIFF", &undetermined(), None)
                .unwrap_err();
            assert_eq!(err, format!("stt HTTP 500: {}", "a".repeat(200)), "{err}");
        }
    }

    #[test]
    fn an_empty_key_redacts_nothing() {
        let srv = MockServer::start(500, "boom");
        let err = keyed_client(srv.url(), Some(""))
            .transcribe(b"RIFF", &undetermined(), None)
            .unwrap_err();
        assert_eq!(err, "stt HTTP 500: boom", "{err}");
    }

    #[test]
    fn a_reflected_bearer_token_is_redacted_from_a_transport_cause() {
        let srv = MockServer::start_raw(&format!(
            "HTTP/1.1 302 Found\r\nLocation: ../../../../{KEY}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        ));
        let err = keyed_client(srv.url(), Some(KEY))
            .transcribe(b"RIFF", &undetermined(), None)
            .unwrap_err();
        assert!(err.starts_with("stt transport: "), "{err}");
        assert!(!err.contains(KEY), "token leaked: {err}");
        assert!(err.contains("[redacted]"), "{err}");
    }

    #[test]
    fn redact_replaces_each_occurrence_literally() {
        assert_eq!(
            redact("a tok b tok", Some("tok")),
            "a [redacted] b [redacted]"
        );
        assert_eq!(
            redact("a.b$c a.b$c", Some("a.b$c")),
            "[redacted] [redacted]"
        );
        assert_eq!(redact("axbxc", Some("a.b")), "axbxc");
        assert_eq!(redact("Tok TOK", Some("tok")), "Tok TOK");
        assert_eq!(redact("stoked", Some("tok")), "s[redacted]ed");
        assert_eq!(redact("boom", Some("")), "boom");
        assert_eq!(redact("boom", None), "boom");
    }

    #[test]
    fn is_timeout_rejects_a_transport_or_http_error() {
        assert!(!is_timeout("stt transport: io: Connection refused"));
        assert!(!is_timeout(r#"stt HTTP 500: {"error":"boom"}"#));
    }

    #[test]
    fn a_2xx_without_a_text_field_is_an_error() {
        let srv = MockServer::start(200, r#"{"error":"no speech detected"}"#);
        let err = client(srv.url())
            .transcribe(b"RIFF", &undetermined(), None)
            .unwrap_err();
        assert!(err.contains("stt response has no text field"), "{err}");
        assert!(err.contains("no speech detected"), "{err}");
    }

    #[test]
    fn segment_line_breaks_are_joined_on_a_space() {
        let srv = MockServer::start(
            200,
            r#"{"text":" first thing \n second thing\r\n\nthird "}"#,
        );
        let out = client(srv.url())
            .transcribe(b"RIFF", &undetermined(), None)
            .unwrap();
        assert_eq!(out.text, "first thing second thing third");
    }

    type Calls = std::sync::Arc<std::sync::Mutex<Vec<(String, Option<String>)>>>;

    struct Fake {
        reply: Result<Transcript, String>,
        calls: Calls,
    }

    impl Transcriber for Fake {
        fn transcribe(
            &self,
            _wav: &[u8],
            language: &SttLanguage,
            prompt: Option<&str>,
        ) -> Result<Transcript, String> {
            self.calls
                .lock()
                .unwrap()
                .push((language.label(), prompt.map(str::to_string)));
            self.reply.clone().map(|t| Transcript {
                language: match language {
                    SttLanguage::Explicit(l) => Some(*l),
                    SttLanguage::Auto { .. } => None,
                },
                ..t
            })
        }
    }

    fn fake(reply: Result<&str, &str>) -> (Fake, Calls) {
        detecting(reply, None)
    }

    fn detecting(reply: Result<&str, &str>, detected: Option<(&str, f32)>) -> (Fake, Calls) {
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let reply = reply
            .map(|t| Transcript {
                text: t.to_string(),
                no_speech_prob: None,
                no_speech_mean: None,
                language: None,
                detected: detected.map(|(code, p)| (code.to_string(), p)),
            })
            .map_err(str::to_string);
        (
            Fake {
                reply,
                calls: calls.clone(),
            },
            calls,
        )
    }

    fn he() -> SttLanguage {
        SttLanguage::Explicit(Lang::parse("he").unwrap())
    }

    #[test]
    fn a_lane_takes_its_language_with_its_own_prompt() {
        let (d, d_calls) = fake(Ok("default"));
        let (l, l_calls) = fake(Ok("lane"));
        let r = Routed::new(Box::new(d)).lane(
            Lang::parse("he").unwrap(),
            Box::new(l),
            Some("Glossary: he".into()),
        );
        let out = r
            .transcribe(b"RIFF", &he(), Some("Glossary: global"))
            .unwrap();
        assert_eq!(out.text, "lane");
        assert_eq!(
            l_calls.lock().unwrap().as_slice(),
            [("he".to_string(), Some("Glossary: he".to_string()))]
        );
        assert!(d_calls.lock().unwrap().is_empty());
        let (d, _) = fake(Ok("default"));
        let (l, l_calls) = fake(Ok("lane"));
        let r = Routed::new(Box::new(d)).lane(Lang::parse("he").unwrap(), Box::new(l), None);
        r.transcribe(b"RIFF", &he(), Some("Glossary: global"))
            .unwrap();
        assert_eq!(
            l_calls.lock().unwrap()[0].1.as_deref(),
            Some("Glossary: global")
        );
    }

    #[test]
    fn undetected_auto_and_unrouted_languages_take_the_default() {
        let (d, d_calls) = fake(Ok("default"));
        let (l, l_calls) = fake(Ok("lane"));
        let r = Routed::new(Box::new(d)).lane(Lang::parse("he").unwrap(), Box::new(l), None);
        let auto = SttLanguage::Auto {
            candidates: vec![Lang::parse("he").unwrap()],
            prior: None,
        };
        assert_eq!(r.transcribe(b"RIFF", &auto, None).unwrap().text, "default");
        let ru = SttLanguage::Explicit(Lang::parse("ru").unwrap());
        assert_eq!(r.transcribe(b"RIFF", &ru, None).unwrap().text, "default");
        assert_eq!(d_calls.lock().unwrap().len(), 2);
        assert!(l_calls.lock().unwrap().is_empty());
    }

    #[test]
    fn an_empty_lane_reply_falls_back_to_the_default() {
        let (d, d_calls) = fake(Ok("תודה"));
        let (l, l_calls) = fake(Ok("  \n"));
        let r = Routed::new(Box::new(d)).lane(
            Lang::parse("he").unwrap(),
            Box::new(l),
            Some("Glossary: he".into()),
        );
        let out = r
            .transcribe(b"RIFF", &he(), Some("Glossary: global"))
            .unwrap();
        assert_eq!(out.text, "תודה");
        assert_eq!(l_calls.lock().unwrap().len(), 1);
        assert_eq!(
            d_calls.lock().unwrap().as_slice(),
            [("he".to_string(), Some("Glossary: global".to_string()))]
        );
    }

    #[test]
    fn a_lane_error_is_returned_not_retried() {
        let (d, d_calls) = fake(Ok("default"));
        let (l, _) = fake(Err("stt HTTP 502: bad gateway"));
        let r = Routed::new(Box::new(d)).lane(Lang::parse("he").unwrap(), Box::new(l), None);
        let err = r.transcribe(b"RIFF", &he(), None).unwrap_err();
        assert_eq!(err, "stt[he] HTTP 502: bad gateway");
        assert!(d_calls.lock().unwrap().is_empty());
    }

    fn auto(prior: Option<&str>) -> SttLanguage {
        SttLanguage::Auto {
            candidates: ["en", "ru", "he"].map(|c| Lang::parse(c).unwrap()).to_vec(),
            prior: prior.and_then(Lang::parse),
        }
    }

    fn with_he_lane(detected: Option<(&str, f32)>) -> (Routed, Calls, Calls) {
        let (d, d_calls) = detecting(Ok("default"), detected);
        let (l, l_calls) = fake(Ok("lane"));
        let r = Routed::new(Box::new(d)).lane(Lang::parse("he").unwrap(), Box::new(l), None);
        (r, d_calls, l_calls)
    }

    #[test]
    fn a_confident_detection_is_sent_again_to_its_lane() {
        let (r, d_calls, l_calls) = with_he_lane(Some(("he", 0.87)));
        let out = r.transcribe(b"RIFF", &auto(None), Some("g")).unwrap();
        assert_eq!(out.text, "lane");
        assert_eq!(
            d_calls.lock().unwrap().as_slice(),
            [("auto".to_string(), Some("g".to_string()))]
        );
        assert_eq!(
            l_calls.lock().unwrap().as_slice(),
            [("he".to_string(), Some("g".to_string()))]
        );
        assert_eq!(out.language, Lang::parse("he"));
        assert_eq!(out.detected, Some(("he".to_string(), 0.87)));
    }

    #[test]
    fn a_detection_at_exactly_the_cut_routes() {
        let (r, _, l_calls) = with_he_lane(Some(("he", ROUTE_P)));
        let out = r.transcribe(b"RIFF", &auto(None), None).unwrap();
        assert_eq!(out.text, "lane", "p = ROUTE_P did not route");
        assert_eq!(l_calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_failing_default_falls_back_to_the_prior() {
        let (d, d_calls) = fake(Err("stt transport: connection refused"));
        let (l, l_calls) = fake(Ok("lane"));
        let r = Routed::new(Box::new(d)).lane(Lang::parse("he").unwrap(), Box::new(l), None);
        let out = r.transcribe(b"RIFF", &auto(Some("he")), None);
        assert_eq!(
            out.map(|t| t.text),
            Ok("lane".to_string()),
            "a down default failed a dictation the lane could serve"
        );
        assert_eq!(d_calls.lock().unwrap().len(), 1);
        assert_eq!(l_calls.lock().unwrap().len(), 1);
        let err = r.transcribe(b"RIFF", &auto(None), None).unwrap_err();
        assert_eq!(err, "stt transport: connection refused");
    }

    #[test]
    fn a_failing_default_is_logged_when_the_lane_serves() {
        let (d, _) = fake(Err("stt HTTP 500: SECRET TRANSCRIPT"));
        let (l, _) = fake(Ok("lane"));
        let r = Routed::new(Box::new(d)).lane(Lang::parse("he").unwrap(), Box::new(l), None);
        let log = crate::testutil::logged(|| {
            r.transcribe(b"RIFF", &auto(Some("he")), None).unwrap();
        });
        assert!(
            log.contains("WARN") && log.contains("stt HTTP 500") && log.contains("lang=he"),
            "the default's failure was not logged: {log}"
        );
        assert!(
            !log.contains("SECRET"),
            "the response body reached the log: {log}"
        );
    }

    #[test]
    fn a_failing_default_is_not_asked_again_for_a_prior_without_a_lane() {
        let (d, d_calls) = fake(Err("stt HTTP 503: unavailable"));
        let (l, _) = fake(Ok("lane"));
        let r = Routed::new(Box::new(d)).lane(Lang::parse("he").unwrap(), Box::new(l), None);
        let err = r.transcribe(b"RIFF", &auto(Some("ru")), None).unwrap_err();
        assert_eq!(err, "stt HTTP 503: unavailable");
        assert_eq!(
            d_calls.lock().unwrap().len(),
            1,
            "the failed default was asked again"
        );
    }

    #[test]
    fn an_unsure_detection_without_a_prior_keeps_the_default() {
        let (r, d_calls, l_calls) = with_he_lane(Some(("he", 0.37)));
        let out = r.transcribe(b"RIFF", &auto(None), None).unwrap();
        assert_eq!(out.text, "default");
        assert_eq!(out.language, None);
        assert_eq!(d_calls.lock().unwrap().len(), 1);
        assert!(l_calls.lock().unwrap().is_empty(), "the lane was called");
    }

    #[test]
    fn a_confident_detection_beats_the_prior() {
        let (r, d_calls, l_calls) = with_he_lane(Some(("ru", 0.94)));
        let out = r.transcribe(b"RIFF", &auto(Some("he")), None).unwrap();
        assert_eq!(out.text, "default");
        assert_eq!(d_calls.lock().unwrap().len(), 1);
        assert!(l_calls.lock().unwrap().is_empty(), "the prior overrode ru");
    }

    #[test]
    fn below_the_cut_the_prior_is_sent_again() {
        let (r, _, l_calls) = with_he_lane(Some(("en", 0.2)));
        let out = r.transcribe(b"RIFF", &auto(Some("he")), None).unwrap();
        assert_eq!(out.text, "lane");
        assert_eq!(
            l_calls.lock().unwrap().as_slice(),
            [("he".to_string(), None)]
        );
    }

    #[test]
    fn with_no_detection_the_prior_is_sent_again() {
        let (r, _, l_calls) = with_he_lane(None);
        let out = r.transcribe(b"RIFF", &auto(Some("he")), None).unwrap();
        assert_eq!(out.text, "lane");
        assert_eq!(l_calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn with_no_detection_and_no_prior_the_default_stands() {
        let (r, d_calls, l_calls) = with_he_lane(None);
        let out = r.transcribe(b"RIFF", &auto(None), None).unwrap();
        assert_eq!(out.text, "default");
        assert_eq!(d_calls.lock().unwrap().len(), 1);
        assert!(l_calls.lock().unwrap().is_empty(), "the lane was called");
    }

    #[test]
    fn a_routed_empty_lane_reply_falls_back_to_the_default_with_the_language() {
        let (d, d_calls) = detecting(Ok("תודה"), Some(("he", 0.87)));
        let (l, l_calls) = fake(Ok(""));
        let r = Routed::new(Box::new(d)).lane(Lang::parse("he").unwrap(), Box::new(l), None);
        let out = r.transcribe(b"RIFF", &auto(None), None).unwrap();
        assert_eq!(out.language, Lang::parse("he"));
        assert_eq!(l_calls.lock().unwrap().len(), 1);
        assert_eq!(
            d_calls.lock().unwrap().as_slice(),
            [("auto".to_string(), None), ("he".to_string(), None)]
        );
    }

    #[test]
    fn a_routed_lane_error_is_returned() {
        let (d, d_calls) = detecting(Ok("default"), Some(("he", 0.87)));
        let (l, _) = fake(Err("stt HTTP 502: bad gateway"));
        let r = Routed::new(Box::new(d)).lane(Lang::parse("he").unwrap(), Box::new(l), None);
        assert_eq!(
            r.transcribe(b"RIFF", &auto(None), None),
            Err("stt[he] HTTP 502: bad gateway".to_string())
        );
        assert_eq!(d_calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn an_undetecting_router_sends_a_prior_straight_to_its_lane() {
        let (d, d_calls) = fake(Ok("default"));
        let (l, l_calls) = fake(Ok("lane"));
        let r = Routed::new(Box::new(d))
            .lane(Lang::parse("he").unwrap(), Box::new(l), None)
            .detecting(false);
        let out = r.transcribe(b"RIFF", &auto(Some("he")), None).unwrap();
        assert_eq!(out.text, "lane");
        assert_eq!(
            d_calls.lock().unwrap().as_slice(),
            [],
            "the default was asked first"
        );
        assert_eq!(
            l_calls.lock().unwrap().as_slice(),
            [("he".to_string(), None)]
        );
        let (d, d_calls) = fake(Ok("default"));
        let r = Routed::new(Box::new(d)).detecting(false);
        assert_eq!(
            r.transcribe(b"RIFF", &auto(None), None).unwrap().text,
            "default"
        );
        assert_eq!(d_calls.lock().unwrap().len(), 1);
    }

    fn detected(body: &str, candidates: &[&str]) -> Option<(String, f32)> {
        let srv = MockServer::start(200, body);
        let auto = SttLanguage::Auto {
            candidates: candidates.iter().map(|c| Lang::parse(c).unwrap()).collect(),
            prior: None,
        };
        client(srv.url())
            .transcribe(b"RIFF", &auto, None)
            .unwrap()
            .detected
    }

    #[test]
    fn detection_is_the_likeliest_sent_candidate() {
        let body = r#"{"text":"x","language_probabilities":{"en":0.1,"he":0.87,"haw":0.001}}"#;
        assert_eq!(
            detected(body, &["en", "ru", "he"]),
            Some(("he".to_string(), 0.87))
        );
        assert_eq!(detected(body, &["en", "ru"]), Some(("en".to_string(), 0.1)));
    }

    #[test]
    fn a_real_reply_detects_among_the_candidates_and_any_code_without_them() {
        let body = r#"{"text":"x",
            "language":"english","detected_language":"english",
            "detected_language_probability":0.2564100921154022,
            "language_probabilities":{"en":0.2564100921154022,"he":0.008125461637973785,
              "bg":0.0016347934724763036,"sl":0.7274107336997986},
            "duration":28.030000686645508}"#;
        let rounded = |d: Option<(String, f32)>| d.map(|(code, p)| (code, (p * 1e4).round()));
        assert_eq!(
            rounded(detected(body, &["en", "ru", "he"])),
            Some(("en".to_string(), 2564.0))
        );
        assert_eq!(
            rounded(detected(body, &[])),
            Some(("sl".to_string(), 7274.0))
        );
    }

    #[test]
    fn a_reply_without_language_probabilities_detects_nothing() {
        assert_eq!(
            detected(r#"{"text":"hello","language":"en"}"#, &["en", "ru"]),
            None
        );
    }

    #[test]
    fn an_explicit_request_detects_nothing() {
        let srv = MockServer::start(
            200,
            r#"{"text":"x","language_probabilities":{"ru":0.94,"en":0.03}}"#,
        );
        let out = client(srv.url()).transcribe(b"RIFF", &he(), None).unwrap();
        assert_eq!(out.language, Lang::parse("he"));
        assert_eq!(out.detected, None);
    }

    #[test]
    fn an_unscored_client_reads_no_detection() {
        let srv = MockServer::start(200, r#"{"text":"x","language_probabilities":{"he":0.87}}"#);
        let auto = SttLanguage::Auto {
            candidates: vec![Lang::parse("he").unwrap()],
            prior: None,
        };
        let out = unscored_client(srv.url())
            .transcribe(b"RIFF", &auto, None)
            .unwrap();
        assert_eq!(out.detected, None);
    }

    #[test]
    fn an_empty_text_field_is_an_empty_transcript() {
        let srv = MockServer::start(200, r#"{"text":""}"#);
        let out = client(srv.url())
            .transcribe(b"RIFF", &undetermined(), None)
            .unwrap();
        assert_eq!(out.text, "");
    }

    fn scored(body: &str) -> Transcript {
        let srv = MockServer::start(200, body);
        client(srv.url())
            .transcribe(b"RIFF", &undetermined(), None)
            .unwrap()
    }

    #[test]
    fn a_stock_phrase_over_a_silent_tail_is_dropped_on_its_weighted_mean() {
        let out = scored(
            r#"{"task":"transcribe","language":"en","duration":1.4,
                "text":" Thank you for watching! ",
                "segments":[
                  {"id":0,"start":0.0,"end":0.7,"text":"Thank you","no_speech_prob":0.04},
                  {"id":1,"start":0.7,"end":1.4,"text":" for watching!","no_speech_prob":0.74}
                ]}"#,
        );
        assert_eq!(out.text, "Thank you for watching!");
        let gate = out.no_speech_mean.unwrap();
        assert!((gate - 0.39).abs() < 1e-6, "{gate}");
        assert!(crate::pipeline::no_speech(Some(gate), 0.3).is_some());
        assert_eq!(out.no_speech_prob, Some(0.74));
    }

    #[test]
    fn a_long_dictation_with_doubtful_stretches_is_kept() {
        let segs = [
            (0.00, 4.50, 0.494),
            (4.50, 7.14, 0.494),
            (7.14, 8.40, 0.0),
            (8.40, 11.33, 0.494),
            (11.33, 13.50, 0.0),
            (13.50, 18.50, 0.494),
            (18.50, 20.30, 0.0),
            (20.30, 24.50, 0.494),
            (24.50, 27.80, 0.494),
            (27.80, 31.50, 0.0),
            (31.50, 36.00, 0.0),
            (36.00, 39.30, 0.0),
            (39.30, 42.30, 0.0),
        ]
        .iter()
        .enumerate()
        .map(|(i, (start, end, p))| {
            format!(r#"{{"id":{i},"start":{start},"end":{end},"text":"x","no_speech_prob":{p}}}"#)
        })
        .collect::<Vec<_>>()
        .join(",");
        let out = scored(&format!(r#"{{"text":"real speech","segments":[{segs}]}}"#));
        let gate = out.no_speech_mean.unwrap();
        assert!((gate - 0.264).abs() < 0.001, "{gate}");
        assert_eq!(crate::pipeline::no_speech(Some(gate), 0.3), None, "kept");
        assert_eq!(out.no_speech_prob, Some(0.494));
        assert!(
            crate::pipeline::no_speech(out.no_speech_prob, 0.08).is_some(),
            "warned"
        );
    }

    #[test]
    fn a_single_segment_at_0_54_is_dropped() {
        let out = scored(
            r#"{"text":"Thank you.","segments":[{"id":0,"start":0.0,"end":1.0,"no_speech_prob":0.54}]}"#,
        );
        let gate = out.no_speech_mean.unwrap();
        assert!((gate - 0.54).abs() < 1e-6, "{gate}");
        assert!(crate::pipeline::no_speech(Some(gate), 0.3).is_some());
    }

    #[test]
    fn segments_without_timings_fall_back_to_the_unweighted_mean() {
        for body in [
            r#"{"text":"x","segments":[{"no_speech_prob":0.1},{"no_speech_prob":0.5}]}"#,
            r#"{"text":"x","segments":[{"start":0.0,"end":9.0,"no_speech_prob":0.1},{"no_speech_prob":0.5}]}"#,
            r#"{"text":"x","segments":[{"start":"0","end":1.0,"no_speech_prob":0.1},{"start":1.0,"end":2.0,"no_speech_prob":0.5}]}"#,
            r#"{"text":"x","segments":[{"start":1.0,"end":1.0,"no_speech_prob":0.1},{"start":1.0,"end":1.0,"no_speech_prob":0.5}]}"#,
            r#"{"text":"x","segments":[{"start":1.0,"end":0.0,"no_speech_prob":0.1},{"start":0.0,"end":2.0,"no_speech_prob":0.5}]}"#,
            r#"{"text":"x","segments":[{"start":0.0,"end":1e308,"no_speech_prob":0.1},{"start":0.0,"end":1e308,"no_speech_prob":0.5}]}"#,
        ] {
            let out = scored(body);
            let gate = out.no_speech_mean.unwrap();
            assert!((gate - 0.3).abs() < 1e-6, "{body}: {gate}");
            assert_eq!(out.no_speech_prob, Some(0.5), "{body}");
        }
    }

    #[test]
    fn a_reply_without_segments_carries_no_score() {
        for body in [
            r#"{"text":"hello","language":"en"}"#,
            r#"{"text":"hello","segments":null}"#,
            r#"{"text":"hello","segments":[]}"#,
            r#"{"text":"hello","segments":[{"id":0,"text":"hello"}]}"#,
        ] {
            let srv = MockServer::start(200, body);
            let out = client(srv.url())
                .transcribe(b"RIFF", &undetermined(), None)
                .unwrap();
            assert_eq!(out.text, "hello", "{body}");
            assert_eq!(out.no_speech_prob, None, "{body}");
            assert_eq!(out.no_speech_mean, None, "{body}");
        }
    }

    #[test]
    fn a_null_score_inside_a_segment_is_still_an_error() {
        let srv = MockServer::start(
            200,
            r#"{"text":"x","segments":[{"id":0,"no_speech_prob":null}]}"#,
        );
        let err = client(srv.url())
            .transcribe(b"RIFF", &undetermined(), None)
            .unwrap_err();
        assert!(
            err.starts_with("stt response no_speech_prob is not a number: "),
            "{err}"
        );
    }

    #[test]
    fn a_non_numeric_segment_score_is_an_error() {
        let srv = MockServer::start(
            200,
            r#"{"text":"x","segments":[{"id":0,"no_speech_prob":"high"}]}"#,
        );
        let err = client(srv.url())
            .transcribe(b"RIFF", &undetermined(), None)
            .unwrap_err();
        assert!(
            err.starts_with("stt response no_speech_prob is not a number: "),
            "{err}"
        );

        let srv = MockServer::start(200, r#"{"text":"x","segments":"none"}"#);
        let err = client(srv.url())
            .transcribe(b"RIFF", &undetermined(), None)
            .unwrap_err();
        assert!(
            err.starts_with("stt response segments is not an array: "),
            "{err}"
        );
    }
}
