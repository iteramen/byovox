//! One serial synthesis worker and one bounded pending selection. No clipboard, microphone or text logging.
use crate::indicator::IndicatorState;
use crate::pipeline::Shared;
use crate::read_aloud::{Endpoint, TtsConfig, synthesize};
use crate::ui::UserEvent;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;
use winit::event_loop::EventLoopProxy;

type SelectionPending = Arc<(Mutex<Option<(u64, isize)>>, Condvar)>;

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

#[derive(Clone)]
pub struct Manager {
    config: TtsConfig,
    generation: Arc<AtomicU64>,
    active: Arc<AtomicU64>,
    pending: Pending,
    selection_pending: SelectionPending,
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
        let manager = Self {
            config,
            generation,
            active,
            pending,
            selection_pending: Arc::new((Mutex::new(None), Condvar::new())),
            player,
            shared,
            proxy,
        };
        let lookup = manager.clone();
        std::thread::Builder::new()
            .name("byovox-selection".into())
            .spawn(move || {
                loop {
                    let (id, window) = {
                        let (lock, cv) = &*lookup.selection_pending;
                        let mut slot = lock.lock().unwrap_or_else(|p| p.into_inner());
                        while slot.is_none() {
                            slot = cv.wait(slot).unwrap_or_else(|p| p.into_inner());
                        }
                        slot.take().expect("pending selection")
                    };
                    let current = || lookup.generation.load(Ordering::SeqCst) == id;
                    let prepared = guarded_selection(&lookup.shared, current, || selection(window))
                        .and_then(|text| lookup.config.prepare(&text));
                    if !current() {
                        continue;
                    }
                    match prepared {
                        Ok(utterances) => {
                            let mut slot =
                                lookup.pending.0.lock().unwrap_or_else(|p| p.into_inner());
                            if !current() {
                                continue;
                            }
                            *slot = Some((id, utterances));
                            lookup.pending.1.notify_one();
                        }
                        Err(error) => {
                            if lookup
                                .active
                                .compare_exchange(id, 0, Ordering::SeqCst, Ordering::SeqCst)
                                .is_ok()
                            {
                                report(
                                    &lookup.shared,
                                    &lookup.proxy,
                                    &lookup.generation,
                                    id,
                                    IndicatorState::Error,
                                    Some(error),
                                );
                            }
                        }
                    }
                }
            })?;
        Ok(manager)
    }

    pub fn stop(&self) {
        let id = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let was_active = self.active.swap(0, Ordering::SeqCst) != 0;
        self.selection_pending
            .0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
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

    pub fn toggle(&self, window: isize) {
        if self.is_active() {
            self.stop();
            return;
        }
        let id = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        if let Err(error) = selection_admission(&self.shared) {
            report(
                &self.shared,
                &self.proxy,
                &self.generation,
                id,
                IndicatorState::Error,
                Some(error),
            );
            return;
        }
        self.active.store(id, Ordering::SeqCst);
        *self
            .selection_pending
            .0
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = Some((id, window));
        report(
            &self.shared,
            &self.proxy,
            &self.generation,
            id,
            IndicatorState::Working,
            None,
        );
        self.selection_pending.1.notify_one();
    }
}

fn selection_admission(shared: &Mutex<Shared>) -> Result<(), String> {
    let state = shared.lock().unwrap_or_else(|p| p.into_inner());
    if !state.enabled {
        return Err("Read aloud: byovox is disabled".into());
    }
    if state.state != "idle" || state.pending_actions != 0 {
        return Err("Read aloud: finish the current dictation first".into());
    }
    Ok(())
}

fn guarded_selection(
    shared: &Mutex<Shared>,
    current: impl Fn() -> bool,
    select: impl FnOnce() -> Result<Option<String>, String>,
) -> Result<String, String> {
    if !current() {
        return Err("Read aloud: cancelled".into());
    }
    selection_admission(shared)?;
    let selected = select()?;
    if !current() {
        return Err("Read aloud: cancelled".into());
    }
    selection_admission(shared)?;
    selected.ok_or_else(|| "Read aloud: select text first".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Barrier, atomic::AtomicBool};

    #[test]
    fn busy_disabled_and_cancelled_reads_never_inspect_selection() {
        let shared = Mutex::new(Shared::default());
        shared.lock().unwrap().state = "recording";
        assert!(
            guarded_selection(
                &shared,
                || true,
                || panic!("selection accessed while recording")
            )
            .is_err()
        );
        shared.lock().unwrap().state = "idle";
        shared.lock().unwrap().pending_actions = 1;
        assert!(
            guarded_selection(
                &shared,
                || true,
                || panic!("selection accessed before queued action")
            )
            .is_err()
        );
        shared.lock().unwrap().pending_actions = 0;
        shared.lock().unwrap().enabled = false;
        assert!(
            guarded_selection(
                &shared,
                || true,
                || panic!("selection accessed while disabled")
            )
            .is_err()
        );
        shared.lock().unwrap().enabled = true;
        assert!(
            guarded_selection(
                &shared,
                || false,
                || panic!("selection accessed after cancel")
            )
            .is_err()
        );
    }

    #[test]
    fn cancellation_does_not_wait_for_lookup_and_drops_its_late_text() {
        let shared = Arc::new(Mutex::new(Shared::default()));
        let current = Arc::new(AtomicBool::new(true));
        let entered = Arc::new(Barrier::new(2));
        let finish = Arc::new(Barrier::new(2));
        let (s, c, e, f) = (
            shared.clone(),
            current.clone(),
            entered.clone(),
            finish.clone(),
        );
        let worker = std::thread::spawn(move || {
            guarded_selection(
                &s,
                || c.load(Ordering::SeqCst),
                || {
                    e.wait();
                    f.wait();
                    Ok(Some("synthetic selection".into()))
                },
            )
        });
        entered.wait();
        // The slow provider owns no shared-state lock: stop/recording can progress immediately.
        shared
            .try_lock()
            .expect("lookup must not hold the dispatcher state")
            .state = "recording";
        current.store(false, Ordering::SeqCst);
        finish.wait();
        assert_eq!(worker.join().unwrap().unwrap_err(), "Read aloud: cancelled");
    }

    #[test]
    fn admission_is_checked_again_after_lookup() {
        let shared = Mutex::new(Shared::default());
        assert!(
            guarded_selection(
                &shared,
                || true,
                || {
                    shared.lock().unwrap().pending_actions = 1;
                    Ok(Some("synthetic selection".into()))
                }
            )
            .is_err()
        );
    }
}
