//! Read-aloud's selection: the app's own Copy, then the previous clipboard text back.
use std::time::{Duration, Instant};

use windows::Win32::Foundation::HWND;
use windows::Win32::System::DataExchange::GetClipboardSequenceNumber;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY,
    KEYEVENTF_KEYUP, SendInput, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GUITHREADINFO, GetClassNameW, GetForegroundWindow, GetGUIThreadInfo, GetWindowThreadProcessId,
};

use super::hotkey::INJECT_MARKER;

/// How long the app gets to put its Copy on the clipboard; no change means nothing was selected.
const COPY_WAIT: Duration = Duration::from_millis(1000);

/// Copies the selection in `window` and puts the clipboard's previous text back.
pub(crate) fn copy_selection(
    window: isize,
    trigger: u32,
    current: &impl Fn() -> bool,
) -> Result<String, String> {
    wait_for_release(trigger, current)?;
    // SAFETY: getter; the handle is compared and used for class lookups only.
    let foreground = unsafe { GetForegroundWindow() };
    if foreground.0 as isize != window {
        return Err("Read aloud: the window changed before Copy; try again".into());
    }
    let chord = copy_chord(&class(foreground), &class(focused(foreground)))
        .ok_or("Read aloud: this terminal is not supported; Windows Terminal is")?;
    let mut clipboard =
        arboard::Clipboard::new().map_err(|_| "Read aloud: clipboard unavailable; try again")?;
    let previous = clipboard.get_text().ok();
    // SAFETY: plain getter.
    let before = unsafe { GetClipboardSequenceNumber() };
    send_copy(chord)?;
    let deadline = Instant::now() + COPY_WAIT;
    // SAFETY: plain getter.
    while unsafe { GetClipboardSequenceNumber() } == before {
        if !current() {
            return Err("Read aloud: cancelled".into());
        }
        if Instant::now() >= deadline {
            return unchanged_clipboard(chord, previous);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let copied = clipboard.get_text();
    if let Some(previous) = previous
        && let Err(e) = clipboard.set_text(previous)
    {
        tracing::warn!(error = %e, "could not restore the previous clipboard text");
    }
    match copied {
        Ok(text) if !text.trim().is_empty() => Ok(text),
        _ => Err("Read aloud: select text first".into()),
    }
}

/// Nothing new was copied. A program in Windows Terminal that copies on select (herdr) has
/// already put its selection on the clipboard, and Terminal forwards Ctrl+Insert to it unused.
fn unchanged_clipboard(chord: CopyChord, current: Option<String>) -> Result<String, String> {
    match current {
        Some(text) if chord == CopyChord::WindowsTerminal && !text.trim().is_empty() => Ok(text),
        _ => Err("Read aloud: select text first".into()),
    }
}

/// A Copy sent while the shortcut's modifiers are down reaches the app as Ctrl+Alt+C.
fn wait_for_release(trigger: u32, current: &impl Fn() -> bool) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if !current() {
            return Err("Read aloud: cancelled".into());
        }
        // Either side of Ctrl/Alt/Shift/Win, and the trigger.
        let held = [0x11, 0x12, 0x10, 0x5b, 0x5c, trigger]
            .into_iter()
            // SAFETY: plain getter.
            .any(|key| unsafe { GetAsyncKeyState(key as i32) } as u16 & 0x8000 != 0);
        if !held {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err("Read aloud: release the shortcut keys and try again".into());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn focused(window: HWND) -> HWND {
    let mut info = GUITHREADINFO {
        cbSize: size_of::<GUITHREADINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: live out-parameter with cbSize set; reads focus metadata only.
    let thread = unsafe { GetWindowThreadProcessId(window, None) };
    match unsafe { GetGUIThreadInfo(thread, &mut info) } {
        Ok(()) if !info.hwndFocus.is_invalid() => info.hwndFocus,
        _ => window,
    }
}

fn class(window: HWND) -> String {
    let mut name = [0u16; 128];
    // SAFETY: the wrapper bounds the output to this stack buffer.
    let count = unsafe { GetClassNameW(window, &mut name) };
    String::from_utf16_lossy(&name[..count.max(0) as usize])
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CopyChord {
    Desktop,
    WindowsTerminal,
}

/// Ctrl+C in a terminal interrupts the running program. Windows Terminal copies on Ctrl+Insert;
/// other consoles have no Copy chord that is safe to send.
fn copy_chord(root: &str, focused: &str) -> Option<CopyChord> {
    let either = |name: &str| root.eq_ignore_ascii_case(name) || focused.eq_ignore_ascii_case(name);
    if either("CASCADIA_HOSTING_WINDOW_CLASS") {
        Some(CopyChord::WindowsTerminal)
    } else if ["ConsoleWindowClass", "mintty", "VirtualConsoleClass"]
        .into_iter()
        .any(either)
    {
        None
    } else {
        Some(CopyChord::Desktop)
    }
}

/// Whether the foreground window, or the control focused in it, is a terminal: there
/// Shift+Enter can submit a line as Enter does.
pub(super) fn terminal_in_front() -> bool {
    // SAFETY: getter; the handle is used for class lookups only.
    let window = unsafe { GetForegroundWindow() };
    is_terminal(&class(window), &class(focused(window)))
}

fn is_terminal(root: &str, focused: &str) -> bool {
    copy_chord(root, focused) != Some(CopyChord::Desktop)
}

fn copy_inputs(chord: CopyChord) -> Vec<INPUT> {
    let key = match chord {
        CopyChord::Desktop => 0x43,
        CopyChord::WindowsTerminal => 0x2d,
    };
    [(0x11, false), (key, false), (key, true), (0x11, true)]
        .into_iter()
        .map(|(vk, up)| INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(vk),
                    dwFlags: (if up {
                        KEYEVENTF_KEYUP
                    } else {
                        Default::default()
                    }) | (if vk == 0x2d {
                        KEYEVENTF_EXTENDEDKEY
                    } else {
                        Default::default()
                    }),
                    dwExtraInfo: INJECT_MARKER,
                    ..Default::default()
                },
            },
        })
        .collect()
}

fn send_copy(chord: CopyChord) -> Result<(), String> {
    let events = copy_inputs(chord);
    // SAFETY: one balanced Ctrl+C (desktop) or Ctrl+Insert (Windows Terminal) batch.
    let sent = unsafe { SendInput(&events, size_of::<INPUT>() as i32) };
    if sent != events.len() as u32 {
        if sent > 0 {
            // Release only the keys this batch could have pressed, with our marker.
            unsafe { SendInput(&events[2..], size_of::<INPUT>() as i32) };
        }
        return Err("Read aloud: Copy could not reach this app".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_terminal_uses_copy_insert_and_other_terminals_stay_refused() {
        assert_eq!(
            copy_chord(
                "CASCADIA_HOSTING_WINDOW_CLASS",
                "Windows.UI.Input.InputSite.WindowClass"
            ),
            Some(CopyChord::WindowsTerminal)
        );
        assert_eq!(
            copy_chord("other", "cascadia_hosting_window_class"),
            Some(CopyChord::WindowsTerminal)
        );
        for name in ["ConsoleWindowClass", "mintty", "VirtualConsoleClass"] {
            assert_eq!(copy_chord(name, "child"), None);
            assert_eq!(copy_chord("root", name), None);
        }
        assert_eq!(
            copy_chord("Notepad++", "Scintilla"),
            Some(CopyChord::Desktop)
        );
    }

    #[test]
    fn every_recognised_terminal_gets_no_line_break_and_a_desktop_app_does() {
        for name in [
            "CASCADIA_HOSTING_WINDOW_CLASS",
            "ConsoleWindowClass",
            "mintty",
            "VirtualConsoleClass",
        ] {
            assert!(is_terminal(name, "child"), "{name}");
            assert!(is_terminal("root", name), "{name}");
        }
        assert!(!is_terminal("Notepad", "RichEditD2DPT"));
        assert!(!is_terminal(
            "Chrome_WidgetWin_1",
            "Chrome_RenderWidgetHostHWND"
        ));
    }

    #[test]
    fn only_windows_terminal_reads_a_clipboard_its_copy_left_unchanged() {
        let text = || Some("selected in herdr".to_string());
        assert_eq!(
            unchanged_clipboard(CopyChord::WindowsTerminal, text()).as_deref(),
            Ok("selected in herdr")
        );
        assert!(unchanged_clipboard(CopyChord::Desktop, text()).is_err());
        assert!(unchanged_clipboard(CopyChord::WindowsTerminal, None).is_err());
        assert!(
            unchanged_clipboard(
                CopyChord::WindowsTerminal,
                Some(
                    " 
"
                    .into()
                )
            )
            .is_err()
        );
    }

    #[test]
    fn terminal_copy_never_sends_c_enter_or_paste_keys() {
        let inputs = copy_inputs(CopyChord::WindowsTerminal);
        let keys: Vec<_> = inputs
            .iter()
            .map(|input| {
                // SAFETY: copy_inputs writes the keyboard arm for every event.
                let key = unsafe { input.Anonymous.ki };
                assert_eq!(key.dwExtraInfo, INJECT_MARKER);
                (key.wVk.0, key.dwFlags)
            })
            .collect();
        assert_eq!(
            keys,
            vec![
                (0x11, Default::default()),
                (0x2d, KEYEVENTF_EXTENDEDKEY),
                (0x2d, KEYEVENTF_EXTENDEDKEY | KEYEVENTF_KEYUP),
                (0x11, KEYEVENTF_KEYUP)
            ]
        );
    }
}
