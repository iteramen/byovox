//! Test doubles: a one-thread HTTP server and in-process fakes for the platform traits.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

enum Reply {
    Raw(String),
    Close,
    Stall,
}

pub struct MockServer {
    url: String,
    requests: Arc<Mutex<Vec<String>>>,
}

impl MockServer {
    pub fn start(status: u16, body: &str) -> MockServer {
        Self::serve(vec![Reply::Raw(Self::response(status, body))])
    }

    /// Answers every request with this exact response text — for replies a well-formed
    /// server would not produce, such as a `Content-Length` longer than the body sent.
    pub fn start_raw(response: &str) -> MockServer {
        Self::serve(vec![Reply::Raw(response.to_string())])
    }

    pub fn start_closing_first(status: u16, body: &str) -> MockServer {
        Self::serve(vec![Reply::Close, Reply::Raw(Self::response(status, body))])
    }

    pub fn start_then(first: (u16, &str), then: (u16, &str)) -> MockServer {
        Self::serve(vec![
            Reply::Raw(Self::response(first.0, first.1)),
            Reply::Raw(Self::response(then.0, then.1)),
        ])
    }

    /// Records every request and answers none, holding each connection open while it goes on
    /// accepting: a server still transcribing when the client's timeout runs out.
    pub fn start_stalling() -> MockServer {
        Self::serve(vec![Reply::Stall])
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }

    fn response(status: u16, body: &str) -> String {
        format!(
            "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    /// Serves connection *i* with `replies[i]`; the last entry repeats forever.
    fn serve(replies: Vec<Reply>) -> MockServer {
        assert!(
            !replies.is_empty(),
            "a mock server needs at least one reply"
        );
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests: Arc<Mutex<Vec<String>>> = Default::default();
        let recorded = requests.clone();
        std::thread::spawn(move || {
            let mut held = Vec::new();
            for (i, stream) in listener.incoming().enumerate() {
                let Ok(mut s) = stream else { break };
                let reply = &replies[i.min(replies.len() - 1)];
                if matches!(reply, Reply::Close) {
                    continue;
                }
                let mut raw = Vec::new();
                let mut buf = [0u8; 4096];
                let mut header_end = None;
                while header_end.is_none() {
                    let Ok(n) = s.read(&mut buf) else { break };
                    if n == 0 {
                        break;
                    }
                    raw.extend_from_slice(&buf[..n]);
                    header_end = raw.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4);
                }
                let Some(he) = header_end else { continue };
                let head = String::from_utf8_lossy(&raw[..he]).to_string();
                let len: usize = head
                    .lines()
                    .find_map(|l| {
                        let (name, value) = l.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim())
                    })
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                while raw.len() < he + len {
                    let Ok(n) = s.read(&mut buf) else { break };
                    if n == 0 {
                        break;
                    }
                    raw.extend_from_slice(&buf[..n]);
                }
                recorded
                    .lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&raw).to_string());
                match reply {
                    Reply::Raw(response) => {
                        let _ = s.write_all(response.as_bytes());
                    }
                    _ => held.push(s),
                }
            }
        });
        MockServer { url, requests }
    }
}

/// A recorded request's `Authorization` value, found case-insensitively by name (RFC 9110
/// header names are case-insensitive, and ureq 3 sends them lowercased).
pub fn auth_header(req: &str) -> Option<String> {
    req.lines().find_map(|l| {
        let (name, value) = l.split_once(':')?;
        name.eq_ignore_ascii_case("authorization")
            .then(|| value.trim().to_string())
    })
}

pub mod fakes {
    use std::sync::{Arc, Mutex};

    use crate::audio::Audio;
    use crate::capture::Capture;
    use crate::indicator::{Indicator, IndicatorState};
    use crate::inject::{Inject, Target};
    use crate::lang::{Lang, SttLanguage};
    use crate::layout::Layout;
    use crate::pipeline::{DictationRecord, Recorder};
    use crate::polish::Polisher;
    use crate::stt::{Transcriber, Transcript};

    #[derive(Clone)]
    pub struct FakeCapture {
        pub samples: usize,
        pub level: i16,
        pub start_fails: bool,
        pub stop_fails: bool,
        pub starts: Arc<Mutex<usize>>,
        pub stops: Arc<Mutex<usize>>,
    }
    impl FakeCapture {
        pub fn new(samples: usize) -> Self {
            Self {
                samples,
                level: 1000,
                start_fails: false,
                stop_fails: false,
                starts: Default::default(),
                stops: Default::default(),
            }
        }

        pub fn starts(&self) -> usize {
            *self.starts.lock().unwrap()
        }

        pub fn stops(&self) -> usize {
            *self.stops.lock().unwrap()
        }
    }
    impl Capture for FakeCapture {
        fn start(&mut self) -> Result<(), String> {
            *self.starts.lock().unwrap() += 1;
            if self.start_fails {
                return Err("mic open: device in use".into());
            }
            Ok(())
        }
        fn stop(&mut self) -> Result<Audio, String> {
            *self.stops.lock().unwrap() += 1;
            if self.stop_fails {
                return Err("mic stop: stream ended early".into());
            }
            Ok(Audio {
                samples: vec![self.level; self.samples],
            })
        }
    }

    pub struct FakeLayout(pub Option<Lang>);
    impl Layout for FakeLayout {
        fn current(&self) -> Option<Lang> {
            self.0
        }
    }

    pub type TranscribeCall = (Vec<(&'static str, String)>, Option<String>);

    #[derive(Clone)]
    pub struct FakeTranscriber {
        pub result: Result<Transcript, String>,
        pub calls: Arc<Mutex<Vec<TranscribeCall>>>,
    }
    impl FakeTranscriber {
        pub fn ok(text: &str) -> Self {
            Self {
                result: Ok(Transcript {
                    text: text.into(),
                    no_speech_prob: None,
                    no_speech_mean: None,
                    language: None,
                    detected: None,
                }),
                calls: Default::default(),
            }
        }
        pub fn scored(text: &str, p: f32) -> Self {
            Self::scored_segments(text, p, p)
        }
        pub fn scored_segments(text: &str, mean: f32, max: f32) -> Self {
            Self {
                result: Ok(Transcript {
                    text: text.into(),
                    no_speech_prob: Some(max),
                    no_speech_mean: Some(mean),
                    language: None,
                    detected: None,
                }),
                calls: Default::default(),
            }
        }
        pub fn err(e: &str) -> Self {
            Self {
                result: Err(e.into()),
                calls: Default::default(),
            }
        }
    }
    impl Transcriber for FakeTranscriber {
        fn transcribe(
            &self,
            _wav: &[u8],
            language: &SttLanguage,
            prompt: Option<&str>,
        ) -> Result<Transcript, String> {
            self.calls
                .lock()
                .unwrap()
                .push((language.form_fields(), prompt.map(str::to_string)));
            self.result.clone()
        }
    }

    #[derive(Clone)]
    pub struct FakePolisher {
        pub result: Result<String, String>,
        pub calls: Arc<Mutex<Vec<String>>>,
    }
    impl FakePolisher {
        pub fn ok(text: &str) -> Self {
            Self {
                result: Ok(text.into()),
                calls: Default::default(),
            }
        }
        pub fn err(e: &str) -> Self {
            Self {
                result: Err(e.into()),
                calls: Default::default(),
            }
        }
    }
    impl Polisher for FakePolisher {
        fn polish(&self, raw: &str) -> Result<String, String> {
            self.calls.lock().unwrap().push(raw.to_string());
            self.result.clone()
        }
    }

    #[derive(Clone)]
    pub struct FakeInject {
        pub name: &'static str,
        pub fail: bool,
        pub texts: Arc<Mutex<Vec<String>>>,
    }
    impl FakeInject {
        pub fn new(name: &'static str, fail: bool) -> Self {
            Self {
                name,
                fail,
                texts: Default::default(),
            }
        }
    }
    #[derive(Clone, Default)]
    pub struct FakeTarget {
        pub window: Option<isize>,
        pub closed: bool,
        pub accepts: bool,
        pub restored: Arc<Mutex<Vec<isize>>>,
    }
    impl Target for FakeTarget {
        fn current(&self) -> Option<isize> {
            self.window
        }
        fn restore(&self, window: isize) -> Result<(), String> {
            self.restored.lock().unwrap().push(window);
            if self.closed {
                return Err("the window dictated into was closed".into());
            }
            Ok(())
        }
        fn accepts_text(&self) -> bool {
            self.accepts
        }
    }

    impl Inject for FakeInject {
        fn name(&self) -> &'static str {
            self.name
        }
        fn inject(&mut self, text: &str) -> Result<(), String> {
            if self.fail {
                return Err(format!("{} failed", self.name));
            }
            self.texts.lock().unwrap().push(text.to_string());
            Ok(())
        }
    }

    #[derive(Clone, Default)]
    pub struct FakeIndicator(pub Arc<Mutex<Vec<IndicatorState>>>);
    impl Indicator for FakeIndicator {
        fn set(&mut self, state: IndicatorState) {
            self.0.lock().unwrap().push(state);
        }
    }

    #[derive(Clone, Debug, PartialEq)]
    pub struct RecordedDictation {
        pub samples: usize,
        pub layout: Option<Lang>,
        pub language: String,
        pub detected: Option<String>,
        pub detected_p: Option<f32>,
        pub raw: String,
        pub no_speech_prob: Option<f32>,
        pub polished: Option<String>,
        pub polish_model: Option<String>,
        pub rung: Option<&'static str>,
        pub dropped_chars: usize,
        pub stt_ms: u128,
        pub polish_ms: u128,
        pub inject_ms: u128,
    }

    #[derive(Clone, Default)]
    pub struct FakeRecorder(pub Arc<Mutex<Vec<RecordedDictation>>>);
    impl Recorder for FakeRecorder {
        fn record(&mut self, r: &DictationRecord<'_>) {
            self.0.lock().unwrap().push(RecordedDictation {
                samples: r.audio.samples.len(),
                layout: r.layout,
                language: r.language.label(),
                detected: r.detected.map(|(code, _)| code.to_string()),
                detected_p: r.detected.map(|(_, p)| p),
                raw: r.raw.to_string(),
                no_speech_prob: r.no_speech_prob,
                polished: r.polished.map(str::to_string),
                polish_model: r.polish_model.map(str::to_string),
                rung: r.rung,
                dropped_chars: r.dropped_chars,
                stt_ms: r.stt_ms,
                polish_ms: r.polish_ms,
                inject_ms: r.inject_ms,
            });
        }
    }
}

// One global collector for the test binary: `tracing` caches each callsite's interest process-wide,
// so a second `.init()` would panic and a scoped collector would lose a race with whichever
// thread reached the callsite first.

thread_local! {
    static SINK: std::cell::RefCell<Option<Vec<u8>>> = const { std::cell::RefCell::new(None) };
}

struct ThreadSink;
impl std::io::Write for ThreadSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        SINK.with(|s| {
            if let Some(armed) = s.borrow_mut().as_mut() {
                armed.extend_from_slice(buf);
            }
        });
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for ThreadSink {
    type Writer = ThreadSink;
    fn make_writer(&'a self) -> ThreadSink {
        ThreadSink
    }
}

/// The collector is global, installed once, not scoped with `set_default`: a scoped collector
/// loses the callsite-interest race (a thread with none caches "never"), so the helper would
/// capture nothing, intermittently, only under the whole suite. The writer does the routing.
pub fn logged(run: impl FnOnce()) -> String {
    static INSTALLED: std::sync::Once = std::sync::Once::new();
    INSTALLED.call_once(|| {
        tracing_subscriber::fmt()
            .with_writer(ThreadSink)
            // Colour codes would land between a field name and its value.
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .init();
    });
    SINK.with(|s| *s.borrow_mut() = Some(Vec::new()));
    run();
    let captured = SINK.with(|s| s.borrow_mut().take()).unwrap_or_default();
    String::from_utf8(captured).unwrap()
}
