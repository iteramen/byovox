//! Main-thread UI: the winit event loop, tray icon and menu, floating pill, and audio cues.

use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::window::{Window, WindowId, WindowLevel};

use crate::hotkey::{HotkeyEvent, HotkeyMode};
use crate::indicator::{Indicator, IndicatorState};
use crate::pipeline::Shared;

/// `Shared` behind a lock a pipeline panic may have poisoned: unwinding through winit's Win32 frames
/// is worse than reading what the panicking thread left. The pipeline posts `Quit` when it dies.
fn shared_of(lock: &Mutex<Shared>) -> std::sync::MutexGuard<'_, Shared> {
    lock.lock().unwrap_or_else(|p| p.into_inner())
}

const ERROR_HOLD: Duration = Duration::from_secs(3);

/// Windows fires a burst of notifications when a Bluetooth headset disconnects; the default settles only after it.
const CUE_REOPEN_SETTLE: Duration = Duration::from_millis(500);

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct ReopenTimer(Option<Instant>);

impl ReopenTimer {
    /// The first notification of a run sets the deadline and the rest ride on it, so a flapping endpoint still fires.
    fn notified(&mut self, now: Instant) {
        self.0.get_or_insert(now + CUE_REOPEN_SETTLE);
    }

    fn due(&mut self, now: Instant) -> bool {
        match self.0 {
            Some(at) if now >= at => {
                self.0 = None;
                true
            }
            _ => false,
        }
    }

    fn deadline(self) -> Option<Instant> {
        self.0
    }
}

const SPIN_FRAMES: usize = 8;
const SPIN_PERIOD: Duration = Duration::from_millis(125);

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct SpinTimer(Option<(Instant, usize)>);

impl SpinTimer {
    fn start(&mut self, now: Instant) {
        self.0 = Some((now + SPIN_PERIOD, 1));
    }

    fn stop(&mut self) {
        self.0 = None;
    }

    /// Re-arms from `now`, so a stalled loop shows one frame instead of replaying every frame it slept through.
    fn due(&mut self, now: Instant) -> Option<usize> {
        match self.0 {
            Some((at, frame)) if now >= at => {
                self.0 = Some((now + SPIN_PERIOD, (frame + 1) % SPIN_FRAMES));
                Some(frame)
            }
            _ => None,
        }
    }

    fn deadline(self) -> Option<Instant> {
        self.0.map(|(at, _)| at)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuAction {
    ToggleEnabled,
    ToggleMode,
    ToggleCues,
    #[cfg(windows)]
    ToggleAutostart,
    CopyLast,
    OpenConfig,
    OpenLogs,
    RunCheck,
    OpenRelease,
    Quit,
}

#[derive(Debug, Clone)]
pub enum UserEvent {
    Indicator(IndicatorState),
    Menu(MenuAction),
    /// The cue sink may be bound to a removed endpoint. Posted from a WASAPI thread, so it carries nothing.
    AudioDeviceChanged,
    UpdateAvailable(crate::update::Release),
    Quit,
}

pub struct UiOptions {
    pub pill: bool,
    /// Only the starting value; `App.cues_on` is authoritative afterwards.
    pub cue: bool,
    pub cue_volume: f32,
    pub version: &'static str,
}

#[derive(Clone)]
pub struct ProxyIndicator(pub EventLoopProxy<UserEvent>);

impl Indicator for ProxyIndicator {
    /// Never blocks; a closed loop is ignored because the process is exiting.
    fn set(&mut self, state: IndicatorState) {
        let _ = self.0.send_event(UserEvent::Indicator(state));
    }
}

pub fn build_event_loop() -> Result<(EventLoop<UserEvent>, EventLoopProxy<UserEvent>)> {
    let event_loop = EventLoop::<UserEvent>::with_user_event()
        .build()
        .context("event loop")?;
    let proxy = event_loop.create_proxy();
    Ok((event_loop, proxy))
}

pub fn icon_rgba(state: IndicatorState) -> Vec<u8> {
    let (r, g, b) = match state {
        IndicatorState::Idle | IndicatorState::Done | IndicatorState::Uncertain => (140, 140, 140),
        IndicatorState::Recording => (220, 40, 40),
        IndicatorState::Working => (230, 160, 30),
        IndicatorState::Error => (200, 30, 90),
    };
    let mut px = vec![0u8; 32 * 32 * 4];
    for y in 0..32 {
        for x in 0..32 {
            let dx = x as f32 - 15.5;
            let fy = y as f32;
            // body radius 4 against the ring's inner edge at 7.2 leaves a 3 px gap that survives the tray's 2:1 downscale
            let dy = fy - fy.clamp(9.0, 13.0);
            let body = dx * dx + dy * dy <= 4.0 * 4.0;
            let ring = (dx * dx + (fy - 12.5) * (fy - 12.5)).sqrt();
            let cradle = fy >= 12.5 && (ring - 8.5).abs() <= 1.3;
            let stem = dx.abs() <= 1.7 && (21.0..=24.0).contains(&fy);
            let base = dx.abs() <= 6.0 && (24.0..=26.0).contains(&fy);
            if body || cradle || stem || base {
                let i = (y * 32 + x) * 4;
                px[i..i + 4].copy_from_slice(&[r, g, b, 255]);
            }
        }
    }
    px
}

fn working_frame_rgba(frame: usize) -> Vec<u8> {
    let mut px = icon_rgba(IndicatorState::Working);
    let angle = (frame % SPIN_FRAMES) as f32 * std::f32::consts::TAU / SPIN_FRAMES as f32;
    // y grows downwards, so +sin / −cos is clockwise from 12 o'clock
    let (cx, cy) = (15.5 + 13.5 * angle.sin(), 16.0 - 13.5 * angle.cos());
    for y in 0..32 {
        for x in 0..32 {
            let (dx, dy) = (x as f32 - cx, y as f32 - cy);
            if dx * dx + dy * dy <= 2.0 * 2.0 {
                let i = (y * 32 + x) * 4;
                px[i..i + 4].copy_from_slice(&[230, 160, 30, 255]);
            }
        }
    }
    px
}

pub fn disabled_icon_rgba() -> Vec<u8> {
    let mut px = icon_rgba(IndicatorState::Idle);
    for y in 0..32 {
        for x in 0..32 {
            let dx = x as f32 - 15.5;
            let dy = y as f32 - 15.5;
            let on_diagonal = (dx - dy).abs() < 2.2 || (dx + dy).abs() < 2.2;
            if on_diagonal && dx.abs() <= 8.0 && dy.abs() <= 8.0 {
                let i = (y * 32 + x) * 4;
                px[i..i + 4].copy_from_slice(&[220, 40, 40, 255]);
            }
        }
    }
    px
}

fn other_mode(current: HotkeyMode) -> HotkeyMode {
    match current {
        HotkeyMode::Hold => HotkeyMode::Toggle,
        HotkeyMode::Toggle => HotkeyMode::Hold,
    }
}

pub fn pill_text(state: IndicatorState) -> Option<&'static str> {
    match state {
        IndicatorState::Recording => Some("●  recording"),
        IndicatorState::Working => Some("…  working"),
        IndicatorState::Error => Some("×  failed"),
        IndicatorState::Idle | IndicatorState::Done | IndicatorState::Uncertain => None,
    }
}

/// Done reads as idle: it exists only to separate the completion cue from silent returns to Idle.
fn state_word(state: IndicatorState) -> &'static str {
    match state {
        IndicatorState::Idle | IndicatorState::Done | IndicatorState::Uncertain => "idle",
        IndicatorState::Recording => "recording",
        IndicatorState::Working => "working",
        IndicatorState::Error => "error",
    }
}

fn status_label(
    enabled: bool,
    state: IndicatorState,
    last_error: Option<String>,
    update: Option<&str>,
) -> String {
    let label = match (enabled, state, last_error) {
        (false, _, _) => "byovox: disabled".to_string(),
        (true, IndicatorState::Error, Some(e)) => {
            format!("byovox: error — {}", e.chars().take(60).collect::<String>())
        }
        (true, s, _) => format!("byovox: {}", state_word(s)),
    };
    match update {
        Some(v) => format!("{label} · {v} available"),
        None => label,
    }
}

struct App {
    opts: UiOptions,
    shared: Arc<Mutex<Shared>>,
    hotkey_tx: Sender<HotkeyEvent>,
    config_path: PathBuf,
    log_dir: PathBuf,
    proxy: EventLoopProxy<UserEvent>,
    tray: Option<TrayIcon>,
    items: Option<MenuItems>,
    pill: Option<Pill>,
    cue: Option<Cue>,
    /// Windows only: cpal has no cross-platform device-change hook.
    #[cfg(windows)]
    device_watch: Option<crate::platform::windows::audio::DefaultRenderWatch>,
    reopen: ReopenTimer,
    spin: SpinTimer,
    error_until: Option<Instant>,
    enabled: bool,
    mode: HotkeyMode,
    cues_on: bool,
    shown: IndicatorState,
    update: Announcement,
}

/// May arrive before `resumed` builds the tray; it is put there once.
#[derive(Debug, Default)]
struct Announcement {
    release: Option<crate::update::Release>,
    shown: bool,
}

impl Announcement {
    fn arrived(&mut self, release: crate::update::Release) {
        self.release = Some(release);
        self.shown = false;
    }

    fn due(&mut self, tray_built: bool) -> Option<crate::update::Release> {
        if !tray_built || self.shown {
            return None;
        }
        let release = self.release.clone()?;
        self.shown = true;
        Some(release)
    }

    fn version(&self) -> Option<&str> {
        self.release.as_ref().map(|r| r.version.as_str())
    }
}

struct MenuItems {
    status: MenuItem,
    enabled: MenuItem,
    /// Checked = toggle, cleared = hold.
    mode: CheckMenuItem,
    cues: CheckMenuItem,
    #[cfg(windows)]
    autostart: CheckMenuItem,
    copy_last: MenuItem,
    open_config: MenuItem,
    open_logs: MenuItem,
    run_check: MenuItem,
    open_release: MenuItem,
    quit: MenuItem,
    menu: Menu,
}

impl App {
    fn build_tray(&mut self) -> Result<()> {
        let menu = Menu::new();
        let items = MenuItems {
            status: MenuItem::new("byovox: idle", false, None),
            enabled: MenuItem::new(if self.enabled { "Disable" } else { "Enable" }, true, None),
            mode: CheckMenuItem::new("Toggle mode", true, self.mode == HotkeyMode::Toggle, None),
            cues: CheckMenuItem::new("Audio cues", true, self.cues_on, None),
            #[cfg(windows)]
            autostart: CheckMenuItem::new("Run on startup", true, autostart_on(), None),
            copy_last: MenuItem::new("Copy last transcript", true, None),
            open_config: MenuItem::new("Open config", true, None),
            open_logs: MenuItem::new("Open logs", true, None),
            run_check: MenuItem::new("Run check", true, None),
            open_release: MenuItem::new("Open the release page", true, None),
            quit: MenuItem::new("Quit", true, None),
            menu: menu.clone(),
        };
        menu.append_items(&[
            &items.status,
            &PredefinedMenuItem::separator(),
            &items.enabled,
            &items.mode,
            &items.cues,
            #[cfg(windows)]
            &items.autostart,
            &items.copy_last,
            &items.open_config,
            &items.open_logs,
            &items.run_check,
            &PredefinedMenuItem::separator(),
            &items.quit,
        ])?;
        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu.clone()))
            .with_tooltip(format!("byovox {}", self.opts.version))
            .with_icon(Icon::from_rgba(icon_rgba(IndicatorState::Idle), 32, 32)?)
            .build()?;
        // muda needs a `Fn + Send + Sync` handler and `EventLoopProxy` is not `Sync`, so the proxy rides in a mutex.
        let proxy = Mutex::new(self.proxy.clone());
        let ids = [
            (items.enabled.id().clone(), MenuAction::ToggleEnabled),
            (items.mode.id().clone(), MenuAction::ToggleMode),
            (items.cues.id().clone(), MenuAction::ToggleCues),
            #[cfg(windows)]
            (items.autostart.id().clone(), MenuAction::ToggleAutostart),
            (items.copy_last.id().clone(), MenuAction::CopyLast),
            (items.open_config.id().clone(), MenuAction::OpenConfig),
            (items.open_logs.id().clone(), MenuAction::OpenLogs),
            (items.run_check.id().clone(), MenuAction::RunCheck),
            (items.open_release.id().clone(), MenuAction::OpenRelease),
            (items.quit.id().clone(), MenuAction::Quit),
        ];
        // Runs in the tray's window procedure: a panic would unwind through foreign frames, so a poisoned lock drops the click.
        MenuEvent::set_event_handler(Some(move |ev: MenuEvent| {
            if let Some((_, action)) = ids.iter().find(|(id, _)| *id == ev.id)
                && let Ok(p) = proxy.lock()
            {
                let _ = p.send_event(UserEvent::Menu(*action));
            }
        }));
        self.tray = Some(tray);
        self.items = Some(items);
        self.announce();
        Ok(())
    }

    /// `sound` is false for the timed Error to Idle fall back, which must not add a second tone.
    fn apply(&mut self, state: IndicatorState, sound: bool) {
        tracing::debug!(?state, sound, "indicator");
        let spinning = self.enabled && self.tray.is_some() && state == IndicatorState::Working;
        if let (Some(tray), Some(items)) = (&self.tray, &self.items) {
            // The Cancel that Disable sends returns as an Idle repaint; the cross must stay up.
            let rgba = if !self.enabled {
                disabled_icon_rgba()
            } else if spinning {
                working_frame_rgba(0)
            } else {
                icon_rgba(state)
            };
            if let Ok(icon) = Icon::from_rgba(rgba, 32, 32) {
                let _ = tray.set_icon(Some(icon));
            }
            items.status.set_text(self.status_line(state));
        }
        self.shown = state;
        if let Some(pill) = &mut self.pill {
            pill.show(pill_text(state));
        }
        if sound
            && self.cues_on
            && let Some(cue) = &mut self.cue
        {
            match state {
                IndicatorState::Recording => cue.play(&[(880.0, 70)]),
                // Only a landed dictation sounds like success; taps, cancels and empty transcripts return to Idle silently.
                IndicatorState::Done => cue.play(&[(660.0, 60)]),
                IndicatorState::Uncertain => cue.play(&[(330.0, 180)]),
                IndicatorState::Error => cue.play(&[(440.0, 110), (220.0, 220)]),
                IndicatorState::Idle | IndicatorState::Working => {}
            }
        }
        self.error_until = (state == IndicatorState::Error).then(|| Instant::now() + ERROR_HOLD);
        if spinning {
            self.spin.start(Instant::now());
        } else {
            self.spin.stop();
        }
    }

    /// Open the cue sink and follow the default output endpoint; a sink dies silently with its endpoint (see `Cue`). Idempotent.
    fn open_cues(&mut self) {
        if self.cue.is_none() {
            self.cue = Some(Cue::new(self.opts.cue_volume));
        }
        #[cfg(windows)]
        if self.device_watch.is_none() {
            let proxy = Mutex::new(self.proxy.clone());
            let watch = crate::platform::windows::audio::DefaultRenderWatch::new(move || {
                if let Ok(p) = proxy.lock() {
                    let _ = p.send_event(UserEvent::AudioDeviceChanged);
                }
            });
            match watch {
                Ok(w) => self.device_watch = Some(w),
                Err(e) => tracing::warn!(
                    error = %e,
                    "no output-device notifications; cues will not follow a device change"
                ),
            }
        }
    }

    fn close_cues(&mut self) {
        self.cue = None;
        #[cfg(windows)]
        {
            self.device_watch = None;
        }
        self.reopen = ReopenTimer::default();
    }

    /// Needs an event-loop pass of its own: a tone queued in the pass that opened the sink is lost (see `Cue`).
    fn reopen_cue(&mut self) {
        let Some(cue) = &mut self.cue else {
            return;
        };
        if cue.reopen() {
            // No device name: byovox keeps names out of the running log.
            tracing::info!("cue output re-opened after an audio device change");
        }
    }

    fn status_line(&self, state: IndicatorState) -> String {
        let last_error = shared_of(&self.shared).last_error.clone();
        status_label(self.enabled, state, last_error, self.update.version())
    }

    fn announce(&mut self) {
        let Some(release) = self.update.due(self.tray.is_some() && self.items.is_some()) else {
            return;
        };
        if let Some(items) = &self.items {
            items
                .open_release
                .set_text(format!("Open the byovox {} release page", release.version));
            if let Err(e) = items.menu.insert(&items.open_release, 1) {
                tracing::debug!(error = %e, "the release item could not be added to the menu");
            }
            items.status.set_text(self.status_line(self.shown));
        }
        #[cfg(windows)]
        if release.balloon
            && let Some(tray) = &self.tray
            && let Err(e) = show_balloon(
                tray,
                &format!("byovox {} available", release.version),
                "The tray menu opens its release page.",
            )
        {
            tracing::debug!(error = %e, "the update balloon could not be shown");
        }
    }

    fn menu(&mut self, action: MenuAction, event_loop: &ActiveEventLoop) {
        match action {
            MenuAction::Quit => event_loop.exit(),
            MenuAction::ToggleEnabled => {
                self.enabled = !self.enabled;
                // Cancel before the pipeline can see the flag, so the microphone closes now.
                if !self.enabled {
                    let _ = self.hotkey_tx.send(HotkeyEvent::Cancel);
                }
                shared_of(&self.shared).enabled = self.enabled;
                // Let the swallowing chord's trigger through to the window; only the Windows hook has an arming call.
                #[cfg(windows)]
                crate::platform::windows::hotkey::set_armed(self.enabled);
                tracing::info!(enabled = self.enabled, "dictation toggled from the tray");
                if let Some(items) = &self.items {
                    items
                        .enabled
                        .set_text(if self.enabled { "Disable" } else { "Enable" });
                }
                self.apply(IndicatorState::Idle, false);
            }
            MenuAction::ToggleMode => {
                self.mode = other_mode(self.mode);
                // A mode change mid-recording would strand the microphone (toggle ignores the matching release).
                let _ = self.hotkey_tx.send(HotkeyEvent::Cancel);
                shared_of(&self.shared).mode = self.mode;
                tracing::info!(mode = ?self.mode, "hotkey mode changed from the tray");
                // muda flips the check itself; set it from our own state so mark and pipeline agree.
                if let Some(items) = &self.items {
                    items.mode.set_checked(self.mode == HotkeyMode::Toggle);
                }
            }
            MenuAction::ToggleCues => {
                self.cues_on = !self.cues_on;
                // Open in this click's own pass: a tone queued in the opening pass never reaches the device (see `Cue`).
                if self.cues_on {
                    self.open_cues();
                } else {
                    self.close_cues();
                }
                tracing::info!(cues = self.cues_on, "audio cues toggled from the tray");
                if let Some(items) = &self.items {
                    items.cues.set_checked(self.cues_on);
                }
            }
            #[cfg(windows)]
            MenuAction::ToggleAutostart => {
                use crate::platform::windows::autostart;
                let config = (self.config_path != crate::config::default_path())
                    .then_some(self.config_path.as_path());
                let r = if autostart_on() {
                    autostart::disable()
                } else {
                    autostart::enable(config)
                };
                match r {
                    Ok(()) => {
                        tracing::info!(on = autostart_on(), "autostart toggled from the tray")
                    }
                    Err(e) => tracing::warn!("autostart from the tray: {e:#}"),
                }
                // Set from the Run key so the mark shows what the next logon does, a failed write included.
                if let Some(items) = &self.items {
                    items.autostart.set_checked(autostart_on());
                }
            }
            MenuAction::CopyLast => {
                let text = shared_of(&self.shared).last_transcript.clone();
                copy_last(text, |t| {
                    arboard::Clipboard::new().and_then(|mut cb| cb.set_text(t))
                });
            }
            MenuAction::OpenConfig => open_path(&self.config_path),
            MenuAction::OpenLogs => open_path(&self.log_dir),
            MenuAction::OpenRelease => {
                if let Some(release) = &self.update.release {
                    open_path(Path::new(&release.url));
                }
            }
            MenuAction::RunCheck => {
                // The sibling CLI: the tray runs inside `byovox-daemon`, which has no `check` subcommand.
                if let Ok(exe) = std::env::current_exe() {
                    let _ = std::process::Command::new(crate::daemon::cli_exe(&exe))
                        .arg("check")
                        .arg("--pause")
                        .spawn();
                }
            }
        }
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.tray.is_none() {
            if let Err(e) = self.build_tray() {
                tracing::error!(error = %e, "tray icon failed; continuing without it");
            }
            if self.opts.pill && self.pill.is_none() {
                match Pill::new(event_loop) {
                    Ok(p) => self.pill = Some(p),
                    Err(e) => {
                        tracing::warn!(error = %e, "pill window failed; continuing without it")
                    }
                }
            }
            if self.cues_on {
                self.open_cues();
            }
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Indicator(s) => self.apply(s, true),
            UserEvent::Menu(a) => self.menu(a, event_loop),
            // Scheduled only: `about_to_wait` re-opens once the disconnect burst has gone quiet.
            UserEvent::AudioDeviceChanged => self.reopen.notified(Instant::now()),
            UserEvent::UpdateAvailable(release) => {
                self.update.arrived(release);
                self.announce();
            }
            UserEvent::Quit => event_loop.exit(),
        }
    }

    fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        let Some(pill) = &mut self.pill else {
            return;
        };
        match event {
            WindowEvent::RedrawRequested => pill.draw(),
            // Text is laid out in physical pixels; a DPI change must repaint.
            WindowEvent::ScaleFactorChanged { .. } => pill.window.request_redraw(),
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let now = Instant::now();
        if let Some(t) = self.error_until
            && now >= t
        {
            self.error_until = None;
            self.apply(IndicatorState::Idle, false);
        }
        if self.reopen.due(now) {
            self.reopen_cue();
        }
        if let Some(frame) = self.spin.due(now)
            && let Some(tray) = &self.tray
            && let Ok(icon) = Icon::from_rgba(working_frame_rgba(frame), 32, 32)
        {
            let _ = tray.set_icon(Some(icon));
        }
        let next = [
            self.error_until,
            self.reopen.deadline(),
            self.spin.deadline(),
        ]
        .into_iter()
        .flatten()
        .min();
        event_loop.set_control_flow(match next {
            Some(t) => ControlFlow::WaitUntil(t),
            None => ControlFlow::Wait,
        });
    }
}

/// Blocks until Quit. `hotkey_tx` lets the tray cancel an in-flight recording on disable.
pub fn run(
    event_loop: EventLoop<UserEvent>,
    opts: UiOptions,
    shared: Arc<Mutex<Shared>>,
    hotkey_tx: Sender<HotkeyEvent>,
    config_path: PathBuf,
    log_dir: PathBuf,
) -> Result<()> {
    let proxy = event_loop.create_proxy();
    let (enabled, mode) = {
        let s = shared_of(&shared);
        (s.enabled, s.mode)
    };
    let cues_on = opts.cue;
    let mut app = App {
        opts,
        shared,
        hotkey_tx,
        config_path,
        log_dir,
        proxy,
        tray: None,
        items: None,
        pill: None,
        cue: None,
        #[cfg(windows)]
        device_watch: None,
        reopen: ReopenTimer::default(),
        spin: SpinTimer::default(),
        error_until: None,
        enabled,
        mode,
        cues_on,
        shown: IndicatorState::Idle,
        update: Announcement::default(),
    };
    event_loop.run_app(&mut app).context("event loop")?;
    Ok(())
}

// Windows' own Segoe UI, so no font ships with byovox.
fn system_font() -> Result<Vec<u8>> {
    let windir = std::env::var_os("WINDIR").context("WINDIR is not set")?;
    std::fs::read(Path::new(&windir).join("Fonts").join("segoeui.ttf")).context("Segoe UI")
}

const PILL_W: u32 = 170;
const PILL_H: u32 = 36;
const BG: u32 = 0x0020_2020;
const FG: u32 = 0xF0;

struct Pill {
    window: std::rc::Rc<Window>,
    surface: softbuffer::Surface<std::rc::Rc<Window>, std::rc::Rc<Window>>,
    font: fontdue::Font,
    text: Option<&'static str>,
}

impl Pill {
    /// Created hidden; the first `show` positions it by the cursor before mapping, so it never flashes at the origin.
    fn new(event_loop: &ActiveEventLoop) -> Result<Pill> {
        let attrs = Window::default_attributes()
            .with_title("byovox")
            .with_decorations(false)
            .with_resizable(false)
            .with_visible(false)
            .with_active(false)
            .with_window_level(WindowLevel::AlwaysOnTop)
            .with_inner_size(winit::dpi::LogicalSize::new(PILL_W, PILL_H));
        #[cfg(windows)]
        let attrs = {
            use winit::platform::windows::WindowAttributesExtWindows;
            attrs.with_skip_taskbar(true)
        };
        let window = std::rc::Rc::new(event_loop.create_window(attrs).context("pill window")?);
        deny_activation(&window)?;
        let ctx = softbuffer::Context::new(window.clone()).map_err(|e| anyhow::anyhow!("{e}"))?;
        let surface =
            softbuffer::Surface::new(&ctx, window.clone()).map_err(|e| anyhow::anyhow!("{e}"))?;
        let font = fontdue::Font::from_bytes(system_font()?, fontdue::FontSettings::default())
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let mut pill = Pill {
            window,
            surface,
            font,
            text: None,
        };
        pill.draw();
        Ok(pill)
    }

    fn show(&mut self, text: Option<&'static str>) {
        self.text = text;
        match text {
            Some(_) => {
                if let Some((x, y)) = cursor_position() {
                    self.window
                        .set_outer_position(winit::dpi::PhysicalPosition::new(x + 24, y + 24));
                }
                self.window.set_visible(true);
                self.window.request_redraw();
            }
            None => self.window.set_visible(false),
        }
        // winit recomputes the ex-style on every visibility change, erasing this bit; re-assert it (set_visible runs inline, so after the wipe).
        if let Err(e) = deny_activation(&self.window) {
            tracing::warn!(error = %e, "pill lost its no-activation style");
        }
    }

    fn draw(&mut self) {
        let size = self.window.inner_size();
        let (w, h) = (size.width.max(1), size.height.max(1));
        if self
            .surface
            .resize(w.try_into().unwrap(), h.try_into().unwrap())
            .is_err()
        {
            return;
        }
        let Ok(mut buf) = self.surface.buffer_mut() else {
            return;
        };
        buf.fill(BG);
        if let Some(text) = self.text {
            // Window is logical, buffer physical: scale every layout number or text shrinks as DPI rises.
            let scale = self.window.scale_factor() as f32;
            let mut pen_x = 12.0 * scale;
            let baseline = (24.0 * scale) as i32;
            for ch in text.chars() {
                let (metrics, bitmap) = self.font.rasterize(ch, 16.0 * scale);
                for (i, a) in bitmap.iter().enumerate() {
                    let px = pen_x as i32 + metrics.xmin + (i % metrics.width.max(1)) as i32;
                    let py = baseline - metrics.height as i32 - metrics.ymin
                        + (i / metrics.width.max(1)) as i32;
                    if px >= 0 && py >= 0 && (px as u32) < w && (py as u32) < h {
                        let idx = (py as u32 * w + px as u32) as usize;
                        // Source-over, not replace: overlapping glyph boxes would erase a neighbour's antialiasing.
                        let dst = buf[idx] & 0xFF;
                        let shade = dst + (*a as u32 * (FG.saturating_sub(dst)) / 255);
                        buf[idx] = (shade << 16) | (shade << 8) | shade;
                    }
                }
                pen_x += metrics.advance_width;
            }
        }
        let _ = buf.present();
    }
}

/// Keep the pill out of the activation chain, taskbar and Alt-Tab. Re-assert after every `set_visible`; re-check on a winit bump.
/// At construction a failure is fatal (the caller drops the pill); from `show` the caller only WARNs.
#[cfg(windows)]
fn deny_activation(window: &Window) -> Result<()> {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        GWL_EXSTYLE, GetWindowLongPtrW, SetWindowLongPtrW, WS_EX_APPWINDOW, WS_EX_NOACTIVATE,
        WS_EX_TOOLWINDOW,
    };
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

    let handle = window.window_handle().context("pill window handle")?;
    let RawWindowHandle::Win32(h) = handle.as_raw() else {
        anyhow::bail!("pill window is not a Win32 window");
    };
    let hwnd = HWND(h.hwnd.get() as *mut std::ffi::c_void);
    let wanted = (WS_EX_NOACTIVATE.0 | WS_EX_TOOLWINDOW.0) as isize;
    let unwanted = WS_EX_APPWINDOW.0 as isize;
    // SAFETY: `hwnd` is this window's live handle; both calls touch only its extended style.
    let readback = unsafe {
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, (ex | wanted) & !unwanted);
        GetWindowLongPtrW(hwnd, GWL_EXSTYLE)
    };
    // SetWindowLongPtrW's return is indistinguishable from failure, so read the style back; all three bits must land.
    if readback & wanted != wanted || readback & unwanted != 0 {
        anyhow::bail!(
            "the pill window kept the wrong extended style (0x{readback:08X}): wanted \
             WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW, not WS_EX_APPWINDOW"
        );
    }
    Ok(())
}

#[cfg(windows)]
fn balloon_data(
    hwnd: windows::Win32::Foundation::HWND,
    title: &str,
    text: &str,
) -> windows::Win32::UI::Shell::NOTIFYICONDATAW {
    use windows::Win32::UI::Shell::{NIF_INFO, NIIF_INFO, NOTIFYICONDATAW};
    let mut nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        // tray-icon 0.24.2 has no uID getter; its private counter gives byovox's icon uID 2. A bump that moves it fails the call and logs at DEBUG.
        uID: 2,
        uFlags: NIF_INFO,
        dwInfoFlags: NIIF_INFO,
        ..Default::default()
    };
    let n = nid.szInfoTitle.len() - 1;
    for (d, s) in nid.szInfoTitle[..n].iter_mut().zip(title.encode_utf16()) {
        *d = s;
    }
    let n = nid.szInfo.len() - 1;
    for (d, s) in nid.szInfo[..n].iter_mut().zip(text.encode_utf16()) {
        *d = s;
    }
    nid
}

#[cfg(windows)]
fn show_balloon(tray: &TrayIcon, title: &str, text: &str) -> windows::core::Result<()> {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::Shell::{NIM_MODIFY, Shell_NotifyIconW};
    let nid = balloon_data(HWND(tray.window_handle()), title, text);
    // SAFETY: `nid` is a fully initialised NOTIFYICONDATAW that outlives the call, naming the tray's live window.
    unsafe { Shell_NotifyIconW(NIM_MODIFY, &nid) }.ok()
}

#[cfg(not(windows))]
fn deny_activation(_window: &Window) -> Result<()> {
    Ok(()) // no X11/Wayland/AppKit equivalent yet
}

#[cfg(windows)]
fn autostart_on() -> bool {
    crate::platform::windows::autostart::is_enabled()
        .inspect_err(|e| tracing::warn!("reading autostart: {e:#}"))
        .unwrap_or(false)
}

#[cfg(windows)]
fn cursor_position() -> Option<(i32, i32)> {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;
    let mut p = POINT::default();
    // SAFETY: GetCursorPos writes a POINT.
    unsafe { GetCursorPos(&mut p).ok()? };
    Some((p.x, p.y))
}

#[cfg(not(windows))]
fn cursor_position() -> Option<(i32, i32)> {
    None // no cursor source off Windows; the pill stays at its last position
}

/// A sink opened ahead of any cue, because a tone queued in the pass that opened it never reaches the device; it dies silently with its endpoint.
/// Cues are decoration: every failure is soft, and a failure streak reports once.
struct Cue {
    sink: Option<rodio::MixerDeviceSink>,
    warned: bool,
    gain: f32,
}

impl Cue {
    fn new(gain: f32) -> Cue {
        let mut cue = Cue {
            sink: None,
            warned: false,
            gain,
        };
        if let Err(e) = cue.open() {
            tracing::debug!(error = %e, "audio output not ready at start; the first cue will retry");
        }
        cue
    }

    fn open(&mut self) -> Result<&mut rodio::MixerDeviceSink> {
        if self.sink.is_none() {
            let mut sink = rodio::DeviceSinkBuilder::open_default_sink().context("audio output")?;
            // Without rodio's `tracing` feature its drop notice is a raw `eprintln!`; a tray app must not scribble on stderr.
            sink.log_on_drop(false);
            self.sink = Some(sink);
        }
        Ok(self.sink.as_mut().expect("just opened"))
    }

    fn play(&mut self, notes: &[(f32, u64)]) {
        let outcome = self.open_and_play(notes);
        self.settle(outcome);
    }

    /// A stream whose device went away keeps accepting tones, so only a fresh sink recovers.
    /// Returns whether a working sink came back.
    fn reopen(&mut self) -> bool {
        self.sink = None;
        let outcome = self.open().map(|_| ());
        self.settle(outcome)
    }

    /// A failure drops the handle so the next cue opens a fresh sink, and reports once per streak.
    fn settle(&mut self, outcome: Result<()>) -> bool {
        match outcome {
            Ok(()) => {
                self.warned = false;
                true
            }
            Err(e) => {
                self.sink = None;
                if !self.warned {
                    self.warned = true;
                    tracing::warn!(error = %e, "audio cue unavailable; continuing without it");
                }
                false
            }
        }
    }

    fn open_and_play(&mut self, notes: &[(f32, u64)]) -> Result<()> {
        use rodio::Source;
        let mut at = 0;
        for &(hz, ms) in notes {
            let src = self.tone(hz, ms).delay(Duration::from_millis(at));
            self.open()?.mixer().add(src);
            at += ms;
        }
        Ok(())
    }

    fn tone(&self, hz: f32, ms: u64) -> impl rodio::Source + use<> {
        use rodio::Source;
        rodio::source::SineWave::new(hz)
            .take_duration(Duration::from_millis(ms))
            .amplify(self.gain)
    }
}

/// Runs `write` on a thread of its own; `None` writes nothing. A hung clipboard owner stalls `set_text` for ~5 s.
fn copy_last<W>(text: Option<String>, write: W) -> Option<std::thread::JoinHandle<()>>
where
    W: FnOnce(String) -> Result<(), arboard::Error> + Send + 'static,
{
    let Some(text) = text else {
        tracing::info!("no transcript held yet; clipboard left as it was");
        return None;
    };
    Some(std::thread::spawn(move || match write(text) {
        Ok(()) => tracing::info!("last transcript copied to the clipboard"),
        Err(e) => tracing::warn!(error = %e, "copying the last transcript failed"),
    }))
}

fn open_path(p: &Path) {
    #[cfg(windows)]
    let _ = std::process::Command::new("explorer").arg(p).spawn();
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(p).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let _ = std::process::Command::new("xdg-open").arg(p).spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_last_returns_while_the_clipboard_write_is_still_blocked() {
        let (release, blocked) = std::sync::mpsc::channel::<()>();
        let (wrote, written) = std::sync::mpsc::channel::<String>();
        let handle = copy_last(Some("שלום\nмир".into()), move |t| {
            let _ = blocked.recv_timeout(Duration::from_secs(2));
            wrote.send(t).unwrap();
            Ok(())
        })
        .expect("a held transcript is written");
        assert!(
            written.try_recv().is_err(),
            "copy_last returned only after the write finished"
        );
        release.send(()).unwrap();
        handle.join().unwrap();
        assert_eq!(written.recv().unwrap(), "שלום\nмир");
    }

    #[test]
    fn copy_last_with_nothing_held_never_writes() {
        let handle = copy_last(None, |_| panic!("nothing held, nothing written"));
        assert!(handle.is_none());
    }

    #[test]
    fn icon_rgba_is_32x32_and_state_coloured() {
        let idle = icon_rgba(IndicatorState::Idle);
        assert_eq!(idle.len(), 32 * 32 * 4);
        let rec = icon_rgba(IndicatorState::Recording);
        let c = (8 * 32 + 16) * 4;
        assert!(rec[c] > rec[c + 1] && rec[c] > rec[c + 2]);
        assert!(idle[c] == idle[c + 1] && idle[c] == idle[c + 2] && idle[c + 3] == 255);
        let corner = (2 * 32 + 2) * 4;
        assert_eq!(idle[corner + 3], 0);
        let (mut min_x, mut max_x, mut min_y, mut max_y) = (31, 0, 31, 0);
        for y in 0..32 {
            for x in 0..32 {
                if idle[(y * 32 + x) * 4 + 3] == 255 {
                    min_x = min_x.min(x);
                    max_x = max_x.max(x);
                    min_y = min_y.min(y);
                    max_y = max_y.max(y);
                }
            }
        }
        assert_eq!((min_x, max_x, min_y, max_y), (6, 25, 6, 26));
    }

    #[test]
    fn the_disabled_icon_is_a_red_cross_on_the_idle_mic() {
        let off = disabled_icon_rgba();
        assert_eq!(off.len(), 32 * 32 * 4);
        let c = (16 * 32 + 16) * 4;
        assert!(off[c] > off[c + 1] && off[c] > off[c + 2]);
        let cap = (9 * 32 + 19) * 4;
        assert!(off[cap] == off[cap + 1] && off[cap] == off[cap + 2] && off[cap + 3] == 255);
        let side = (22 * 32 + 16) * 4;
        assert!(off[side] == off[side + 1] && off[side] == off[side + 2] && off[side + 3] == 255);
    }

    #[test]
    fn the_status_line_names_a_newer_release_in_every_state() {
        let error = || Some("stt transport: refused".to_string());
        assert_eq!(
            status_label(true, IndicatorState::Idle, None, None),
            "byovox: idle"
        );
        assert_eq!(
            status_label(true, IndicatorState::Idle, None, Some("0.4.0")),
            "byovox: idle · 0.4.0 available"
        );
        assert_eq!(
            status_label(false, IndicatorState::Recording, None, Some("0.4.0")),
            "byovox: disabled · 0.4.0 available"
        );
        assert_eq!(
            status_label(true, IndicatorState::Error, error(), Some("0.4.0")),
            "byovox: error — stt transport: refused · 0.4.0 available"
        );
        assert_eq!(
            status_label(true, IndicatorState::Error, error(), None),
            "byovox: error — stt transport: refused"
        );
    }

    #[test]
    fn a_release_waits_for_the_tray_and_is_announced_once() {
        let release = crate::update::Release {
            version: "0.4.0".into(),
            url: "https://github.com/iteramen/byovox/releases/tag/v0.4.0".into(),
            balloon: true,
        };
        let mut early = Announcement::default();
        assert_eq!(early.due(true), None, "nothing has arrived");
        early.arrived(release.clone());
        assert_eq!(early.version(), Some("0.4.0"));
        assert_eq!(early.due(false), None, "announced before there was a tray");
        assert_eq!(early.due(true), Some(release.clone()));
        assert_eq!(early.due(true), None, "announced twice");

        let mut late = Announcement::default();
        late.arrived(release.clone());
        assert_eq!(late.due(true), Some(release));
        assert_eq!(late.due(true), None, "announced twice");
    }

    #[cfg(windows)]
    #[test]
    fn the_balloon_targets_uid_2_and_keeps_its_terminating_nuls() {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::Shell::{NIF_INFO, NIIF_INFO};
        let nid = balloon_data(HWND::default(), "byovox 0.4.0 available", "Open it.");
        assert_eq!(nid.uID, 2);
        assert_eq!(nid.uFlags, NIF_INFO);
        assert_eq!(nid.dwInfoFlags, NIIF_INFO);
        let title: Vec<u16> = "byovox 0.4.0 available".encode_utf16().collect();
        assert_eq!(&nid.szInfoTitle[..title.len()], &title[..]);
        assert_eq!(nid.szInfoTitle[title.len()], 0);

        let long = "x".repeat(500);
        let nid = balloon_data(HWND::default(), &long, &long);
        assert_eq!(nid.szInfoTitle[62], u16::from(b'x'));
        assert_eq!(nid.szInfoTitle[63], 0, "the title lost its NUL");
        assert_eq!(nid.szInfo[254], u16::from(b'x'));
        assert_eq!(nid.szInfo[255], 0, "the text lost its NUL");
    }

    #[test]
    fn the_mode_item_flips_between_hold_and_toggle() {
        assert_eq!(other_mode(HotkeyMode::Hold), HotkeyMode::Toggle);
        assert_eq!(other_mode(HotkeyMode::Toggle), HotkeyMode::Hold);
        assert_eq!(other_mode(other_mode(HotkeyMode::Hold)), HotkeyMode::Hold);
    }

    #[test]
    fn a_burst_of_device_changes_coalesces_into_one_reopen() {
        let t0 = Instant::now();
        let at = |ms| t0 + Duration::from_millis(ms);
        let mut timer = ReopenTimer::default();

        assert!(!timer.due(at(0)));
        assert_eq!(timer.deadline(), None);

        for ms in [0, 50, 100] {
            timer.notified(at(ms));
            assert!(!timer.due(at(ms)), "re-opened mid-burst at {ms} ms");
        }
        assert_eq!(timer.deadline(), Some(at(500)));
        assert!(!timer.due(at(499)));

        assert!(timer.due(at(500)));
        assert!(!timer.due(at(5_000)));
        assert_eq!(timer.deadline(), None);
    }

    #[test]
    fn a_flap_faster_than_the_window_still_reopens_once_per_window() {
        let t0 = Instant::now();
        let at = |ms| t0 + Duration::from_millis(ms);
        let mut timer = ReopenTimer::default();

        let flap: u64 = 100;
        let mut reopens = Vec::new();
        for ms in (0..10_000).step_by(flap as usize) {
            timer.notified(at(ms));
            if timer.due(at(ms)) {
                reopens.push(ms);
            }
        }

        let settle = CUE_REOPEN_SETTLE.as_millis() as u64;
        assert!(
            !reopens.is_empty(),
            "a continuous flap never re-opened at all"
        );
        assert_eq!(
            reopens[0], settle,
            "the first re-open waited more than one window"
        );
        for pair in reopens.windows(2) {
            let gap = pair[1] - pair[0];
            assert!(gap >= settle, "two re-opens inside one window: {reopens:?}");
            assert!(gap <= settle + flap, "the re-open drifted: {reopens:?}");
        }
    }

    #[test]
    fn working_frames_orbit_a_dot_around_the_untouched_glyph() {
        let glyph = icon_rgba(IndicatorState::Working);
        let mut centroids = Vec::new();
        for frame in 0..SPIN_FRAMES {
            let px = working_frame_rgba(frame);
            assert_eq!(px.len(), 32 * 32 * 4);
            let (mut n, mut sx, mut sy) = (0.0f32, 0.0f32, 0.0f32);
            for y in 0..32 {
                for x in 0..32 {
                    let i = (y * 32 + x) * 4;
                    if glyph[i + 3] == 255 {
                        assert_eq!(px[i..i + 4], glyph[i..i + 4], "frame {frame} ({x}, {y})");
                    }
                    if px[i..i + 4] != glyph[i..i + 4] {
                        assert_eq!(
                            px[i..i + 4],
                            [230, 160, 30, 255],
                            "frame {frame} ({x}, {y})"
                        );
                        n += 1.0;
                        sx += x as f32;
                        sy += y as f32;
                    }
                }
            }
            assert!(n > 0.0, "frame {frame} drew no dot");
            let (mx, my) = (sx / n, sy / n);
            // A dot clipped by the canvas edge would pull the centroid inwards.
            let a = frame as f32 * std::f32::consts::TAU / SPIN_FRAMES as f32;
            let (ex, ey) = (15.5 + 13.5 * a.sin(), 16.0 - 13.5 * a.cos());
            assert!(
                (mx - ex).abs() < 0.5 && (my - ey).abs() < 0.5,
                "frame {frame}: centroid ({mx}, {my}), expected ({ex}, {ey})"
            );
            centroids.push((mx, my));
        }
        assert!(centroids[0].1 < 6.0);
        for i in 0..centroids.len() {
            for j in i + 1..centroids.len() {
                let (a, b) = (centroids[i], centroids[j]);
                assert!(
                    (a.0 - b.0).abs() + (a.1 - b.1).abs() > 1.0,
                    "frames {i} and {j} share a centroid"
                );
            }
        }
        assert_eq!(working_frame_rgba(SPIN_FRAMES), working_frame_rgba(0));
    }

    #[test]
    fn the_spin_timer_steps_one_frame_per_period_and_never_bursts() {
        let t0 = Instant::now();
        let mut spin = SpinTimer::default();
        assert_eq!(spin.deadline(), None);
        assert_eq!(spin.due(t0), None);

        spin.start(t0);
        assert_eq!(spin.deadline(), Some(t0 + SPIN_PERIOD));
        assert_eq!(spin.due(t0 + SPIN_PERIOD / 2), None);
        assert_eq!(spin.deadline(), Some(t0 + SPIN_PERIOD));

        let mut frames = Vec::new();
        for _ in 0..9 {
            let at = spin.deadline().unwrap();
            frames.push(spin.due(at).unwrap());
        }
        assert_eq!(frames, [1, 2, 3, 4, 5, 6, 7, 0, 1]);

        let now = spin.deadline().unwrap() + SPIN_PERIOD * 10;
        assert_eq!(spin.due(now), Some(2));
        assert_eq!(spin.deadline(), Some(now + SPIN_PERIOD));
        assert_eq!(spin.due(now), None);

        spin.stop();
        assert_eq!(spin.deadline(), None);
        assert_eq!(spin.due(now + SPIN_PERIOD * 100), None);
    }

    #[test]
    fn a_cue_tone_peaks_at_the_configured_gain() {
        let cue = Cue::new(0.3);
        let peak = cue.tone(440.0, 50).fold(0.0f32, |m, s| m.max(s.abs()));
        assert!((0.299..=0.3).contains(&peak), "peak {peak}, expected 0.3");
    }

    // `#[ignore]`d: opens the default output device and makes a noise; run with `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn a_reopened_cue_plays_and_clears_the_failure_streak() {
        let mut cue = Cue::new(crate::config::IndicatorConfig::default().cue_volume as f32);
        assert!(cue.sink.is_some(), "no default output device on this box");
        cue.play(&[(880.0, 30)]);
        assert!(!cue.warned, "a working device reported a failure streak");

        cue.reopen();
        assert!(cue.sink.is_some(), "the re-open produced no sink");
        cue.play(&[(660.0, 30)]);
        assert!(!cue.warned, "the re-opened sink reported a failure streak");

        // A reported streak: a good re-open must re-arm the WARN.
        cue.warned = true;
        cue.reopen();
        assert!(
            cue.sink.is_some() && !cue.warned,
            "the streak was not cleared"
        );
    }

    #[cfg(windows)]
    #[test]
    fn the_system_font_has_every_pill_glyph() {
        let font =
            fontdue::Font::from_bytes(system_font().unwrap(), fontdue::FontSettings::default())
                .unwrap();
        for state in [
            IndicatorState::Recording,
            IndicatorState::Working,
            IndicatorState::Error,
        ] {
            for ch in pill_text(state)
                .unwrap()
                .chars()
                .filter(|c| !c.is_whitespace())
            {
                assert_ne!(font.lookup_glyph_index(ch), 0, "{ch:?} missing");
            }
        }
    }

    #[test]
    fn pill_text_per_state() {
        assert_eq!(pill_text(IndicatorState::Recording), Some("●  recording"));
        assert_eq!(pill_text(IndicatorState::Working), Some("…  working"));
        assert_eq!(pill_text(IndicatorState::Idle), None);
        assert_eq!(pill_text(IndicatorState::Uncertain), None);
        assert_eq!(
            icon_rgba(IndicatorState::Uncertain),
            icon_rgba(IndicatorState::Idle)
        );
    }
}
