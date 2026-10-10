use std::sync::Mutex;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows::Win32::UI::Accessibility::{
    CUIAutomation8, IUIAutomation, IUIAutomation2, IUIAutomationElement, IUIAutomationTextPattern,
    IUIAutomationValuePattern, UIA_TextPatternId, UIA_ValuePatternId,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE,
    MOUSEINPUT, SendInput, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GUITHREADINFO, GetClassNameW, GetForegroundWindow, GetGUIThreadInfo, GetWindowThreadProcessId,
    IsIconic, IsWindow, IsWindowVisible, SW_RESTORE, SetForegroundWindow, ShowWindow,
};
use windows::core::Interface;

use super::audio::Apartment;
use super::hotkey::INJECT_MARKER;
use crate::inject::{Inject, Target};

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum KeyEvent {
    Unicode { unit: u16, up: bool },
    Vk { vk: u16, up: bool },
}

pub fn key_events(text: &str) -> Vec<KeyEvent> {
    let mut out = Vec::with_capacity(text.len() * 2);
    for ch in text.chars() {
        if ch == '\n' || ch == '\r' {
            continue;
        }
        let mut units = [0u16; 2];
        for unit in ch.encode_utf16(&mut units) {
            out.push(KeyEvent::Unicode {
                unit: *unit,
                up: false,
            });
            out.push(KeyEvent::Unicode {
                unit: *unit,
                up: true,
            });
        }
    }
    out
}

fn to_input(ev: KeyEvent) -> INPUT {
    let ki = match ev {
        KeyEvent::Unicode { unit, up } => KEYBDINPUT {
            wVk: VIRTUAL_KEY(0),
            wScan: unit,
            dwFlags: if up {
                KEYEVENTF_UNICODE | KEYEVENTF_KEYUP
            } else {
                KEYEVENTF_UNICODE
            },
            time: 0,
            dwExtraInfo: INJECT_MARKER,
        },
        KeyEvent::Vk { vk, up } => KEYBDINPUT {
            wVk: VIRTUAL_KEY(vk),
            wScan: 0,
            dwFlags: if up {
                KEYEVENTF_KEYUP
            } else {
                Default::default()
            },
            time: 0,
            dwExtraInfo: INJECT_MARKER,
        },
    };
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki },
    }
}

/// Events per `SendInput` call. Must be at least 4 — the longest character — or `chunks`
/// could not find a boundary to end on.
const CHUNK: usize = 64;

fn is_boundary(ev: KeyEvent) -> bool {
    match ev {
        KeyEvent::Unicode { unit, up } => !up && !(0xDC00..=0xDFFF).contains(&unit),
        KeyEvent::Vk { up, .. } => !up,
    }
}

/// Split the plan into `SendInput` batches. Consecutive calls are not atomic, so a batch never
/// ends inside a character (a key-down/up pair or a surrogate pair).
fn chunks(events: &[KeyEvent]) -> Vec<&[KeyEvent]> {
    let mut out = Vec::new();
    let mut start = 0;
    while start < events.len() {
        let mut end = (start + CHUNK).min(events.len());
        while end < events.len() && !is_boundary(events[end]) {
            end -= 1;
        }
        out.push(&events[start..end]);
        start = end;
    }
    out
}

fn send(events: &[KeyEvent]) -> Result<(), String> {
    for chunk in chunks(events) {
        let inputs: Vec<INPUT> = chunk.iter().copied().map(to_input).collect();
        // SAFETY: inputs is a valid array of INPUT for the call's duration.
        let sent = unsafe { SendInput(&inputs, size_of::<INPUT>() as i32) };
        if sent as usize != inputs.len() {
            return Err(format!(
                "SendInput inserted {sent}/{} events (elevated window?)",
                inputs.len()
            ));
        }
    }
    Ok(())
}

pub struct TypeInject;
impl Inject for TypeInject {
    fn name(&self) -> &'static str {
        "type"
    }
    fn inject(&mut self, text: &str) -> Result<(), String> {
        send(&key_events(text))
    }
}

fn ctrl_v() -> Vec<KeyEvent> {
    vec![
        KeyEvent::Vk {
            vk: 0x11,
            up: false,
        },
        KeyEvent::Vk {
            vk: 0x56,
            up: false,
        },
        KeyEvent::Vk { vk: 0x56, up: true },
        KeyEvent::Vk { vk: 0x11, up: true },
    ]
}

pub struct PasteInject;
impl Inject for PasteInject {
    fn name(&self) -> &'static str {
        "paste"
    }
    fn inject(&mut self, text: &str) -> Result<(), String> {
        let mut cb = arboard::Clipboard::new().map_err(|e| format!("clipboard: {e}"))?;
        let previous = cb.get_text().ok();
        cb.set_text(text)
            .map_err(|e| format!("clipboard set: {e}"))?;
        let sent = send(&ctrl_v());
        std::thread::sleep(Duration::from_millis(150));
        if let Some(p) = previous
            && let Err(e) = cb.set_text(p)
        {
            tracing::warn!(error = %e, "could not restore the previous clipboard text");
        }
        sent
    }
}

pub struct ClipboardOnlyInject;
impl Inject for ClipboardOnlyInject {
    fn name(&self) -> &'static str {
        "clipboard-only"
    }
    fn inject(&mut self, text: &str) -> Result<(), String> {
        arboard::Clipboard::new()
            .and_then(|mut cb| cb.set_text(text))
            .map_err(|e| format!("clipboard: {e}"))
    }
}

const SHELL_CLASSES: [&str; 4] = [
    "Progman",
    "WorkerW",
    "Shell_TrayWnd",
    "Shell_SecondaryTrayWnd",
];

/// Whether the focused element takes text, by UI Automation: an editable value is a text box; a
/// text pattern with no value is a terminal. Read-only or neither is refused; no UIA provider reads as not text.
fn focus_accepts_text() -> Result<bool, String> {
    let _com = Apartment::enter()?;
    // SAFETY: plain COM calls on interfaces this function owns, released before `_com` leaves the apartment.
    unsafe {
        let focused = automation()?
            .GetFocusedElement()
            .map_err(|e| format!("focused element: {e}"))?;
        if let Ok(value) =
            focused.GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId)
        {
            let read_only = value
                .CurrentIsReadOnly()
                .map_err(|e| format!("value pattern: {e}"))?;
            return Ok(!read_only.as_bool());
        }
        Ok(focused
            .GetCurrentPatternAs::<IUIAutomationTextPattern>(UIA_TextPatternId)
            .is_ok())
    }
}

fn focus_takes_text() -> bool {
    let by_uia = focus_accepts_text().unwrap_or_else(|e| {
        tracing::debug!(error = %e, "UI Automation could not read the focused element");
        false
    });
    by_uia || caret_shown()
}

fn caret_shown() -> bool {
    let mut info = GUITHREADINFO {
        cbSize: size_of::<GUITHREADINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: `info` is a live GUITHREADINFO with its size set; a null foreground yields thread 0, which the call refuses.
    unsafe {
        let tid = GetWindowThreadProcessId(GetForegroundWindow(), None);
        GetGUIThreadInfo(tid, &mut info).is_ok() && !info.hwndCaret.is_invalid()
    }
}

fn automation() -> Result<IUIAutomation, String> {
    // SAFETY: creates an in-process COM object the caller owns.
    let uia: IUIAutomation =
        unsafe { CoCreateInstance(&CUIAutomation8, None, CLSCTX_INPROC_SERVER) }
            .map_err(|e| format!("UI Automation: {e}"))?;
    // Only CUIAutomation8 implements IUIAutomation2; the timeouts keep a hung target from
    // holding the dictation for the 20 s default.
    if let Ok(uia2) = uia.cast::<IUIAutomation2>() {
        // SAFETY: plain setters on a live interface.
        unsafe {
            let _ = uia2.SetConnectionTimeout(1000);
            let _ = uia2.SetTransactionTimeout(1000);
        }
    }
    Ok(uia)
}

/// The window an element lives in, by UIA ancestry, not the foreground (WhatsApp's box is a hidden WebView2).
/// # Safety
/// The caller holds the apartment both interfaces were made in.
unsafe fn top_level(uia: &IUIAutomation, element: &IUIAutomationElement) -> Option<HWND> {
    unsafe {
        let root = uia.GetRootElement().ok()?;
        let walker = uia.RawViewWalker().ok()?;
        let mut current = element.clone();
        for _ in 0..64 {
            let parent = walker.GetParentElement(&current).ok()?;
            if uia.CompareElements(&parent, &root).ok()?.as_bool() {
                let hwnd = current.CurrentNativeWindowHandle().ok()?;
                return (!hwnd.is_invalid()).then_some(hwnd);
            }
            current = parent;
        }
        None
    }
}

struct Remembered {
    window: isize,
    foreground: isize,
    element: IUIAutomationElement,
    _com: Apartment,
}
// SAFETY: made by `current`, dropped by a later `current` or `restore` on the pipeline's one thread.
unsafe impl Send for Remembered {}

fn remember(foreground: isize) -> Result<Remembered, String> {
    let com = Apartment::enter()?;
    let uia = automation()?;
    // SAFETY: COM calls inside the apartment `com` holds; `element` is released before it.
    let (element, window) = unsafe {
        let element = uia
            .GetFocusedElement()
            .map_err(|e| format!("focused element: {e}"))?;
        let window = top_level(&uia, &element).map_or(foreground, |h| h.0 as isize);
        (element, window)
    };
    Ok(Remembered {
        window,
        foreground,
        element,
        _com: com,
    })
}

fn cloaked(hwnd: HWND) -> bool {
    let mut v = 0u32;
    // SAFETY: writes one DWORD into `v`, whose size is passed.
    let _ = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            (&raw mut v).cast(),
            size_of::<u32>() as u32,
        )
    };
    v != 0
}

#[derive(Default)]
pub struct WinTarget {
    remembered: Mutex<Option<Remembered>>,
}
impl Target for WinTarget {
    fn current(&self) -> Option<isize> {
        // SAFETY: takes no arguments; null when no window is in the foreground.
        let hwnd = unsafe { GetForegroundWindow() };
        if hwnd.is_invalid() {
            return None;
        }
        let mut class = [0u16; 64];
        // SAFETY: the wrapper passes the slice's own length, so the call cannot overrun it.
        let n = unsafe { GetClassNameW(hwnd, &mut class) };
        let class = String::from_utf16_lossy(&class[..n.max(0) as usize]);
        if SHELL_CLASSES.contains(&class.as_str()) {
            return None;
        }
        let foreground = hwnd.0 as isize;
        let remembered = remember(foreground)
            .inspect_err(|e| tracing::debug!(error = %e, "could not remember the focused element"))
            .ok();
        let window = remembered.as_ref().map_or(foreground, |r| r.window);
        *self.remembered.lock().unwrap() = remembered;
        Some(window)
    }

    fn accepts_text(&self) -> bool {
        focus_takes_text()
    }

    fn restore(&self, window: isize) -> Result<(), String> {
        let remembered = self
            .remembered
            .lock()
            .unwrap()
            .take()
            .filter(|r| r.window == window);
        let hwnd = HWND(window as *mut _);
        let in_front = |fg: HWND| {
            (fg == hwnd || remembered.as_ref().is_some_and(|r| r.foreground == fg.0 as isize))
                // SAFETY: accepts any handle value.
                && unsafe { IsWindowVisible(fg) }.as_bool()
                && !cloaked(fg)
        };
        // SAFETY: every call accepts any handle value, a stale one included; `IsWindow` tells a closed window apart.
        unsafe {
            if in_front(GetForegroundWindow()) {
                return Ok(());
            }
            // Hidden or cloaked is closed too: an app that closes to the tray only hides its window, which then takes the foreground.
            if !IsWindow(Some(hwnd)).as_bool() || !IsWindowVisible(hwnd).as_bool() || cloaked(hwnd)
            {
                return Err("the window dictated into was closed".into());
            }
            if IsIconic(hwnd).as_bool() {
                let _ = ShowWindow(hwnd, SW_RESTORE);
            }
            // The foreground lock refuses a background process unless it sent the last input; an empty mouse input fixes that.
            let nudge = INPUT {
                r#type: INPUT_MOUSE,
                Anonymous: INPUT_0 {
                    mi: MOUSEINPUT {
                        dwExtraInfo: INJECT_MARKER,
                        ..Default::default()
                    },
                },
            };
            SendInput(&[nudge], size_of::<INPUT>() as i32);
            let _ = SetForegroundWindow(hwnd);
        }
        let deadline = Instant::now() + Duration::from_millis(500);
        // SAFETY: as above.
        while !in_front(unsafe { GetForegroundWindow() }) {
            if Instant::now() > deadline {
                return Err("Windows would not bring back the window dictated into".into());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        // A restored window keeps its focus; only where that takes no text is the press-time element handed focus.
        if let Some(r) = remembered
            && !focus_takes_text()
            // SAFETY: a COM call on an element this thread made, inside the apartment `r` holds.
            && let Err(e) = unsafe { r.element.SetFocus() }
        {
            tracing::debug!(error = %e, "could not put focus back on the element dictated into");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::Accessibility::TreeScope_Descendants;
    use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, GetWindowTextW};
    use windows::core::PCWSTR;

    #[test]
    fn plans_unicode_units_and_never_a_virtual_key() {
        let ev = key_events("a\n\rb");
        assert_eq!(
            ev,
            vec![
                KeyEvent::Unicode {
                    unit: 'a' as u16,
                    up: false
                },
                KeyEvent::Unicode {
                    unit: 'a' as u16,
                    up: true
                },
                KeyEvent::Unicode {
                    unit: 'b' as u16,
                    up: false
                },
                KeyEvent::Unicode {
                    unit: 'b' as u16,
                    up: true
                },
            ]
        );
        assert!(!ev.iter().any(|e| matches!(e, KeyEvent::Vk { .. })));
    }

    #[test]
    fn non_bmp_becomes_surrogate_pairs() {
        let ev = key_events("😀");
        let units: Vec<u16> = ev
            .iter()
            .filter_map(|e| match e {
                KeyEvent::Unicode { unit, up: false } => Some(*unit),
                _ => None,
            })
            .collect();
        assert_eq!(units, vec![0xD83D, 0xDE00]);
    }

    #[test]
    fn hebrew_and_cyrillic_are_plain_units() {
        assert_eq!(key_events("א").len(), 2);
        assert_eq!(key_events("ю").len(), 2);
    }

    #[test]
    fn every_planned_input_carries_the_inject_marker() {
        let planned: Vec<KeyEvent> = key_events("a\n😀").into_iter().chain(ctrl_v()).collect();
        assert!(!planned.is_empty());
        for ev in planned {
            let input = to_input(ev);
            // SAFETY: `to_input` just wrote the `ki` arm of this union.
            let extra = unsafe { input.Anonymous.ki.dwExtraInfo };
            assert_eq!(extra, INJECT_MARKER, "{ev:?} lost the marker");
        }
    }

    #[test]
    fn a_batch_never_ends_inside_a_character() {
        let ev = key_events(&format!("a{}", "😀".repeat(40)));
        assert_eq!(ev.len(), 162);
        assert_ne!(ev.len() % CHUNK, 0);

        let batches = chunks(&ev);
        let flat: Vec<KeyEvent> = batches.iter().flat_map(|b| b.iter().copied()).collect();
        assert_eq!(flat, ev, "chunking must not lose or reorder events");

        let is_unit = |e: &&KeyEvent, range: std::ops::RangeInclusive<u16>| matches!(e, KeyEvent::Unicode { unit, .. } if range.contains(unit));
        for b in &batches {
            assert!(
                !b.is_empty() && b.len() <= CHUNK,
                "bad batch length {}",
                b.len()
            );
            assert!(is_boundary(b[0]), "batch starts mid-character: {:?}", b[0]);
            assert_eq!(b.len() % 2, 0, "batch splits a down/up pair");
            let highs = b.iter().filter(|e| is_unit(e, 0xD800..=0xDBFF)).count();
            let lows = b.iter().filter(|e| is_unit(e, 0xDC00..=0xDFFF)).count();
            assert_eq!(highs, lows, "a surrogate pair straddles a batch boundary");
        }
    }

    #[test]
    fn the_paste_chord_is_one_batch() {
        assert_eq!(chunks(&ctrl_v()).len(), 1);
    }

    #[test]
    fn nothing_to_type_is_no_batches_at_all() {
        assert!(chunks(&key_events("")).is_empty());
        assert!(chunks(&key_events("\r")).is_empty());
    }

    #[test]
    fn a_window_that_is_not_there_is_refused() {
        let e = WinTarget::default().restore(1).unwrap_err();
        assert!(e.contains("closed"), "{e}");
    }

    #[test]
    fn cloaked_reads_the_flag_dwm_holds() {
        use windows::Win32::Graphics::Dwm::{DWMWA_CLOAK, DwmSetWindowAttribute};
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DestroyWindow, WINDOW_EX_STYLE, WS_OVERLAPPEDWINDOW,
        };
        use windows::core::{BOOL, w};

        let cloak = |hwnd, on: bool| {
            let v = BOOL::from(on);
            // SAFETY: reads one BOOL from `v`, whose size is passed.
            unsafe {
                DwmSetWindowAttribute(
                    hwnd,
                    DWMWA_CLOAK,
                    (&raw const v).cast(),
                    size_of::<BOOL>() as u32,
                )
            }
            .expect("DWMWA_CLOAK");
        };
        // SAFETY: a plain top-level STATIC window, never shown, destroyed below.
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                w!("byovox-cloaked-test"),
                WS_OVERLAPPEDWINDOW,
                0,
                0,
                100,
                100,
                None,
                None,
                None,
                None,
            )
        }
        .expect("CreateWindowExW");
        assert!(!cloaked(hwnd), "a fresh window read as cloaked");
        cloak(hwnd, true);
        let on = cloaked(hwnd);
        cloak(hwnd, false);
        let off = !cloaked(hwnd);
        // SAFETY: the window this test created, on this thread.
        let _ = unsafe { DestroyWindow(hwnd) };
        assert!(on, "a cloaked window read as not cloaked");
        assert!(off, "an uncloaked window still read as cloaked");
    }

    #[test]
    fn the_automation_client_takes_its_timeouts() {
        let _com = Apartment::enter().expect("apartment");
        let uia = automation().expect("automation");
        let uia2 = uia
            .cast::<IUIAutomation2>()
            .expect("the client does not implement IUIAutomation2");
        // SAFETY: plain getters on a live interface.
        let (connection, transaction) =
            unsafe { (uia2.ConnectionTimeout(), uia2.TransactionTimeout()) };
        assert_eq!(connection.expect("connection timeout"), 1000);
        assert_eq!(transaction.expect("transaction timeout"), 1000);
    }

    #[test]
    #[ignore]
    fn the_console_running_the_test_accepts_text() {
        assert!(WinTarget::default().accepts_text());
    }

    #[test]
    #[ignore]
    fn restore_takes_focus_back_from_another_window() {
        let target = WinTarget::default();
        let before = target.current().expect("a foreground window to return to");
        let mut other = std::process::Command::new("charmap.exe")
            .spawn()
            .expect("charmap");
        let deadline = Instant::now() + Duration::from_secs(5);
        let foreground = || unsafe { GetForegroundWindow() }.0 as isize;
        while foreground() == before && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        let moved = foreground() != before;
        let restored = target.restore(before);
        let after = target.current();
        let _ = other.kill();
        let _ = other.wait();
        assert!(moved, "Character Map never took the foreground");
        restored.expect("restore");
        assert_eq!(after, Some(before));
    }

    struct Form(std::process::Child);
    impl Drop for Form {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn open_form(title: &str, extra: &str, on_shown: &str) -> Form {
        let script = r#"
            Add-Type -AssemblyName System.Windows.Forms
            Add-Type -MemberDefinition '[DllImport("dwmapi.dll")] public static extern int DwmSetWindowAttribute(IntPtr h, int a, ref int v, int s); [DllImport("user32.dll")] public static extern bool CreateCaret(IntPtr h, IntPtr b, int w, int ht);' -Name Native -Namespace Probe
            $form = New-Object System.Windows.Forms.Form
            $form.Text = 'TITLE'
            $first = New-Object System.Windows.Forms.TextBox
            $first.Name = 'first'
            $first.Location = '10,10'
            $second = EXTRA
            $second.Name = 'second'
            $second.Location = '10,60'
            $form.Controls.AddRange(@($first, $second))
            $form.Add_Shown({ $form.Activate(); $first.Focus(); ON_SHOWN })
            [System.Windows.Forms.Application]::Run($form)
        "#
        .replace("TITLE", title)
        .replace("EXTRA", extra)
        .replace("ON_SHOWN", on_shown);
        Form(
            std::process::Command::new("powershell.exe")
                .args(["-NoProfile", "-Command", &script])
                .spawn()
                .expect("powershell"),
        )
    }

    fn unique_title(name: &str) -> String {
        format!("byovox-test-{}-{name}", std::process::id())
    }

    fn foreground_title() -> String {
        let mut buf = [0u16; 256];
        // SAFETY: the wrapper passes the slice's own length; a null foreground reads as empty.
        let n = unsafe { GetWindowTextW(GetForegroundWindow(), &mut buf) };
        String::from_utf16_lossy(&buf[..n.max(0) as usize])
    }

    fn wait_for(done: impl Fn() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        done()
    }

    unsafe fn control_named(
        uia: &IUIAutomation,
        root: &IUIAutomationElement,
        id: &str,
    ) -> IUIAutomationElement {
        unsafe {
            let all = root
                .FindAll(TreeScope_Descendants, &uia.CreateTrueCondition().unwrap())
                .unwrap();
            (0..all.Length().unwrap())
                .map(|i| all.GetElement(i).unwrap())
                .find(|e| e.CurrentAutomationId().is_ok_and(|a| a == id))
                .unwrap_or_else(|| panic!("no control named {id}"))
        }
    }

    fn focused_after_restore(name: &str, extra: &str) -> String {
        let _com = Apartment::enter().expect("apartment");
        let title = unique_title(name);
        let form = open_form(&title, extra, "");
        assert!(
            wait_for(|| foreground_title() == title),
            "the form never took the foreground"
        );
        // SAFETY: COM calls inside the apartment `_com` holds; the elements drop before it.
        unsafe {
            let hwnd = GetForegroundWindow();
            let uia = automation().expect("automation");
            let focused = || {
                let e = uia.GetFocusedElement().expect("focused element");
                e.CurrentAutomationId().expect("automation id").to_string()
            };
            let target = WinTarget::default();
            let before = target.current().expect("a foreground window to return to");
            assert_eq!(focused(), "first");
            let root = uia.ElementFromHandle(hwnd).expect("form element");
            control_named(&uia, &root, "second")
                .SetFocus()
                .expect("SetFocus");
            assert_eq!(focused(), "second");
            let other = Form(
                std::process::Command::new("charmap.exe")
                    .spawn()
                    .expect("charmap"),
            );
            let moved = wait_for(|| GetForegroundWindow() != hwnd);
            let restored = target.restore(before);
            let after = focused();
            drop(other);
            drop(form);
            assert!(moved, "Character Map never took the foreground");
            restored.expect("restore");
            after
        }
    }

    #[test]
    #[ignore]
    fn a_field_focused_after_the_press_keeps_focus() {
        let extra = "New-Object System.Windows.Forms.TextBox";
        assert_eq!(focused_after_restore("keeps-focus", extra), "second");
    }

    #[test]
    #[ignore]
    fn focus_on_a_button_goes_back_to_the_press_time_field() {
        let extra = "New-Object System.Windows.Forms.Button";
        assert_eq!(focused_after_restore("button", extra), "first");
    }

    #[test]
    #[ignore]
    fn a_cloaked_window_is_refused_like_a_closed_one() {
        let title = unique_title("cloaked");
        let on_shown =
            "$v = 1; [Probe.Native]::DwmSetWindowAttribute($form.Handle, 13, [ref]$v, 4)";
        let form = open_form(&title, "New-Object System.Windows.Forms.Button", on_shown);
        let find = || {
            let wide: Vec<u16> = title.encode_utf16().chain([0]).collect();
            // SAFETY: `wide` is NUL-terminated and outlives the call.
            unsafe { FindWindowW(None, PCWSTR(wide.as_ptr())) }.unwrap_or_default()
        };
        let up = wait_for(|| !find().is_invalid() && cloaked(find()));
        let hwnd = find();
        let other = Form(
            std::process::Command::new("charmap.exe")
                .spawn()
                .expect("charmap"),
        );
        // SAFETY: accepts any handle value.
        let moved = wait_for(|| unsafe { GetForegroundWindow() } != hwnd);
        let result = WinTarget::default().restore(hwnd.0 as isize);
        drop(other);
        drop(form);
        assert!(up, "the form never cloaked itself");
        assert!(moved, "Character Map never took the foreground");
        let e = result.unwrap_err();
        assert!(e.contains("closed"), "{e}");
    }

    #[test]
    #[ignore]
    fn a_cloaked_window_in_front_is_refused_too() {
        let title = unique_title("cloaked-front");
        let on_shown =
            "$v = 1; [Probe.Native]::DwmSetWindowAttribute($form.Handle, 13, [ref]$v, 4)";
        let form = open_form(&title, "New-Object System.Windows.Forms.Button", on_shown);
        let wide: Vec<u16> = title.encode_utf16().chain([0]).collect();
        // SAFETY: `wide` is NUL-terminated and outlives the call.
        let find = || unsafe { FindWindowW(None, PCWSTR(wide.as_ptr())) }.unwrap_or_default();
        let up = wait_for(|| !find().is_invalid() && cloaked(find()));
        let hwnd = find();
        // SAFETY: takes no arguments.
        let in_front = unsafe { GetForegroundWindow() } == hwnd;
        let result = WinTarget::default().restore(hwnd.0 as isize);
        drop(form);
        assert!(up, "the form never cloaked itself");
        assert!(in_front, "the cloaked form lost the foreground");
        let e = result.unwrap_err();
        assert!(e.contains("closed"), "{e}");
    }

    #[test]
    #[ignore]
    fn a_caret_marks_a_text_field_ui_automation_cannot_see() {
        let title = unique_title("caret");
        let on_shown = "$second.Focus(); [void][Probe.Native]::CreateCaret($second.Handle, [IntPtr]::Zero, 2, 16)";
        let form = open_form(&title, "New-Object System.Windows.Forms.Button", on_shown);
        let up = wait_for(|| {
            foreground_title() == title && caret_shown() && focus_accepts_text() == Ok(false)
        });
        let takes = WinTarget::default().accepts_text();
        drop(form);
        assert!(up, "the Button never had focus and a caret");
        assert!(takes, "the caret did not make it a text field");
    }

    #[test]
    #[ignore]
    fn clipboard_only_sets_the_clipboard() {
        const PROBE: &str = "byovox clipboard probe שלום мир 😀";

        let mut cb = arboard::Clipboard::new().expect("clipboard");
        let previous = cb.get_text().ok();
        println!("previous clipboard text: {previous:?}");

        ClipboardOnlyInject.inject(PROBE).expect("inject");
        let got = cb.get_text().expect("read back");
        println!("read back: {got:?}");
        assert_eq!(got, PROBE);

        if let Some(p) = previous {
            cb.set_text(p).expect("restore");
            println!("restored: {:?}", cb.get_text().ok());
        }
    }
}
