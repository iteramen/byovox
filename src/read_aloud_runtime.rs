//! One serial synthesis worker and one bounded pending selection. No clipboard, microphone or text logging.
use crate::indicator::IndicatorState;
use crate::pipeline::Shared;
use crate::read_aloud::{Endpoint, TtsConfig, synthesize};
use crate::ui::UserEvent;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;
use winit::event_loop::EventLoopProxy;

type Pending = Arc<(Mutex<Option<(u64, Vec<(String, Endpoint)>)>>, Condvar)>;

fn selection(window: isize) -> Result<Option<String>, String> {
    #[cfg(windows)]
    {
        crate::platform::windows::inject::selected_text_optional(window)
    }
    #[cfg(not(windows))]
    {
        let _ = window;
        Err("Read aloud: Windows selection backend unavailable".into())
    }
}

pub struct Manager {
    config: TtsConfig,
    generation: Arc<AtomicU64>,
    active: Arc<AtomicU64>,
    pending: Pending,
    player: Arc<Mutex<Option<rodio::Player>>>,
    shared: Arc<Mutex<Shared>>,
    proxy: EventLoopProxy<UserEvent>,
}

fn report(
    shared: &Mutex<Shared>,
    proxy: &EventLoopProxy<UserEvent>,
    generation: &Arc<AtomicU64>,
    id: u64,
    state: IndicatorState,
    error: Option<String>,
) {
    if generation.load(Ordering::SeqCst) != id {
        return;
    }
    let mut snapshot = shared.lock().unwrap_or_else(|p| p.into_inner());
    if generation.load(Ordering::SeqCst) != id {
        return;
    }
    snapshot.last_error = error;
    drop(snapshot);
    let _ = proxy.send_event(UserEvent::ReadAloud(id, generation.clone(), state));
}

impl Manager {
    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::SeqCst) != 0
    }
    pub fn is_playing(&self) -> bool {
        self.player
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
            .is_some_and(|p| !p.empty() && p.get_pos() > Duration::from_millis(20))
    }
    pub fn new(
        config: TtsConfig,
        shared: Arc<Mutex<Shared>>,
        proxy: EventLoopProxy<UserEvent>,
    ) -> std::io::Result<Self> {
        let generation = Arc::new(AtomicU64::new(0));
        let active = Arc::new(AtomicU64::new(0));
        let pending: Pending = Arc::new((Mutex::new(None), Condvar::new()));
        let player: Arc<Mutex<Option<rodio::Player>>> = Arc::new(Mutex::new(None));
        let (cfg, gen_id, busy, queue, output, pipeline, ui) = (
            config.clone(),
            generation.clone(),
            active.clone(),
            pending.clone(),
            player.clone(),
            shared.clone(),
            proxy.clone(),
        );
        std::thread::Builder::new()
            .name("byovox-read-aloud".into())
            .spawn(move || {
                loop {
                    let (id, utterances) = {
                        let (lock, cv) = &*queue;
                        let mut slot = lock.lock().unwrap_or_else(|p| p.into_inner());
                        while slot.is_none() {
                            slot = cv.wait(slot).unwrap_or_else(|p| p.into_inner());
                        }
                        slot.take().expect("pending selection")
                    };
                    let current = || gen_id.load(Ordering::SeqCst) == id;
                    let result = (|| -> Result<(), String> {
                        let mut device = rodio::DeviceSinkBuilder::open_default_sink()
                            .map_err(|_| "Read aloud: audio output unavailable")?;
                        device.log_on_drop(false);
                        let sink = rodio::Player::connect_new(device.mixer());
                        {
                            let mut target = output.lock().unwrap_or_else(|p| p.into_inner());
                            if !current() {
                                return Ok(());
                            }
                            *target = Some(sink);
                        }
                        for (text, endpoint) in utterances {
                            if !current() {
                                break;
                            }
                            let audio = synthesize(&cfg, &endpoint, &text, current)?;
                            {
                                let target = output.lock().unwrap_or_else(|p| p.into_inner());
                                if !current() {
                                    break;
                                }
                                target.as_ref().expect("active player").append(
                                    rodio::buffer::SamplesBuffer::new(
                                        std::num::NonZeroU16::new(audio.channels)
                                            .expect("validated channels"),
                                        std::num::NonZeroU32::new(audio.rate)
                                            .expect("validated rate"),
                                        audio.samples,
                                    ),
                                );
                            }
                            loop {
                                if !current() {
                                    break;
                                }
                                let empty = output
                                    .lock()
                                    .unwrap_or_else(|p| p.into_inner())
                                    .as_ref()
                                    .is_none_or(rodio::Player::empty);
                                if empty {
                                    break;
                                }
                                std::thread::sleep(Duration::from_millis(20));
                            }
                        }
                        Ok(())
                    })();
                    output.lock().unwrap_or_else(|p| p.into_inner()).take();
                    if current()
                        && busy
                            .compare_exchange(id, 0, Ordering::SeqCst, Ordering::SeqCst)
                            .is_ok()
                    {
                        let (state, error) = match result {
                            Ok(()) => (IndicatorState::Idle, None),
                            Err(e) => (IndicatorState::Error, Some(e)),
                        };
                        report(&pipeline, &ui, &gen_id, id, state, error);
                    }
                }
            })?;
        Ok(Self {
            config,
            generation,
            active,
            pending,
            player,
            shared,
            proxy,
        })
    }

    pub fn stop(&self) {
        let id = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let was_active = self.active.swap(0, Ordering::SeqCst) != 0;
        self.pending
            .0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        if let Some(player) = self
            .player
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
        {
            player.stop();
        }
        if was_active {
            report(
                &self.shared,
                &self.proxy,
                &self.generation,
                id,
                IndicatorState::Idle,
                None,
            );
        }
    }

    pub fn unavailable(&self) {
        let id = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        report(
            &self.shared,
            &self.proxy,
            &self.generation,
            id,
            IndicatorState::Error,
            Some("Read aloud: shortcut unavailable; choose another tts.hotkey and restart".into()),
        );
    }

    /// True consumes this physical press and its release; false keeps normal dictation handling.
    pub fn contextual(&self, window: isize) -> bool {
        if self.is_active() {
            self.stop();
            return true;
        }
        {
            let shared = self.shared.lock().unwrap_or_else(|p| p.into_inner());
            if !shared.enabled {
                return true;
            }
            // A recording press must retain normal hold/toggle semantics regardless of selection.
            if shared.state != "idle" || shared.pending_actions != 0 {
                return false;
            }
        }
        match selection(window) {
            Ok(None) => false,
            Ok(Some(text)) => {
                self.begin(Ok(text));
                true
            }
            Err(error) => {
                self.begin(Err(error));
                true
            }
        }
    }

    pub fn toggle(&self, window: isize) {
        if self.is_active() {
            self.stop();
            return;
        }
        self.begin(
            selection(window)
                .and_then(|text| text.ok_or_else(|| "Read aloud: select text first".into())),
        );
    }

    fn begin(&self, selected: Result<String, String>) {
        let id = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let result = (|| {
            {
                let shared = self.shared.lock().unwrap_or_else(|p| p.into_inner());
                if !shared.enabled {
                    return Err("Read aloud: byovox is disabled".into());
                }
                if shared.state != "idle" || shared.pending_actions != 0 {
                    return Err("Read aloud: finish the current dictation first".into());
                }
            }
            self.config.prepare(&selected?)
        })();
        match result {
            Ok(utterances) => {
                self.active.store(id, Ordering::SeqCst);
                *self.pending.0.lock().unwrap_or_else(|p| p.into_inner()) = Some((id, utterances));
                report(
                    &self.shared,
                    &self.proxy,
                    &self.generation,
                    id,
                    IndicatorState::Working,
                    None,
                );
                self.pending.1.notify_one();
            }
            Err(error) => report(
                &self.shared,
                &self.proxy,
                &self.generation,
                id,
                IndicatorState::Error,
                Some(error),
            ),
        }
    }
}
