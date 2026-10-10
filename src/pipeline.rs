//! The dictation state machine. Knows no OS: everything platform-specific arrives as a boxed
//! trait. One `handle` call per hotkey event; it returns the outcome of a finished dictation.

use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::audio::Audio;
use crate::capture::Capture;
use crate::hotkey::{HotkeyEvent, HotkeyMode};
use crate::indicator::{Indicator, IndicatorState};
use crate::inject::{Inject, Target};
use crate::lang::{Lang, LanguagePolicy, SttLanguage};
use crate::layout::Layout;
use crate::polish::{Polisher, word_count};
use crate::stt::Transcriber;

pub struct PipelineConfig {
    /// `hotkey.mode` as configured; only seeds `Shared::mode`, which the tray owns afterwards.
    pub initial_mode: HotkeyMode,
    pub min_hold: Duration,
    pub polish_min_words: usize,
    pub prompt: Option<String>,
    pub trailing_space: bool,
    /// `inject.strip_terminal_period`: gates the one-terminal-period pop in `finish`.
    pub strip_terminal_period: bool,
    /// `polish::restores_first_i`: the polish reply's first-word "i" gets its capital back.
    /// The raw fallback keeps whisper's own casing.
    pub restore_first_i: bool,
    /// Play the warning cue when a kept transcript's `no_speech_prob` is above this; `0.0` never warns.
    pub no_speech_warn: f32,
    /// Discard a transcript whose duration-weighted mean `no_speech_prob`
    /// (`Transcript::no_speech_mean`) is above this; `0.0` keeps every one.
    /// `config::load` has already refused anything outside `0.0..=1.0`.
    pub no_speech_threshold: f32,
    /// `polish.model`, recorded on capture-log rows for which the polish stage ran.
    pub polish_model: String,
    /// Longest transcript, in characters, that may be typed; `0` lifts the limit. A longer reply
    /// is held for `byovox last` rather than truncated.
    pub max_chars: usize,
}

/// What `byovox status` / `byovox last` read; written by the pipeline thread.
#[derive(Debug)]
pub struct Shared {
    pub state: &'static str,
    /// The tray's Enable/Disable toggle. A disabled pipeline starts nothing; an event during a
    /// recording closes the microphone and discards the audio.
    pub enabled: bool,
    /// The live hotkey mode, seeded from `PipelineConfig::initial_mode` and flipped by the tray.
    pub mode: HotkeyMode,
    pub last_transcript: Option<String>,
    pub last_error: Option<String>,
}

impl Default for Shared {
    /// A fresh pipeline is idle and listening (`bool::default()` would make it deaf); the mode
    /// is a placeholder that `Pipeline::new` overwrites.
    fn default() -> Shared {
        Shared {
            state: "idle",
            enabled: true,
            mode: HotkeyMode::Hold,
            last_transcript: None,
            last_error: None,
        }
    }
}

/// One finished dictation, for the opt-in capture log.
pub struct DictationRecord<'a> {
    pub audio: &'a Audio,
    /// The keyboard layout read at press time, `None` when unreadable. `language` is the
    /// request whose reply was kept; only this says what the layout was.
    pub layout: Option<Lang>,
    pub language: &'a SttLanguage,
    /// The language whisper detected on the auto-detect request and its probability.
    pub detected: Option<(&'a str, f32)>,
    pub raw: &'a str,
    /// The strongest segment score for this clip, `None` when the server did not score it.
    /// Rows for dictations the no-speech gate dropped carry it too; the gate itself decides on
    /// the weighted mean, which the row does not carry.
    pub no_speech_prob: Option<f32>,
    pub polished: Option<&'a str>,
    /// The polish model, `None` when the stage did not run (disabled, or under min_words).
    pub polish_model: Option<&'a str>,
    pub rung: Option<&'static str>,
    /// Characters `sanitize` removed from the endpoint's reply. With `rung: None` and a non-zero
    /// count, the reply sanitised away to nothing.
    pub dropped_chars: usize,
    pub stt_ms: u128,
    pub polish_ms: u128,
    pub inject_ms: u128,
}

pub trait Recorder: Send {
    fn record(&mut self, r: &DictationRecord<'_>);
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Discarded,
    Empty,
    Inserted {
        rung: &'static str,
    },
    /// Every rung failed; the text is in `Shared::last_transcript`.
    Held,
    /// The microphone could not be closed; no audio, so no request was made.
    CaptureFailed(String),
    SttFailed(String),
}

impl Outcome {
    /// Whether the dictation reached Working and so held the pipeline thread while hotkey events
    /// queued up. `Discarded` and `CaptureFailed` are decided before the first request.
    fn reached_working(&self) -> bool {
        !matches!(self, Outcome::Discarded | Outcome::CaptureFailed(_))
    }
}

/// Drive one pipeline from its event channel until every sender is gone. `handle` runs a whole
/// dictation synchronously, so events queued behind it are dropped: obeyed afterwards they would
/// hit a state machine back at Idle, and in toggle mode leave the microphone open.
pub fn pump(pipe: &mut Pipeline, rx: &Receiver<HotkeyEvent>) {
    while let Ok(ev) = rx.recv() {
        if pipe
            .handle(ev, Instant::now())
            .is_some_and(|out| out.reached_working())
        {
            while rx.try_recv().is_ok() {}
        }
    }
}

/// The head of a stage error, for WARN and above and for `Shared::last_error`: everything
/// before the first `:`. The text after the colon can carry a response body, which can be
/// transcript; the full string goes to `debug!` only.
pub(crate) fn summary(e: &str) -> &str {
    e.split_once(':').map_or(e, |(head, _)| head).trim()
}

/// True for a character the endpoint must not put on the keyboard: controls (`Cc`: `\t` navigates, `\r` rewinds, `\x1b` opens escapes)
/// and format characters (`Cf`: bidi overrides/isolates, zero-width, tags). Bidi marks and ZWJ/ZWNJ stay for Hebrew, Arabic, emoji.
fn is_forbidden(c: char) -> bool {
    c.is_control() || (is_format(c) && !matches!(c, '\u{200C}'..='\u{200F}'))
}

/// Unicode 16.0's general category `Cf`, from `DerivedGeneralCategory.txt` (std has no lookup).
fn is_format(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}'
            | '\u{0600}'..='\u{0605}'
            | '\u{061C}'
            | '\u{06DD}'
            | '\u{070F}'
            | '\u{0890}'..='\u{0891}'
            | '\u{08E2}'
            | '\u{180E}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{206F}'
            | '\u{FEFF}'
            | '\u{FFF9}'..='\u{FFFB}'
            | '\u{110BD}'
            | '\u{110CD}'
            | '\u{13430}'..='\u{1343F}'
            | '\u{1BCA0}'..='\u{1BCA3}'
            | '\u{1D173}'..='\u{1D17A}'
            | '\u{E0001}'
            | '\u{E0020}'..='\u{E007F}'
    )
}

/// The transcript with every `\n`, U+2028 and U+2029 turned into a space and everything
/// `is_forbidden` names removed. The single choke point before the text is typed: `type`,
/// `paste` and `clipboard-only` are all covered, since a pasted newline can submit a line too.
pub fn sanitize(text: &str) -> String {
    text.chars()
        .map(|c| {
            if matches!(c, '\n' | '\u{2028}' | '\u{2029}') {
                ' '
            } else {
                c
            }
        })
        .filter(|c| !is_forbidden(*c))
        .collect()
}

/// The score that condemns a transcript as silence: `Some(p)` exactly when the gate drops it.
/// A threshold of `0` gates nothing and an unscored reply is never gated. The one place the
/// comparison is written, on the same `f32` the daemon narrows to, so `byovox check` agrees.
pub fn no_speech(prob: Option<f32>, threshold: f32) -> Option<f32> {
    prob.filter(|p| threshold > 0.0 && *p > threshold)
}

enum State {
    Idle,
    Recording {
        since: Instant,
        /// Read at press time, kept beside the language it resolved to; unreadable again by
        /// the time the capture row is written.
        layout: Option<Lang>,
        language: SttLanguage,
        /// The window the press was made in (`Target::current`): the text goes there, not
        /// wherever focus has moved.
        window: Option<isize>,
    },
}

pub struct Pipeline {
    pub cfg: PipelineConfig,
    capture: Box<dyn Capture>,
    layout: Box<dyn Layout>,
    policy: LanguagePolicy,
    stt: Box<dyn Transcriber>,
    polish: Option<Box<dyn Polisher>>,
    rungs: Vec<Box<dyn Inject>>,
    target: Box<dyn Target>,
    indicator: Box<dyn Indicator>,
    recorder: Option<Box<dyn Recorder>>,
    shared: Arc<Mutex<Shared>>,
    state: State,
}

impl Pipeline {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cfg: PipelineConfig,
        capture: Box<dyn Capture>,
        layout: Box<dyn Layout>,
        policy: LanguagePolicy,
        stt: Box<dyn Transcriber>,
        polish: Option<Box<dyn Polisher>>,
        rungs: Vec<Box<dyn Inject>>,
        target: Box<dyn Target>,
        indicator: Box<dyn Indicator>,
        recorder: Option<Box<dyn Recorder>>,
    ) -> Pipeline {
        let shared = Arc::new(Mutex::new(Shared {
            mode: cfg.initial_mode,
            ..Default::default()
        }));
        Pipeline {
            cfg,
            capture,
            layout,
            policy,
            stt,
            polish,
            rungs,
            target,
            indicator,
            recorder,
            shared,
            state: State::Idle,
        }
    }

    pub fn shared(&self) -> Arc<Mutex<Shared>> {
        self.shared.clone()
    }

    pub fn handle(&mut self, ev: HotkeyEvent, now: Instant) -> Option<Outcome> {
        let recording = matches!(self.state, State::Recording { .. });
        let (enabled, mode) = {
            let shared = self.shared.lock().unwrap();
            (shared.enabled, shared.mode)
        };
        // A disabled pipeline never leaves the microphone open: the first event during a
        // recording closes it and drops the audio.
        if !enabled {
            if recording {
                let _ = self.capture.stop();
                self.set_state(State::Idle, IndicatorState::Idle);
                tracing::info!("dictation discarded: byovox was disabled mid-recording");
                return Some(Outcome::Discarded);
            }
            return None;
        }
        match (ev, mode, recording) {
            (HotkeyEvent::Pressed, HotkeyMode::Hold, false)
            | (HotkeyEvent::Pressed, HotkeyMode::Toggle, false)
            | (HotkeyEvent::Toggle, _, false) => {
                self.begin(now);
                None
            }
            (HotkeyEvent::Released, HotkeyMode::Hold, true)
            | (HotkeyEvent::Pressed, HotkeyMode::Toggle, true)
            | (HotkeyEvent::Toggle, _, true) => Some(self.finish(now)),
            (HotkeyEvent::Cancel, _, true) => {
                let _ = self.capture.stop();
                self.set_state(State::Idle, IndicatorState::Idle);
                tracing::info!("dictation cancelled");
                Some(Outcome::Discarded)
            }
            _ => None,
        }
    }

    fn begin(&mut self, now: Instant) {
        let layout = self.layout.current();
        let language = self.policy.resolve(layout);
        if let Err(e) = self.capture.start() {
            tracing::debug!(error = %e, "microphone open error");
            tracing::error!(error = summary(&e), "microphone open failed");
            self.fail(e);
            return;
        }
        // After the microphone: reading the layout can ask the focused app over UI Automation,
        // and the first syllable must not wait on it.
        let window = self.target.current();
        self.set_state(
            State::Recording {
                since: now,
                layout,
                language,
                window,
            },
            IndicatorState::Recording,
        );
    }

    fn finish(&mut self, now: Instant) -> Outcome {
        let State::Recording {
            since,
            layout,
            language,
            window,
        } = std::mem::replace(&mut self.state, State::Idle)
        else {
            return Outcome::Discarded;
        };
        let audio = match self.capture.stop() {
            Ok(a) => a,
            Err(e) => {
                tracing::debug!(error = %e, "microphone stop error");
                tracing::error!(error = summary(&e), "microphone stop failed");
                self.fail(e.clone());
                return Outcome::CaptureFailed(e);
            }
        };
        if now.duration_since(since) < self.cfg.min_hold {
            self.set_state(State::Idle, IndicatorState::Idle);
            tracing::info!("tap discarded");
            return Outcome::Discarded;
        }
        // A hold just over `min_hold` on a device that opens with the click leaves no samples,
        // and whisper answers an empty WAV with HTTP 400. Not asked; `Discarded`, decided before
        // any request, so `pump` must not drop the press queued behind it.
        if audio.samples.is_empty() {
            self.set_state(State::Idle, IndicatorState::Idle);
            tracing::info!("empty capture discarded");
            return Outcome::Discarded;
        }
        // A muted or dead microphone delivers digital zero, and whisper can type a stock phrase
        // over it with a no-speech score the gate trusts.
        if audio.is_silent() {
            self.set_state(State::Idle, IndicatorState::Idle);
            tracing::info!("silent capture discarded");
            return Outcome::Discarded;
        }
        self.indicator.set(IndicatorState::Working);
        self.shared.lock().unwrap().state = "working";

        let wav = audio.to_wav();
        let t = Instant::now();
        let transcript = match self
            .stt
            .transcribe(&wav, &language, self.cfg.prompt.as_deref())
        {
            Ok(t) => t,
            Err(e) => {
                tracing::debug!(error = %e, "stt error");
                tracing::error!(error = summary(&e), "stt failed");
                self.fail(e.clone());
                return Outcome::SttFailed(e);
            }
        };
        let stt_ms = t.elapsed().as_millis();
        // From here on `language` names the request whose reply was kept.
        let language = transcript.language.map_or(language, SttLanguage::Explicit);
        let detected = transcript
            .detected
            .as_ref()
            .map(|(code, p)| (code.as_str(), *p));
        let no_speech_prob = transcript.no_speech_prob;
        let no_speech_mean = transcript.no_speech_mean;
        let raw = transcript.text;
        tracing::debug!(raw = %raw, "transcript");
        if raw.trim().is_empty() {
            self.set_state(State::Idle, IndicatorState::Idle);
            tracing::info!(lang = %language.label(), stt_ms, "empty transcript");
            return Outcome::Empty;
        }
        // Over near-silence whisper invents a stock phrase and reports its own doubt: the duration-weighted mean decides, never the text.
        // The row is still written (the threshold is tuned from it); nothing else happens, so `byovox last` never returns undictated text.
        if let Some(p) = no_speech(no_speech_mean, self.cfg.no_speech_threshold) {
            if let Some(rec) = &mut self.recorder {
                rec.record(&DictationRecord {
                    audio: &audio,
                    layout,
                    language: &language,
                    detected,
                    raw: &raw,
                    no_speech_prob,
                    polished: None,
                    polish_model: None,
                    rung: None,
                    dropped_chars: 0,
                    stt_ms,
                    polish_ms: 0,
                    inject_ms: 0,
                });
            }
            self.set_state(State::Idle, IndicatorState::Idle);
            tracing::info!(p = %format!("{p:.2}"), lang = %language.label(), stt_ms, "no speech detected");
            return Outcome::Empty;
        }

        let mut errored = false;
        let polisher = self
            .polish
            .as_ref()
            .filter(|_| word_count(&raw) >= self.cfg.polish_min_words);
        // Named on the row only when the stage ran: a failed polish still used the model.
        let polish_model = polisher.is_some().then_some(self.cfg.polish_model.as_str());
        let t = Instant::now();
        let polished = match polisher {
            Some(p) => match p.polish(&raw) {
                Ok(text) if self.cfg.restore_first_i => Some(crate::polish::restore_first_i(&text)),
                Ok(text) => Some(text),
                Err(e) => {
                    tracing::debug!(error = %e, "polish error");
                    tracing::warn!(
                        error = summary(&e),
                        "polish failed; inserting raw transcript"
                    );
                    self.shared.lock().unwrap().last_error = Some(summary(&e).to_string());
                    errored = true;
                    None
                }
            },
            None => None,
        };
        let polish_ms = t.elapsed().as_millis();
        tracing::debug!(polished = ?polished, "polished");

        // The last point the endpoint's text is still just data: past here it is keystrokes in
        // someone's window. The count is logged and the text is not.
        let served = polished.clone().unwrap_or_else(|| raw.clone());
        let sanitized = sanitize(&served);
        let dropped = served.chars().count() - sanitized.chars().count();
        // Counted before the trim: a reply ending in a newline now ends in a space, which is
        // not a dropped character to warn about.
        let mut text = sanitized.trim().to_string();
        // A dictation usually lands mid-sentence, so exactly one terminal `.` goes. An ellipsis
        // stays (the `..` guard), and `?`/`!` were asked for by tone.
        if self.cfg.strip_terminal_period && text.ends_with('.') && !text.ends_with("..") {
            text.pop();
            // the pop can expose whitespace ("Hello .") — never type a stray trailing space
            text.truncate(text.trim_end().len());
        }
        if dropped > 0 {
            tracing::warn!(
                dropped,
                "removed control or format characters the endpoint returned"
            );
        }
        // A reply of only forbidden characters or a lone period is the empty transcript and not held for `byovox last`;
        // it is recorded first: the row carries the raw text and `dropped_chars`, the evidence of a hostile endpoint.
        if text.trim().is_empty() {
            if let Some(rec) = &mut self.recorder {
                rec.record(&DictationRecord {
                    audio: &audio,
                    layout,
                    language: &language,
                    detected,
                    raw: &raw,
                    no_speech_prob,
                    polished: polished.as_deref(),
                    polish_model,
                    rung: None,
                    dropped_chars: dropped,
                    stt_ms,
                    polish_ms,
                    inject_ms: 0,
                });
            }
            // A polish failure already set `last_error`; going to Idle here would hide it.
            let final_state = if errored {
                IndicatorState::Error
            } else {
                IndicatorState::Idle
            };
            self.set_state(State::Idle, final_state);
            tracing::info!(lang = %language.label(), stt_ms, dropped, strip_terminal_period = self.cfg.strip_terminal_period, "empty transcript after sanitising");
            return Outcome::Empty;
        }
        if self.cfg.trailing_space {
            text.push(' ');
        }
        self.shared.lock().unwrap().last_transcript = Some(text.clone());

        // A reply over the cap is neither typed nor truncated (half a dictation would be inserted): it takes the all-rungs-failed path,
        // held in `Shared::last_transcript` with the error indicator. The length is logged, never the text.
        let over_cap = self.cfg.max_chars > 0 && text.chars().count() > self.cfg.max_chars;
        if over_cap {
            tracing::warn!(
                chars = text.chars().count(),
                max_chars = self.cfg.max_chars,
                "transcript over inject.max_chars; not typed, held for `byovox last`"
            );
        }
        let t = Instant::now();
        // The text goes back to the window the dictation started in, or, when nothing was focused at press, follows focus
        // but only into something that takes text. Failing either, no rung runs, not even clipboard-only, whose done cue would lie.
        let refocused = match window {
            _ if over_cap => Ok(()),
            Some(w) => self.target.restore(w),
            None if self.target.accepts_text() => Ok(()),
            None => Err("nothing was focused at press and no text field has focus now".to_string()),
        };
        if let Err(e) = &refocused {
            tracing::warn!(error = %e, "no window to type the dictation into");
        }
        let mut rung_used = None;
        if !over_cap && refocused.is_ok() {
            for rung in &mut self.rungs {
                match rung.inject(&text) {
                    Ok(()) => {
                        rung_used = Some(rung.name());
                        break;
                    }
                    Err(e) => {
                        tracing::debug!(rung = rung.name(), error = %e, "inject rung error");
                        tracing::warn!(
                            rung = rung.name(),
                            error = summary(&e),
                            "inject rung failed"
                        );
                    }
                }
            }
        }
        let inject_ms = t.elapsed().as_millis();

        if let Some(rec) = &mut self.recorder {
            rec.record(&DictationRecord {
                audio: &audio,
                layout,
                language: &language,
                detected,
                raw: &raw,
                no_speech_prob,
                polished: polished.as_deref(),
                polish_model,
                rung: rung_used,
                dropped_chars: dropped,
                stt_ms,
                polish_ms,
                inject_ms,
            });
        }

        let total_ms = stt_ms + polish_ms + inject_ms;
        match rung_used {
            Some(rung) => {
                tracing::info!(lang = %language.label(), stt_ms, polish_ms, inject_ms, total_ms, rung, "dictation inserted");
                // The one path that earns the done cue; every other route back to Idle is silent.
                // A segment above `no_speech_warn` still types, with the "proofread me" cue.
                let uncertain = no_speech(no_speech_prob, self.cfg.no_speech_warn).is_some();
                let final_state = if errored {
                    IndicatorState::Error
                } else if uncertain {
                    tracing::warn!(
                        no_speech_prob = no_speech_prob.unwrap_or_default(),
                        "kept transcript scored above stt.no_speech_warn; warning cue played"
                    );
                    IndicatorState::Uncertain
                } else {
                    IndicatorState::Done
                };
                self.set_state(State::Idle, final_state);
                Outcome::Inserted { rung }
            }
            None => {
                let why = if over_cap {
                    "transcript over inject.max_chars; run `byovox last`".to_string()
                } else if let Err(e) = refocused {
                    format!("{e}; run `byovox last`")
                } else {
                    "no inject rung worked; run `byovox last`".to_string()
                };
                tracing::warn!(lang = %language.label(), stt_ms, polish_ms, inject_ms, total_ms, why, "transcript held for `byovox last`");
                self.fail(why);
                Outcome::Held
            }
        }
    }

    fn fail(&mut self, error: String) {
        self.shared.lock().unwrap().last_error = Some(summary(&error).to_string());
        self.set_state(State::Idle, IndicatorState::Error);
    }

    fn set_state(&mut self, state: State, ind: IndicatorState) {
        self.shared.lock().unwrap().state = match state {
            State::Idle => "idle",
            State::Recording { .. } => "recording",
        };
        self.state = state;
        self.indicator.set(ind);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::LanguageConfig;
    use crate::hotkey::HotkeyMode;
    use crate::indicator::IndicatorState as S;
    use crate::lang::Lang;
    use crate::stt::Transcript;
    use crate::testutil::fakes::*;
    use crate::testutil::logged;
    use std::time::{Duration, Instant};

    struct Rig {
        p: Pipeline,
        cap: FakeCapture,
        stt: FakeTranscriber,
        polish: FakePolisher,
        rung1: FakeInject,
        rung2: FakeInject,
        target: FakeTarget,
        ind: FakeIndicator,
        rec: FakeRecorder,
    }

    fn rig(
        stt: FakeTranscriber,
        polish: Option<FakePolisher>,
        rung1_fails: bool,
        rung2_fails: bool,
    ) -> Rig {
        rig_with_capture(
            FakeCapture::new(16_000),
            stt,
            polish,
            rung1_fails,
            rung2_fails,
        )
    }

    fn rig_with_capture(
        cap: FakeCapture,
        stt: FakeTranscriber,
        polish: Option<FakePolisher>,
        rung1_fails: bool,
        rung2_fails: bool,
    ) -> Rig {
        let lc = LanguageConfig {
            candidates: vec!["en".into(), "ru".into()],
            by_layout: [("he".to_string(), "he".to_string())].into(),
            ..Default::default()
        };
        let policy = LanguagePolicy::from_config(&lc, &[]).unwrap();
        let polish_fake = polish.clone().unwrap_or_else(|| FakePolisher::ok("unused"));
        let rung1 = FakeInject::new("type", rung1_fails);
        let rung2 = FakeInject::new("paste", rung2_fails);
        let target = FakeTarget {
            window: Some(7),
            ..Default::default()
        };
        let ind = FakeIndicator::default();
        let rec = FakeRecorder::default();
        let cfg = PipelineConfig {
            initial_mode: HotkeyMode::Hold,
            min_hold: Duration::from_millis(250),
            polish_min_words: 0,
            prompt: Some("Glossary: Acme".into()),
            trailing_space: false,
            strip_terminal_period: true,
            restore_first_i: false,
            polish_model: "cleanup-1".into(),
            max_chars: 20_000,
            no_speech_threshold: 0.6,
            no_speech_warn: 0.0,
        };
        let p = Pipeline::new(
            cfg,
            Box::new(cap.clone()),
            Box::new(FakeLayout(Lang::parse("he"))),
            policy,
            Box::new(stt.clone()),
            polish.clone().map(|f| Box::new(f) as Box<dyn Polisher>),
            vec![Box::new(rung1.clone()), Box::new(rung2.clone())],
            Box::new(target.clone()),
            Box::new(ind.clone()),
            Some(Box::new(rec.clone())),
        );
        Rig {
            p,
            cap,
            stt,
            polish: polish_fake,
            rung1,
            rung2,
            target,
            ind,
            rec,
        }
    }

    fn dictate(r: &mut Rig, hold: Duration) -> Option<Outcome> {
        let t0 = Instant::now();
        assert!(r.p.handle(HotkeyEvent::Pressed, t0).is_none());
        r.p.handle(HotkeyEvent::Released, t0 + hold)
    }

    #[test]
    fn happy_path_uses_layout_prompt_polish_and_first_rung() {
        let mut r = rig(
            FakeTranscriber::ok("um hello"),
            Some(FakePolisher::ok("Hello.")),
            false,
            false,
        );
        let out = dictate(&mut r, Duration::from_millis(600));
        assert_eq!(out, Some(Outcome::Inserted { rung: "type" }));
        let calls = r.stt.calls.lock().unwrap();
        assert_eq!(
            calls[0].0,
            vec![("language_candidates", "en,ru".to_string())]
        );
        assert_eq!(calls[0].1.as_deref(), Some("Glossary: Acme"));
        assert_eq!(r.polish.calls.lock().unwrap().as_slice(), ["um hello"]);
        assert_eq!(r.rung1.texts.lock().unwrap().as_slice(), ["Hello"]);
        assert!(r.rung2.texts.lock().unwrap().is_empty());
        assert_eq!(
            r.ind.0.lock().unwrap().as_slice(),
            [S::Recording, S::Working, S::Done]
        );
        assert_eq!((r.cap.starts(), r.cap.stops()), (1, 1));
    }

    #[test]
    fn a_polished_list_is_typed_on_one_line_without_a_stray_space() {
        let mut r = rig(
            FakeTranscriber::ok("first milk second eggs"),
            Some(FakePolisher::ok("1. milk\n2. eggs\n")),
            false,
            false,
        );
        assert_eq!(
            dictate(&mut r, Duration::from_millis(600)),
            Some(Outcome::Inserted { rung: "type" })
        );
        assert_eq!(
            r.rung1.texts.lock().unwrap().as_slice(),
            ["1. milk 2. eggs"]
        );
    }

    #[test]
    fn tap_under_min_hold_is_discarded_without_a_request() {
        let mut r = rig(FakeTranscriber::ok("x"), None, false, false);
        assert_eq!(
            dictate(&mut r, Duration::from_millis(100)),
            Some(Outcome::Discarded)
        );
        assert!(r.stt.calls.lock().unwrap().is_empty());
        assert_eq!(r.cap.stops(), 1);
    }

    #[test]
    fn polish_failure_inserts_raw() {
        let mut r = rig(
            FakeTranscriber::ok("raw words"),
            Some(FakePolisher::err("polish HTTP 500: SECRET")),
            false,
            false,
        );
        assert_eq!(
            dictate(&mut r, Duration::from_secs(1)),
            Some(Outcome::Inserted { rung: "type" })
        );
        assert_eq!(r.rung1.texts.lock().unwrap().as_slice(), ["raw words"]);
        assert_eq!(r.ind.0.lock().unwrap().last(), Some(&S::Error));
        assert_eq!(
            r.p.shared().lock().unwrap().last_error.as_deref(),
            Some("polish HTTP 500")
        );
    }

    #[test]
    fn stt_failure_inserts_nothing_and_records_error() {
        let mut r = rig(FakeTranscriber::err("boom"), None, false, false);
        assert_eq!(
            dictate(&mut r, Duration::from_secs(1)),
            Some(Outcome::SttFailed("boom".into()))
        );
        assert!(r.rung1.texts.lock().unwrap().is_empty());
        assert_eq!(
            r.p.shared().lock().unwrap().last_error.as_deref(),
            Some("boom")
        );
        assert_eq!(r.ind.0.lock().unwrap().last(), Some(&S::Error));
    }

    #[test]
    fn last_error_keeps_only_the_summary_so_the_tray_cannot_echo_a_response_body() {
        let mut r = rig(
            FakeTranscriber::err("stt HTTP 500: SECRET"),
            None,
            false,
            false,
        );
        assert_eq!(
            dictate(&mut r, Duration::from_secs(1)),
            Some(Outcome::SttFailed("stt HTTP 500: SECRET".into())),
            "the outcome still carries the whole error for the caller"
        );
        assert_eq!(
            r.p.shared().lock().unwrap().last_error.as_deref(),
            Some("stt HTTP 500")
        );
    }

    #[test]
    fn empty_transcript_is_quietly_idle() {
        let mut r = rig(FakeTranscriber::ok("   "), None, false, false);
        assert_eq!(
            dictate(&mut r, Duration::from_secs(1)),
            Some(Outcome::Empty)
        );
        assert_eq!(r.ind.0.lock().unwrap().last(), Some(&S::Idle));
    }

    #[test]
    fn a_tap_a_cancel_and_an_empty_transcript_are_silent() {
        let mut tap = rig(FakeTranscriber::ok("x"), None, false, false);
        dictate(&mut tap, Duration::from_millis(100));

        let mut cancelled = rig(FakeTranscriber::ok("hi"), None, false, false);
        let t0 = Instant::now();
        cancelled.p.handle(HotkeyEvent::Pressed, t0);
        cancelled
            .p
            .handle(HotkeyEvent::Cancel, t0 + Duration::from_secs(1));

        let mut empty = rig(FakeTranscriber::ok("   "), None, false, false);
        dictate(&mut empty, Duration::from_secs(1));

        for (what, r) in [("tap", &tap), ("cancel", &cancelled), ("empty", &empty)] {
            let states = r.ind.0.lock().unwrap().clone();
            assert!(
                !states.contains(&S::Done),
                "{what} would play the done cue: {states:?}"
            );
        }
    }

    #[test]
    fn a_transcript_whisper_scored_as_silence_is_dropped() {
        let mut r = rig(
            FakeTranscriber::scored("Thank you for watching!", 0.75),
            Some(FakePolisher::ok("Thank you for watching.")),
            false,
            false,
        );
        assert_eq!(
            dictate(&mut r, Duration::from_secs(1)),
            Some(Outcome::Empty)
        );
        assert!(r.polish.calls.lock().unwrap().is_empty());
        assert!(r.rung1.texts.lock().unwrap().is_empty());
        assert!(r.rung2.texts.lock().unwrap().is_empty());
        assert!(
            r.p.shared().lock().unwrap().last_transcript.is_none(),
            "`byovox last` would hand back something that was never dictated"
        );
        assert_eq!(r.ind.0.lock().unwrap().last(), Some(&S::Idle));
        let recs = r.rec.0.lock().unwrap();
        assert_eq!(recs.len(), 1, "the corpus keeps the evidence");
        assert_eq!(recs[0].raw, "Thank you for watching!");
        assert_eq!(recs[0].no_speech_prob, Some(0.75));
        assert_eq!(recs[0].polished, None);
        assert_eq!(recs[0].rung, None);
    }

    #[test]
    fn only_a_score_above_the_threshold_gates() {
        for (what, stt, threshold) in [
            (
                "the stock phrase itself, when whisper scores it as speech",
                FakeTranscriber::scored("Thank you for watching!", 0.04),
                0.6,
            ),
            (
                "an unscored server leaves nothing to judge",
                FakeTranscriber::ok("Thank you for watching!"),
                0.6,
            ),
            (
                "exactly at the threshold is not above it",
                FakeTranscriber::scored("hello there", 0.6),
                0.6,
            ),
            (
                "0 turns the gate off",
                FakeTranscriber::scored("Thank you for watching!", 0.99),
                0.0,
            ),
        ] {
            let mut r = rig(stt, None, false, false);
            r.p.cfg.no_speech_threshold = threshold;
            assert_eq!(
                dictate(&mut r, Duration::from_secs(1)),
                Some(Outcome::Inserted { rung: "type" }),
                "{what}"
            );
            assert_eq!(r.rung1.texts.lock().unwrap().len(), 1, "{what}");
        }
    }

    #[test]
    fn the_same_words_are_dropped_or_inserted_by_their_score_alone() {
        let words = "Thank you for watching!";
        let outcome = |p: f32| {
            let mut r = rig(FakeTranscriber::scored(words, p), None, false, false);
            r.p.cfg.no_speech_threshold = 0.6;
            let out = dictate(&mut r, Duration::from_secs(1));
            (out, r.rung1.texts.lock().unwrap().clone())
        };
        assert_eq!(outcome(0.75), (Some(Outcome::Empty), vec![]));
        assert_eq!(
            outcome(0.04),
            (
                Some(Outcome::Inserted { rung: "type" }),
                vec![words.to_string()]
            )
        );
    }

    #[test]
    fn an_out_of_band_score_types_the_text_and_warns() {
        for (what, p, warn, expect) in [
            ("gray zone warns", 0.19_f32, 0.08_f32, S::Uncertain),
            ("in-band is done", 0.01, 0.08, S::Done),
            ("0 turns the warning off", 0.19, 0.0, S::Done),
        ] {
            let mut r = rig(
                FakeTranscriber::scored("Травма на всю жизнь", p),
                None,
                false,
                false,
            );
            r.p.cfg.no_speech_warn = warn;
            assert_eq!(
                dictate(&mut r, Duration::from_secs(1)),
                Some(Outcome::Inserted { rung: "type" }),
                "{what}"
            );
            assert_eq!(
                r.rung1.texts.lock().unwrap().len(),
                1,
                "{what}: the text still types"
            );
            assert_eq!(r.ind.0.lock().unwrap().last(), Some(&expect), "{what}");
        }
        let mut r = rig(FakeTranscriber::ok("hello"), None, false, false);
        r.p.cfg.no_speech_warn = 0.08;
        dictate(&mut r, Duration::from_secs(1));
        assert_eq!(r.ind.0.lock().unwrap().last(), Some(&S::Done));
    }

    #[test]
    fn the_drop_reads_the_mean_and_the_cue_reads_the_max() {
        let mut r = rig(
            FakeTranscriber::scored_segments("real speech", 0.264, 0.494),
            None,
            false,
            false,
        );
        r.p.cfg.no_speech_threshold = 0.3;
        r.p.cfg.no_speech_warn = 0.08;
        assert_eq!(
            dictate(&mut r, Duration::from_secs(1)),
            Some(Outcome::Inserted { rung: "type" })
        );
        assert_eq!(r.ind.0.lock().unwrap().last(), Some(&S::Uncertain));
        assert_eq!(r.rec.0.lock().unwrap()[0].no_speech_prob, Some(0.494));
    }

    #[test]
    fn the_gated_line_carries_the_probability_and_no_text() {
        let mut r = rig(
            FakeTranscriber::scored("Thank you for watching!", 0.75),
            None,
            false,
            false,
        );
        let log = logged(|| {
            assert_eq!(
                dictate(&mut r, Duration::from_secs(1)),
                Some(Outcome::Empty)
            );
        });
        assert!(log.contains("no speech detected"), "{log}");
        assert!(log.contains("p=0.75"), "{log}");
        assert!(log.contains("lang=auto"), "{log}");
        assert!(!log.contains("Thank you"), "{log}");
    }

    #[test]
    fn a_routed_dictation_is_logged_and_recorded_as_its_lane() {
        let stt = FakeTranscriber {
            result: Ok(Transcript {
                text: "שלום".into(),
                no_speech_prob: None,
                no_speech_mean: None,
                language: Lang::parse("he"),
                detected: Some(("he".into(), 0.87)),
            }),
            calls: Default::default(),
        };
        let mut r = rig(stt, None, false, false);
        let log = logged(|| {
            dictate(&mut r, Duration::from_secs(1));
        });
        assert!(log.contains("lang=he"), "{log}");
        let recs = r.rec.0.lock().unwrap();
        assert_eq!(recs[0].language, "he");
        assert_eq!(recs[0].detected.as_deref(), Some("he"));
        assert_eq!(recs[0].detected_p, Some(0.87));
    }

    #[test]
    fn an_unrouted_dictation_is_logged_and_recorded_as_auto() {
        let mut r = rig(FakeTranscriber::ok("hi there"), None, false, false);
        let log = logged(|| {
            dictate(&mut r, Duration::from_secs(1));
        });
        assert!(log.contains("lang=auto"), "{log}");
        let recs = r.rec.0.lock().unwrap();
        assert_eq!(recs[0].language, "auto");
        assert_eq!(recs[0].detected, None);
    }

    #[test]
    fn first_rung_failing_falls_to_second() {
        let mut r = rig(FakeTranscriber::ok("hi there"), None, true, false);
        assert_eq!(
            dictate(&mut r, Duration::from_secs(1)),
            Some(Outcome::Inserted { rung: "paste" })
        );
        assert_eq!(r.rung2.texts.lock().unwrap().as_slice(), ["hi there"]);
    }

    #[test]
    fn all_rungs_failing_holds_for_last() {
        let mut r = rig(FakeTranscriber::ok("hi there"), None, true, true);
        assert_eq!(dictate(&mut r, Duration::from_secs(1)), Some(Outcome::Held));
        assert_eq!(
            r.p.shared().lock().unwrap().last_transcript.as_deref(),
            Some("hi there")
        );
        assert_eq!(r.ind.0.lock().unwrap().last(), Some(&S::Error));
    }

    #[test]
    fn the_text_goes_back_to_the_window_dictated_into() {
        let mut r = rig(FakeTranscriber::ok("hi there"), None, false, false);
        assert_eq!(
            dictate(&mut r, Duration::from_secs(1)),
            Some(Outcome::Inserted { rung: "type" })
        );
        assert_eq!(r.target.restored.lock().unwrap().as_slice(), [7]);
    }

    #[test]
    fn a_closed_window_types_nothing_and_holds_for_last() {
        let mut r = rig(FakeTranscriber::ok("hi there"), None, false, false);
        r.target.closed = true;
        r.p.target = Box::new(r.target.clone());
        assert_eq!(dictate(&mut r, Duration::from_secs(1)), Some(Outcome::Held));
        assert!(r.rung1.texts.lock().unwrap().is_empty());
        assert!(r.rung2.texts.lock().unwrap().is_empty());
        let shared = r.p.shared();
        let shared = shared.lock().unwrap();
        assert_eq!(shared.last_transcript.as_deref(), Some("hi there"));
        assert!(shared.last_error.as_deref().unwrap().contains("window"));
        assert_eq!(r.ind.0.lock().unwrap().last(), Some(&S::Error));
    }

    #[test]
    fn with_nothing_focused_at_press_the_text_follows_focus_into_a_field() {
        let mut r = rig(FakeTranscriber::ok("hi there"), None, false, false);
        r.target.window = None;
        r.target.accepts = true;
        r.p.target = Box::new(r.target.clone());
        assert_eq!(
            dictate(&mut r, Duration::from_secs(1)),
            Some(Outcome::Inserted { rung: "type" })
        );
        assert!(r.target.restored.lock().unwrap().is_empty());
    }

    #[test]
    fn with_nothing_focused_at_press_and_no_field_now_the_text_is_held() {
        let mut r = rig(FakeTranscriber::ok("hi there"), None, false, false);
        r.target.window = None;
        r.p.target = Box::new(r.target.clone());
        assert_eq!(dictate(&mut r, Duration::from_secs(1)), Some(Outcome::Held));
        assert!(r.rung1.texts.lock().unwrap().is_empty());
        assert!(r.rung2.texts.lock().unwrap().is_empty());
        let shared = r.p.shared();
        let shared = shared.lock().unwrap();
        assert_eq!(shared.last_transcript.as_deref(), Some("hi there"));
        assert!(shared.last_error.as_deref().unwrap().contains("text field"));
        assert_eq!(r.ind.0.lock().unwrap().last(), Some(&S::Error));
    }

    #[test]
    fn min_words_skips_polish() {
        let mut r = rig(
            FakeTranscriber::ok("two words"),
            Some(FakePolisher::ok("X")),
            false,
            false,
        );
        r.p.cfg.polish_min_words = 3;
        dictate(&mut r, Duration::from_secs(1));
        assert!(r.polish.calls.lock().unwrap().is_empty());
        assert_eq!(r.rung1.texts.lock().unwrap().as_slice(), ["two words"]);
        assert_eq!(r.rec.0.lock().unwrap()[0].polish_model, None);
    }

    #[test]
    fn toggle_mode_starts_and_stops_on_toggle() {
        let mut r = rig(FakeTranscriber::ok("hi"), None, false, false);
        r.p.shared().lock().unwrap().mode = HotkeyMode::Toggle;
        let t0 = Instant::now();
        assert!(r.p.handle(HotkeyEvent::Toggle, t0).is_none());
        assert!(
            r.p.handle(HotkeyEvent::Released, t0).is_none(),
            "release ignored in toggle mode"
        );
        assert_eq!(
            r.p.handle(HotkeyEvent::Toggle, t0 + Duration::from_secs(1)),
            Some(Outcome::Inserted { rung: "type" })
        );
    }

    #[test]
    fn cancel_while_recording_discards() {
        let mut r = rig(FakeTranscriber::ok("hi"), None, false, false);
        let t0 = Instant::now();
        r.p.handle(HotkeyEvent::Pressed, t0);
        assert_eq!(
            r.p.handle(HotkeyEvent::Cancel, t0 + Duration::from_secs(1)),
            Some(Outcome::Discarded)
        );
        assert!(r.stt.calls.lock().unwrap().is_empty());
        assert_eq!(r.cap.stops(), 1);
    }

    #[test]
    fn repeat_press_while_recording_is_ignored() {
        let mut r = rig(FakeTranscriber::ok("hi"), None, false, false);
        let t0 = Instant::now();
        r.p.handle(HotkeyEvent::Pressed, t0);
        assert!(
            r.p.handle(HotkeyEvent::Pressed, t0 + Duration::from_millis(30))
                .is_none()
        );
        assert_eq!(
            r.p.handle(HotkeyEvent::Released, t0 + Duration::from_secs(1)),
            Some(Outcome::Inserted { rung: "type" })
        );
    }

    #[test]
    fn the_terminal_period_survives_when_the_strip_is_off() {
        for (served, typed) in [("Hello.", "Hello."), ("Wait...", "Wait...")] {
            let mut r = rig(
                FakeTranscriber::ok("spoken words"),
                Some(FakePolisher::ok(served)),
                false,
                false,
            );
            r.p.cfg.strip_terminal_period = false;
            dictate(&mut r, Duration::from_secs(1));
            assert_eq!(r.rung1.texts.lock().unwrap().as_slice(), [typed]);
        }
    }

    #[test]
    fn a_polished_first_word_i_is_typed_as_i_only_with_the_flag() {
        for (flag, typed) in [(true, "I think"), (false, "i think")] {
            let mut r = rig(
                FakeTranscriber::ok("spoken words"),
                Some(FakePolisher::ok("i think")),
                false,
                false,
            );
            r.p.cfg.restore_first_i = flag;
            dictate(&mut r, Duration::from_secs(1));
            assert_eq!(
                r.rung1.texts.lock().unwrap().as_slice(),
                [typed],
                "flag {flag}"
            );
        }
        let mut r = rig(
            FakeTranscriber::ok("i think"),
            Some(FakePolisher::err("polish HTTP 500")),
            false,
            false,
        );
        r.p.cfg.restore_first_i = true;
        dictate(&mut r, Duration::from_secs(1));
        assert_eq!(r.rung1.texts.lock().unwrap().as_slice(), ["i think"]);
    }

    #[test]
    fn trailing_space_is_appended_when_configured() {
        let mut r = rig(FakeTranscriber::ok("hi"), None, false, false);
        r.p.cfg.trailing_space = true;
        dictate(&mut r, Duration::from_secs(1));
        assert_eq!(r.rung1.texts.lock().unwrap().as_slice(), ["hi "]);
    }

    #[test]
    fn the_trailing_period_is_stripped_before_typing() {
        for (served, typed) in [
            ("Hello.", "Hello"),
            ("Hello .", "Hello"),
            ("Okay. See you.", "Okay. See you"),
            ("wait...", "wait..."),
            ("Really?", "Really?"),
            ("Stop!", "Stop!"),
        ] {
            let mut r = rig(
                FakeTranscriber::ok("spoken words"),
                Some(FakePolisher::ok(served)),
                false,
                false,
            );
            assert_eq!(
                dictate(&mut r, Duration::from_secs(1)),
                Some(Outcome::Inserted { rung: "type" }),
                "{served}"
            );
            assert_eq!(
                r.rung1.texts.lock().unwrap().as_slice(),
                [typed],
                "{served}"
            );
        }
        let mut r = rig(FakeTranscriber::ok("Raw words."), None, false, false);
        dictate(&mut r, Duration::from_secs(1));
        assert_eq!(r.rung1.texts.lock().unwrap().as_slice(), ["Raw words"]);
    }

    #[test]
    fn a_lone_period_ends_as_an_empty_dictation() {
        let mut r = rig(FakeTranscriber::ok("."), None, false, false);
        assert_eq!(
            dictate(&mut r, Duration::from_secs(1)),
            Some(Outcome::Empty)
        );
        assert!(r.rung1.texts.lock().unwrap().is_empty());
        assert!(r.p.shared().lock().unwrap().last_transcript.is_none());
    }

    #[test]
    fn the_stripped_text_feeds_trailing_space_and_last() {
        let mut r = rig(
            FakeTranscriber::ok("hello there"),
            Some(FakePolisher::ok("Hello.")),
            false,
            false,
        );
        r.p.cfg.trailing_space = true;
        dictate(&mut r, Duration::from_secs(1));
        assert_eq!(r.rung1.texts.lock().unwrap().as_slice(), ["Hello "]);
        assert_eq!(
            r.p.shared().lock().unwrap().last_transcript.as_deref(),
            Some("Hello ")
        );
    }

    #[test]
    fn a_disabled_pipeline_starts_nothing() {
        let mut r = rig(FakeTranscriber::ok("hi"), None, false, false);
        r.p.shared().lock().unwrap().enabled = false;
        let t0 = Instant::now();
        assert!(r.p.handle(HotkeyEvent::Pressed, t0).is_none());
        assert!(
            r.p.handle(HotkeyEvent::Released, t0 + Duration::from_secs(1))
                .is_none()
        );
        assert!(r.stt.calls.lock().unwrap().is_empty());
        assert_eq!((r.cap.starts(), r.cap.stops()), (0, 0));
        assert!(r.ind.0.lock().unwrap().is_empty());
        assert_eq!(r.p.shared().lock().unwrap().state, "idle");
    }

    #[test]
    fn disabling_mid_recording_closes_the_microphone_and_discards_the_audio() {
        let mut r = rig(
            FakeTranscriber::ok("heard while disabled"),
            None,
            false,
            false,
        );
        let t0 = Instant::now();
        assert!(r.p.handle(HotkeyEvent::Pressed, t0).is_none());
        r.p.shared().lock().unwrap().enabled = false;
        assert_eq!(
            r.p.handle(HotkeyEvent::Released, t0 + Duration::from_secs(1)),
            Some(Outcome::Discarded)
        );
        assert_eq!(r.cap.stops(), 1, "the microphone must not be left open");
        assert_eq!(r.p.shared().lock().unwrap().state, "idle");
        assert!(r.stt.calls.lock().unwrap().is_empty());
        assert_eq!(r.ind.0.lock().unwrap().last(), Some(&S::Idle));

        r.p.shared().lock().unwrap().enabled = true;
        assert_eq!(
            dictate(&mut r, Duration::from_secs(1)),
            Some(Outcome::Inserted { rung: "type" })
        );
        assert_eq!(r.cap.starts(), 2);
        assert_eq!(r.cap.stops(), 2);
        assert_eq!(r.stt.calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn microphone_open_failure_ends_in_error_without_a_request() {
        let mut cap = FakeCapture::new(16_000);
        cap.start_fails = true;
        let mut r = rig_with_capture(cap, FakeTranscriber::ok("hi"), None, false, false);
        let t0 = Instant::now();
        assert!(r.p.handle(HotkeyEvent::Pressed, t0).is_none());
        assert_eq!(r.ind.0.lock().unwrap().last(), Some(&S::Error));
        assert_eq!(r.p.shared().lock().unwrap().state, "idle");
        assert_eq!(r.cap.stops(), 0, "a mic that never opened is never closed");
        assert!(r.stt.calls.lock().unwrap().is_empty());
        assert!(
            r.p.handle(HotkeyEvent::Released, t0 + Duration::from_secs(1))
                .is_none()
        );
    }

    #[test]
    fn an_empty_capture_is_discarded_without_a_request() {
        let mut r = rig_with_capture(
            FakeCapture::new(0),
            FakeTranscriber::ok("hi"),
            None,
            false,
            false,
        );
        assert_eq!(
            dictate(&mut r, Duration::from_secs(1)),
            Some(Outcome::Discarded)
        );
        assert!(r.stt.calls.lock().unwrap().is_empty());
        assert_eq!(r.p.shared().lock().unwrap().state, "idle");
        assert_ne!(r.ind.0.lock().unwrap().last(), Some(&S::Error));
        assert!(
            !r.ind.0.lock().unwrap().contains(&S::Working),
            "the Working indicator was shown"
        );
    }

    #[test]
    fn a_silent_capture_is_discarded_without_a_request() {
        let mut cap = FakeCapture::new(16_000);
        cap.level = 0;
        let mut r = rig_with_capture(cap, FakeTranscriber::ok("hi"), None, false, false);
        let log = logged(|| {
            assert_eq!(
                dictate(&mut r, Duration::from_secs(1)),
                Some(Outcome::Discarded)
            );
        });
        assert!(log.contains("silent capture discarded"), "{log}");
        assert!(
            r.stt.calls.lock().unwrap().is_empty(),
            "a silent capture was sent"
        );
        assert!(
            r.rec.0.lock().unwrap().is_empty(),
            "a silent capture was recorded"
        );
        assert_eq!(r.p.shared().lock().unwrap().state, "idle");
        assert_ne!(r.ind.0.lock().unwrap().last(), Some(&S::Error));
        assert!(
            !r.ind.0.lock().unwrap().contains(&S::Working),
            "the Working indicator was shown"
        );
    }

    #[test]
    fn a_quiet_capture_above_the_silence_level_is_sent() {
        let mut cap = FakeCapture::new(16_000);
        cap.level = 400;
        let mut r = rig_with_capture(cap, FakeTranscriber::ok("hi"), None, false, false);
        assert!(
            matches!(
                dictate(&mut r, Duration::from_secs(1)),
                Some(Outcome::Inserted { .. })
            ),
            "a quiet capture was not inserted"
        );
        assert_eq!(
            r.stt.calls.lock().unwrap().len(),
            1,
            "a quiet capture was not sent"
        );
    }

    #[test]
    fn microphone_stop_failure_is_its_own_outcome() {
        let mut cap = FakeCapture::new(16_000);
        cap.stop_fails = true;
        let mut r = rig_with_capture(cap, FakeTranscriber::ok("hi"), None, false, false);
        assert_eq!(
            dictate(&mut r, Duration::from_secs(1)),
            Some(Outcome::CaptureFailed(
                "mic stop: stream ended early".into()
            ))
        );
        assert_eq!(r.cap.stops(), 1);
        assert!(r.stt.calls.lock().unwrap().is_empty());
        assert!(r.rung1.texts.lock().unwrap().is_empty());
        assert_eq!(r.ind.0.lock().unwrap().last(), Some(&S::Error));
        assert_eq!(r.p.shared().lock().unwrap().state, "idle");
    }

    #[test]
    fn the_recorder_sees_the_finished_dictation() {
        let mut r = rig(
            FakeTranscriber::scored("um hello", 0.03),
            Some(FakePolisher::ok("Hello.")),
            false,
            false,
        );
        dictate(&mut r, Duration::from_secs(1));
        let recs = r.rec.0.lock().unwrap();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].samples, 16_000);
        assert_eq!(recs[0].layout, Lang::parse("he"));
        assert_eq!(recs[0].language, "auto");
        assert_eq!(recs[0].raw, "um hello");
        assert_eq!(recs[0].no_speech_prob, Some(0.03));
        assert_eq!(recs[0].polished.as_deref(), Some("Hello."));
        assert_eq!(recs[0].polish_model.as_deref(), Some("cleanup-1"));
        assert_eq!(recs[0].rung, Some("type"));
        for ms in [recs[0].stt_ms, recs[0].polish_ms, recs[0].inject_ms] {
            assert!(
                ms < 60_000,
                "{ms} ms is not a plausible fake-backend timing"
            );
        }
    }

    struct PressesWhileWorking {
        tx: Mutex<Option<std::sync::mpsc::Sender<HotkeyEvent>>>,
        event: HotkeyEvent,
    }
    impl Transcriber for PressesWhileWorking {
        fn transcribe(
            &self,
            _wav: &[u8],
            _language: &SttLanguage,
            _prompt: Option<&str>,
        ) -> Result<Transcript, String> {
            if let Some(tx) = self.tx.lock().unwrap().take() {
                tx.send(self.event).expect("the pump still holds rx");
            }
            Ok(Transcript {
                text: "hi there".into(),
                no_speech_prob: None,
                no_speech_mean: None,
                language: None,
                detected: None,
            })
        }
    }

    fn pump_rig(
        mode: HotkeyMode,
        cap: FakeCapture,
        stt: Box<dyn Transcriber>,
        ind: FakeIndicator,
    ) -> Pipeline {
        let policy = LanguagePolicy::from_config(&LanguageConfig::default(), &[]).unwrap();
        Pipeline::new(
            PipelineConfig {
                initial_mode: mode,
                min_hold: Duration::ZERO,
                polish_min_words: 0,
                prompt: None,
                trailing_space: false,
                strip_terminal_period: true,
                restore_first_i: false,
                polish_model: String::new(),
                max_chars: 20_000,
                no_speech_threshold: 0.6,
                no_speech_warn: 0.0,
            },
            Box::new(cap),
            Box::new(FakeLayout(None)),
            policy,
            stt,
            None,
            vec![Box::new(FakeInject::new("type", false))],
            Box::new(FakeTarget {
                window: Some(1),
                ..Default::default()
            }),
            Box::new(ind),
            None,
        )
    }

    #[test]
    fn an_event_arriving_while_working_never_starts_a_recording() {
        for (mode, start, stop) in [
            (HotkeyMode::Toggle, HotkeyEvent::Toggle, HotkeyEvent::Toggle),
            (
                HotkeyMode::Hold,
                HotkeyEvent::Pressed,
                HotkeyEvent::Released,
            ),
        ] {
            let (tx, rx) = std::sync::mpsc::channel();
            let cap = FakeCapture::new(16_000);
            let ind = FakeIndicator::default();
            let stt = PressesWhileWorking {
                tx: Mutex::new(Some(tx.clone())),
                event: start,
            };
            let mut p = pump_rig(mode, cap.clone(), Box::new(stt), ind.clone());

            tx.send(start).unwrap();
            tx.send(stop).unwrap();
            drop(tx); // the transcriber's clone is the last one, and it drops it as it sends
            pump(&mut p, &rx);

            assert_eq!(
                (cap.starts(), cap.stops()),
                (1, 1),
                "{mode:?}: the microphone reopened for a {start:?} delivered while Working"
            );
            assert_eq!(p.shared().lock().unwrap().state, "idle", "{mode:?}");
            assert_eq!(ind.0.lock().unwrap().last(), Some(&S::Done), "{mode:?}");
        }
    }

    #[test]
    fn a_press_behind_a_discarded_tap_still_dictates() {
        let (tx, rx) = std::sync::mpsc::channel();
        let cap = FakeCapture::new(16_000);
        let ind = FakeIndicator::default();
        let mut p = pump_rig(
            HotkeyMode::Toggle,
            cap.clone(),
            Box::new(FakeTranscriber::ok("hi there")),
            ind.clone(),
        );
        p.cfg.min_hold = Duration::from_secs(10); // every hold here is a tap

        tx.send(HotkeyEvent::Toggle).unwrap(); // start
        tx.send(HotkeyEvent::Toggle).unwrap(); // stop: discarded as a tap
        tx.send(HotkeyEvent::Toggle).unwrap(); // the next dictation, already queued
        drop(tx);
        pump(&mut p, &rx);

        assert_eq!(cap.starts(), 2, "the press after a tap was dropped");
        assert_eq!(p.shared().lock().unwrap().state, "recording");
    }

    #[test]
    fn a_press_behind_an_empty_capture_still_dictates() {
        let (tx, rx) = std::sync::mpsc::channel();
        let cap = FakeCapture::new(0);
        let ind = FakeIndicator::default();
        let mut p = pump_rig(
            HotkeyMode::Toggle,
            cap.clone(),
            Box::new(FakeTranscriber::ok("hi there")),
            ind.clone(),
        );

        tx.send(HotkeyEvent::Toggle).unwrap(); // start
        tx.send(HotkeyEvent::Toggle).unwrap(); // stop: the capture is all click, cut to nothing
        tx.send(HotkeyEvent::Toggle).unwrap(); // the next dictation, already queued
        drop(tx);
        pump(&mut p, &rx);

        assert_eq!(
            cap.starts(),
            2,
            "the press after an empty capture was dropped"
        );
        assert_eq!(p.shared().lock().unwrap().state, "recording");
    }

    #[test]
    fn a_newline_is_never_typed() {
        assert_eq!(sanitize("ls\n rm -rf /\x1b[0m"), "ls  rm -rf /[0m");
        assert_eq!(sanitize("a\tb\rc\x08d\x7fe\u{85}f"), "abcdef");
        assert_eq!(sanitize("one\ntwo"), "one two");
        assert!(!sanitize("1. foo\n2. bar\n").contains('\n'));
    }

    #[test]
    fn hebrew_with_a_right_to_left_mark_is_untouched() {
        let he = "שלום\u{200F} עולם, Acme \u{200E}2026";
        assert_eq!(sanitize(he), he);
        let emoji = "\u{1F469}\u{200D}\u{1F4BB} \u{200C}x";
        assert_eq!(sanitize(emoji), emoji);
        assert_eq!(sanitize("a\u{202E}b\u{202D}c"), "abc");
        assert_eq!(sanitize("a\u{2066}b\u{2069}c"), "abc");
    }

    #[test]
    fn format_characters_are_dropped() {
        for c in [
            '\u{00AD}',
            '\u{061C}',
            '\u{180E}',
            '\u{200B}',
            '\u{2060}',
            '\u{FEFF}',
            '\u{E0001}',
            '\u{E0041}',
        ] {
            assert_eq!(sanitize(&format!("a{c}b")), "ab", "U+{:04X} kept", c as u32);
        }
    }

    #[test]
    fn line_and_paragraph_separators_become_a_space() {
        assert_eq!(sanitize("one\u{2028}two\u{2029}three"), "one two three");
    }

    #[test]
    fn a_transcript_that_sanitises_to_nothing_is_empty_not_inserted() {
        let mut r = rig(
            FakeTranscriber::ok("\x1b\x07\t\r\u{202E}\u{2066}"),
            None,
            false,
            false,
        );
        assert_eq!(
            dictate(&mut r, Duration::from_secs(1)),
            Some(Outcome::Empty)
        );
        assert!(r.rung1.texts.lock().unwrap().is_empty());
        assert!(r.rung2.texts.lock().unwrap().is_empty());
        assert!(r.p.shared().lock().unwrap().last_transcript.is_none());
        assert_eq!(r.ind.0.lock().unwrap().last(), Some(&S::Idle));

        let recs = r.rec.0.lock().unwrap();
        assert_eq!(recs.len(), 1, "the hostile reply left no row");
        assert_eq!(recs[0].raw, "\x1b\x07\t\r\u{202E}\u{2066}");
        assert_eq!(recs[0].dropped_chars, 6);
        assert_eq!(recs[0].rung, None);
    }

    #[test]
    fn an_untouched_dictation_records_no_dropped_characters() {
        let mut clean = rig(FakeTranscriber::ok("hello there"), None, false, false);
        dictate(&mut clean, Duration::from_secs(1));
        assert_eq!(clean.rec.0.lock().unwrap()[0].dropped_chars, 0);

        let mut dirty = rig(FakeTranscriber::ok("a\tb\rc"), None, false, false);
        dictate(&mut dirty, Duration::from_secs(1));
        let recs = dirty.rec.0.lock().unwrap();
        assert_eq!(recs[0].dropped_chars, 2);
        assert_eq!(recs[0].rung, Some("type"), "this one still got typed");
    }

    #[test]
    fn a_polish_failure_still_shows_an_error_when_the_reply_sanitises_away() {
        let mut r = rig(
            FakeTranscriber::ok("\x1b\x07"),
            Some(FakePolisher::err("polish HTTP 500: x")),
            false,
            false,
        );
        assert_eq!(
            dictate(&mut r, Duration::from_secs(1)),
            Some(Outcome::Empty)
        );
        assert_eq!(r.ind.0.lock().unwrap().last(), Some(&S::Error));
        assert_eq!(
            r.p.shared().lock().unwrap().last_error.as_deref(),
            Some("polish HTTP 500")
        );
    }

    #[test]
    fn every_rung_receives_the_sanitised_text() {
        let mut r = rig(
            FakeTranscriber::ok("raw\ttext"),
            Some(FakePolisher::err("polish HTTP 500: x")),
            true,
            false,
        );
        assert_eq!(
            dictate(&mut r, Duration::from_secs(1)),
            Some(Outcome::Inserted { rung: "paste" })
        );
        assert_eq!(r.rung2.texts.lock().unwrap().as_slice(), ["rawtext"]);
    }

    #[test]
    fn the_sanitising_warning_carries_a_count_and_no_text() {
        let mut r = rig(
            FakeTranscriber::ok("go\x1b]0;OWNED\x07 home"),
            None,
            false,
            false,
        );
        let log = logged(|| {
            assert_eq!(
                dictate(&mut r, Duration::from_secs(1)),
                Some(Outcome::Inserted { rung: "type" })
            );
        });
        assert!(log.contains("dropped=2"), "{log}");
        assert!(
            log.contains("removed control or format characters"),
            "{log}"
        );
        assert!(!log.contains("OWNED"), "{log}");
        assert_eq!(
            r.rung1.texts.lock().unwrap().as_slice(),
            ["go]0;OWNED home"]
        );
    }

    #[test]
    fn a_transcript_over_the_cap_is_held_rather_than_typed_or_truncated() {
        let long = "x".repeat(50);
        let mut r = rig(FakeTranscriber::ok(&long), None, false, false);
        r.p.cfg.max_chars = 10;
        let log = logged(|| {
            assert_eq!(dictate(&mut r, Duration::from_secs(1)), Some(Outcome::Held));
        });
        assert!(r.rung1.texts.lock().unwrap().is_empty());
        assert!(r.rung2.texts.lock().unwrap().is_empty());
        assert!(r.target.restored.lock().unwrap().is_empty());
        assert_eq!(
            r.p.shared().lock().unwrap().last_transcript.as_deref(),
            Some(long.as_str())
        );
        assert_eq!(r.ind.0.lock().unwrap().last(), Some(&S::Error));
        assert_eq!(
            r.p.shared().lock().unwrap().last_error.as_deref(),
            Some("transcript over inject.max_chars; run `byovox last`")
        );
        assert!(log.contains("chars=50"), "{log}");
        assert!(log.contains("max_chars=10"), "{log}");
        assert!(!log.contains(&long), "{log}");
    }

    #[test]
    fn the_cap_counts_characters_and_zero_lifts_it() {
        let cyrillic = "ы".repeat(30);
        assert_eq!(cyrillic.len(), 60);
        let mut r = rig(FakeTranscriber::ok(&cyrillic), None, false, false);
        r.p.cfg.max_chars = 40;
        assert_eq!(
            dictate(&mut r, Duration::from_secs(1)),
            Some(Outcome::Inserted { rung: "type" })
        );

        let mut off = rig(
            FakeTranscriber::ok(&"x".repeat(100_000)),
            None,
            false,
            false,
        );
        off.p.cfg.max_chars = 0;
        assert_eq!(
            dictate(&mut off, Duration::from_secs(1)),
            Some(Outcome::Inserted { rung: "type" })
        );

        let mut at = rig(FakeTranscriber::ok("0123456789"), None, false, false);
        at.p.cfg.max_chars = 10;
        assert_eq!(
            dictate(&mut at, Duration::from_secs(1)),
            Some(Outcome::Inserted { rung: "type" })
        );
    }

    #[test]
    fn the_capture_row_keeps_the_unsanitised_reply() {
        let mut r = rig(FakeTranscriber::ok("a\tb"), None, false, false);
        dictate(&mut r, Duration::from_secs(1));
        assert_eq!(r.rec.0.lock().unwrap()[0].raw, "a\tb");
        assert_eq!(r.rung1.texts.lock().unwrap().as_slice(), ["ab"]);
    }

    #[test]
    fn summary_drops_the_response_body_from_a_stage_error() {
        assert_eq!(
            summary("stt HTTP 500: {\"error\":\"SECRET TRANSCRIPT\"}"),
            "stt HTTP 500"
        );
        assert_eq!(
            summary("polish response has no content field: SECRET TRANSCRIPT"),
            "polish response has no content field"
        );
        assert_eq!(
            summary("stt transport: io: Connection refused"),
            "stt transport"
        );
        assert_eq!(
            summary("polish transport: connection refused"),
            "polish transport"
        );
        assert_eq!(summary("no colon at all"), "no colon at all");
    }
}
