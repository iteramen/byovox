//! Explicit manual smoke: select this fixture's text, Ctrl+Alt+R to read, again to stop.
//! No input injection, microphone, clipboard or ordinary document is used.
#[cfg(windows)]
mod probe {
    use byovox::{
        hotkey::{Hotkey, HotkeyEvent},
        indicator::IndicatorState,
        pipeline::Shared,
        platform::windows::hotkey::HookHotkey,
        read_aloud::TtsConfig,
        read_aloud_runtime::Manager,
        ui::UserEvent,
    };
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Arc, Mutex, mpsc::channel};
    use std::time::{Duration, Instant};
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, GetForegroundWindow, IsWindow,
        RegisterClassW, SetWindowTextW, WINDOW_EX_STYLE, WINDOW_STYLE, WNDCLASSW, WS_CHILD,
        WS_OVERLAPPEDWINDOW, WS_VISIBLE,
    };
    use windows::core::{PCWSTR, w};
    use winit::{
        application::ApplicationHandler,
        event_loop::{ActiveEventLoop, ControlFlow, EventLoop},
    };
    unsafe extern "system" fn fixture_proc(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        // SAFETY: the standard default procedure handles this fixture's native messages.
        unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
    }
    struct App {
        manager: Arc<Manager>,
        shared: Arc<Mutex<Shared>>,
        window: HWND,
        status: HWND,
        started: Instant,
        next_diagnostic: Instant,
        stopped_at: Option<Instant>,
        stops: Arc<AtomicU32>,
        dictations: Arc<AtomicU32>,
        read_events: Arc<AtomicU32>,
        focused_events: Arc<AtomicU32>,
        failure: Option<String>,
        passed: bool,
    }
    impl ApplicationHandler<UserEvent> for App {
        fn resumed(&mut self, _: &ActiveEventLoop) {}
        fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
            if let UserEvent::ReadAloud(_, _, state) = event {
                println!(
                    "Read state: {state:?}; error={:?}",
                    self.shared.lock().unwrap().last_error
                );
                let text = match state {
                    IndicatorState::Working => {
                        "Requesting speech. Please wait; press again only to cancel."
                    }
                    IndicatorState::Idle => {
                        "Stopped or finished. Select text and press Ctrl+Alt+R to read."
                    }
                    _ => "Read failed; see probe diagnostics.",
                };
                let wide: Vec<u16> = text.encode_utf16().chain([0]).collect();
                unsafe {
                    let _ = SetWindowTextW(self.status, PCWSTR(wide.as_ptr()));
                }
                if state == IndicatorState::Error {
                    self.failure = self.shared.lock().unwrap().last_error.clone();
                    event_loop.exit();
                }
            }
        }
        fn window_event(
            &mut self,
            _: &ActiveEventLoop,
            _: winit::window::WindowId,
            _: winit::event::WindowEvent,
        ) {
        }
        fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
            let now = Instant::now();
            if now >= self.next_diagnostic {
                #[cfg(debug_assertions)]
                println!(
                    "Shortcut health: hook={:?} own_window_focused={} read_actions={} accepted={}",
                    byovox::platform::windows::hotkey::read_hook_diagnostics(),
                    unsafe { GetForegroundWindow() } == self.window,
                    self.read_events.load(Ordering::SeqCst),
                    self.focused_events.load(Ordering::SeqCst)
                );
                if self.manager.is_playing() {
                    unsafe {
                        let _ = SetWindowTextW(
                            self.status,
                            w!("Speaking. Press Ctrl+Alt+R again to stop."),
                        );
                    }
                }
                println!(
                    "Playback health: active={} playing={} error={:?}",
                    self.manager.is_active(),
                    self.manager.is_playing(),
                    self.shared.lock().unwrap().last_error
                );
                self.next_diagnostic = now + Duration::from_secs(15);
            }
            if !unsafe { IsWindow(Some(self.window)) }.as_bool()
                || now.duration_since(self.started) > Duration::from_secs(600)
            {
                println!(
                    "Probe diagnostics: read_hotkeys={} own_window_hotkeys={} active={} playing={} dictations={}",
                    self.read_events.load(Ordering::SeqCst),
                    self.focused_events.load(Ordering::SeqCst),
                    self.manager.is_active(),
                    self.manager.is_playing(),
                    self.dictations.load(Ordering::SeqCst)
                );
                self.failure = Some("Manual desktop probe closed or timed out".into());
                event_loop.exit();
                return;
            }
            if self.stops.load(Ordering::SeqCst) > 0 {
                let at = *self.stopped_at.get_or_insert(now);
                if now.duration_since(at) > Duration::from_millis(1500) {
                    self.passed = !self.manager.is_active()
                        && !self.manager.is_playing()
                        && self.dictations.load(Ordering::SeqCst) == 0;
                    event_loop.exit();
                    return;
                }
            }
            event_loop.set_control_flow(ControlFlow::WaitUntil(now + Duration::from_millis(30)));
        }
    }
    pub fn run() -> Result<(), String> {
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .with_target(false)
            .init();
        let base = std::env::args().nth(1).ok_or("Pass the own TTS base URL")?;
        let config = TtsConfig {
            enabled: true,
            base_url: base,
            hotkey: "ControlLeft+AltLeft+R".into(),
            ..Default::default()
        };
        config.validate()?;
        let event_loop = EventLoop::<UserEvent>::with_user_event()
            .build()
            .map_err(|_| "Event loop unavailable")?;
        let shared = Arc::new(Mutex::new(Shared::default()));
        let manager = Arc::new(
            Manager::new(config, shared.clone(), event_loop.create_proxy())
                .map_err(|_| "TTS worker unavailable")?,
        );
        // SAFETY: standard windows owned by this fixture thread, containing only synthetic text.
        let class = w!("ByovoxReadAloudValidation");
        unsafe {
            if RegisterClassW(&WNDCLASSW {
                lpfnWndProc: Some(fixture_proc),
                lpszClassName: class,
                ..Default::default()
            }) == 0
            {
                return Err("Test window class unavailable".into());
            }
        }
        let window = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                class,
                w!("byovox read-aloud validation - select text, Ctrl+Alt+R"),
                WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                150,
                150,
                850,
                300,
                None,
                None,
                None,
                None,
            )
        }
        .map_err(|_| "Test window unavailable")?;
        let _edit=unsafe{CreateWindowExW(WINDOW_EX_STYLE(0),w!("EDIT"),
            w!("Read aloud test. This is harmless synthetic English text. Select this paragraph and press Control, Alt and R to hear it. Press the same keys again while it is speaking to stop playback. No microphone or clipboard is used by this validation window."),
            WINDOW_STYLE(WS_CHILD.0|WS_VISIBLE.0|0x0004|0x0800),20,25,790,190,Some(window),None,None,None)}.map_err(|_|"Test field unavailable")?;
        let status = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                w!("Select the paragraph. Press Ctrl+Alt+R once, then wait for speech."),
                WS_CHILD | WS_VISIBLE,
                20,
                215,
                790,
                30,
                Some(window),
                None,
                None,
                None,
            )
        }
        .map_err(|_| "Test status unavailable")?;
        let (tx, rx) = channel();
        let hook =
            HookHotkey::new("F23", "Escape")?.with_read_aloud("ControlLeft+AltLeft+R", "Escape")?;
        std::thread::spawn(move || Box::new(hook).run(tx));
        let stops = Arc::new(AtomicU32::new(0));
        let dictations = Arc::new(AtomicU32::new(0));
        let read_events = Arc::new(AtomicU32::new(0));
        let focused_events = Arc::new(AtomicU32::new(0));
        let (worker, stop_count, other, reads, focused) = (
            manager.clone(),
            stops.clone(),
            dictations.clone(),
            read_events.clone(),
            focused_events.clone(),
        );
        let owned = window.0 as isize;
        std::thread::spawn(move || {
            while let Ok(event) = rx.recv() {
                match event {
                    HotkeyEvent::ReadAloud(window) => {
                        reads.fetch_add(1, Ordering::SeqCst);
                        let own =
                            window == owned && unsafe { GetForegroundWindow() }.0 as isize == owned;
                        println!("Read hotkey received; fixture focused={own}");
                        if own {
                            focused.fetch_add(1, Ordering::SeqCst);
                            let playing = worker.is_playing();
                            println!(
                                "Before toggle: active={} playing={playing}",
                                worker.is_active()
                            );
                            worker.toggle(window);
                            println!(
                                "After toggle: active={} playing={}",
                                worker.is_active(),
                                worker.is_playing()
                            );
                            if playing {
                                stop_count.fetch_add(1, Ordering::SeqCst);
                            }
                        }
                    }
                    HotkeyEvent::Cancel => worker.stop(),
                    HotkeyEvent::ReadAloudUnavailable => worker.unavailable(),
                    HotkeyEvent::Pressed | HotkeyEvent::Toggle => {
                        other.fetch_add(1, Ordering::SeqCst);
                    }
                    HotkeyEvent::Released => {}
                }
            }
        });
        println!(
            "Manual fixture ready: select its paragraph, Ctrl+Alt+R to read, same chord while speaking to stop."
        );
        let mut app = App {
            manager,
            shared,
            window,
            status,
            started: Instant::now(),
            next_diagnostic: Instant::now() + Duration::from_secs(1),
            stopped_at: None,
            stops,
            dictations,
            read_events,
            focused_events,
            failure: None,
            passed: false,
        };
        let result = event_loop
            .run_app(&mut app)
            .map_err(|_| "Desktop event loop failed");
        app.manager.stop();
        unsafe {
            let _ = DestroyWindow(app.window);
        }
        result?;
        if !app.passed {
            return Err(app
                .failure
                .unwrap_or_else(|| "Read/stop validation did not complete".into()));
        }
        println!(
            "Verified selected fixture text -> live TTS -> audio playback; hotkey stop; no dictation or late playback."
        );
        Ok(())
    }
}
#[cfg(windows)]
fn main() -> Result<(), String> {
    probe::run()
}
#[cfg(not(windows))]
fn main() {
    println!("This interactive probe requires Windows.");
}
