//! Explicit narration-only Copy transaction. No cut, paste, select-all or text logging.
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{
    ERROR_SUCCESS, GetLastError, GlobalFree, HANDLE, HGLOBAL, HWND, LPARAM, SetLastError, WPARAM,
};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, EnumClipboardFormats, GetClipboardData, GetClipboardOwner,
    GetClipboardSequenceNumber, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{
    GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY,
    KEYEVENTF_KEYUP, SendInput, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, GA_ROOTOWNER, GUITHREADINFO, GWL_STYLE, GetAncestor,
    GetClassNameW, GetForegroundWindow, GetGUIThreadInfo, GetWindowLongW, GetWindowThreadProcessId,
    HWND_MESSAGE, SMTO_ABORTIFHUNG, SMTO_BLOCK, SendMessageTimeoutW, WINDOW_EX_STYLE, WINDOW_STYLE,
};
use windows::core::w;

use super::hotkey::INJECT_MARKER;

const MAX_SNAPSHOT: usize = 8 * 1024 * 1024;
const MAX_FORMATS: usize = 64;
const UNICODE_TEXT: u32 = 13;
const COPY_WAIT: Duration = Duration::from_millis(1200);
const CLIPBOARD_BUSY: &str = "Read aloud: clipboard busy; try again";
const UNSAFE_SNAPSHOT: &str =
    "Read aloud: cannot safely preserve this clipboard; copy fallback refused";

fn source_current(window: isize, current: &impl Fn() -> bool) -> Result<(), String> {
    // SAFETY: getter; handles are compared only, never dereferenced.
    if !current() || unsafe { GetForegroundWindow() }.0 as isize != window {
        return Err("Read aloud: cancelled or source window changed".into());
    }
    Ok(())
}

pub(super) fn wait_for_release(
    window: isize,
    trigger: u32,
    current: &impl Fn() -> bool,
) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        source_current(window, current)?;
        // Include either side of Ctrl/Alt/Shift/Win, the trigger, C and Insert. Never synthesize
        // releases for keys the user is holding, or turn Ctrl+C into Ctrl+Alt+C.
        let held = [0x11, 0x12, 0x10, 0x5b, 0x5c, 0x43, 0x2d, trigger]
            .into_iter()
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

pub(super) fn selection(
    window: isize,
    current: &(impl Fn() -> bool + Sync),
) -> Result<String, String> {
    // A single bounded selection worker waits for this transaction; it cannot accumulate
    // copy threads. Its persistent UIA/MTA thread never owns a clipboard window.
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .name("byovox-selection-copy".into())
            .spawn_scoped(scope, move || copy_on_worker(window, current))
            .map_err(|_| "Read aloud: could not start Copy selection")?
            .join()
            .map_err(|_| "Read aloud: Copy selection failed")?
    })
}

struct Clipboard;
impl Clipboard {
    fn open(owner: HWND) -> Result<Self, String> {
        // SAFETY: a live hidden owner window on this thread, or null for tests/read-only.
        unsafe { OpenClipboard(Some(owner)) }.map_err(|_| CLIPBOARD_BUSY)?;
        Ok(Self)
    }
}
impl Drop for Clipboard {
    fn drop(&mut self) {
        // SAFETY: balances the successful OpenClipboard on this thread.
        let _ = unsafe { CloseClipboard() };
    }
}

struct Owner(HWND);
impl Owner {
    fn new() -> Result<Self, String> {
        // SAFETY: a built-in STATIC message-only window, never shown or activated.
        unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                w!(""),
                WINDOW_STYLE(0),
                0,
                0,
                0,
                0,
                Some(HWND_MESSAGE),
                None,
                None,
                None,
            )
        }
        .map(Self)
        .map_err(|_| "Read aloud: clipboard owner unavailable".into())
    }
}
impl Drop for Owner {
    fn drop(&mut self) {
        // SAFETY: this thread created this private, never-visible window.
        let _ = unsafe { DestroyWindow(self.0) };
    }
}

struct SavedFormat {
    format: u32,
    memory: HGLOBAL,
}
impl Drop for SavedFormat {
    fn drop(&mut self) {
        if !self.memory.is_invalid() {
            // SAFETY: memory is ours until SetClipboardData succeeds and clears the field.
            let _ = unsafe { GlobalFree(Some(self.memory)) };
        }
    }
}

fn preservable(format: u32) -> bool {
    // Text, locale, DIB/DIBV5, file lists and registered HGLOBAL formats (HTML/RTF/PNG,
    // app metadata, etc.). Never guess ownership/copy semantics for GDI/private handles.
    matches!(format, 1 | 7 | 8 | 13 | 15 | 16 | 17) || format >= 0xc000
}

fn clipboard_bytes(format: u32, limit: usize) -> Result<Vec<u8>, String> {
    // SAFETY: caller holds OpenClipboard; only documented HGLOBAL formats enter here.
    unsafe {
        let handle = GetClipboardData(format).map_err(|_| UNSAFE_SNAPSHOT)?;
        let memory = HGLOBAL(handle.0);
        let size = GlobalSize(memory);
        if size == 0 || size > limit {
            return Err(UNSAFE_SNAPSHOT.into());
        }
        let pointer = GlobalLock(memory);
        if pointer.is_null() {
            return Err(UNSAFE_SNAPSHOT.into());
        }
        let bytes = std::slice::from_raw_parts(pointer.cast::<u8>(), size).to_vec();
        let _ = GlobalUnlock(memory);
        Ok(bytes)
    }
}

fn snapshot() -> Result<Vec<SavedFormat>, String> {
    let mut saved = Vec::new();
    let mut format = 0;
    let mut total = 0;
    loop {
        // SAFETY: caller holds OpenClipboard. LastError distinguishes end from failure.
        unsafe { SetLastError(ERROR_SUCCESS) };
        format = unsafe { EnumClipboardFormats(format) };
        if format == 0 {
            if unsafe { GetLastError() } != ERROR_SUCCESS {
                return Err(UNSAFE_SNAPSHOT.into());
            }
            break;
        }
        if saved.len() >= MAX_FORMATS || !preservable(format) {
            return Err(UNSAFE_SNAPSHOT.into());
        }
        let bytes = clipboard_bytes(format, MAX_SNAPSHOT - total)?;
        total += bytes.len();
        // Preallocate every restoration handle BEFORE Copy changes the clipboard.
        let memory =
            unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes.len()) }.map_err(|_| UNSAFE_SNAPSHOT)?;
        let item = SavedFormat { format, memory };
        let pointer = unsafe { GlobalLock(memory) };
        if pointer.is_null() {
            return Err(UNSAFE_SNAPSHOT.into());
        }
        // SAFETY: newly allocated memory has exactly bytes.len() writable bytes.
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), pointer.cast::<u8>(), bytes.len());
            let _ = GlobalUnlock(memory);
        }
        saved.push(item);
    }
    Ok(saved)
}

fn owner_pid() -> Option<u32> {
    // SAFETY: read-only metadata; never reads text or a window title.
    let owner = unsafe { GetClipboardOwner() }.ok()?;
    let mut pid = 0;
    unsafe { GetWindowThreadProcessId(owner, Some(&mut pid)) };
    (pid != 0).then_some(pid)
}

fn may_restore(copied: u32, now: u32, owner: Option<u32>, source: u32) -> bool {
    copied != 0 && copied == now && source != 0 && owner == Some(source)
}

fn same_copy(before: u32, after: u32, owner: Option<u32>, source: u32) -> bool {
    before != 0 && after != 0 && before != after && source != 0 && owner == Some(source)
}

fn decode_text(bytes: &[u8]) -> Result<String, String> {
    if !bytes.len().is_multiple_of(2) {
        return Err("Read aloud: invalid copied text".into());
    }
    let units: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .collect();
    let end = units
        .iter()
        .position(|u| *u == 0)
        .ok_or("Read aloud: invalid copied text")?;
    let text = String::from_utf16(&units[..end]).map_err(|_| "Read aloud: invalid copied text")?;
    if text.trim().is_empty() {
        return Err("Read aloud: select text first".into());
    }
    if text.chars().count() > crate::read_aloud::MAX_TEXT {
        return Err("Read aloud: selection exceeds 20,000 characters".into());
    }
    Ok(text)
}

fn class(window: HWND) -> String {
    let mut name = [0u16; 128];
    // SAFETY: wrapper bounds the output to this stack buffer.
    let count = unsafe { GetClassNameW(window, &mut name) };
    String::from_utf16_lossy(&name[..count.max(0) as usize])
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CopyChord {
    Desktop,
    WindowsTerminal,
}

fn source_owns_clipboard(
    chord: CopyChord,
    source_pid: u32,
    clipboard_pid: u32,
    clipboard_class: &str,
    source_window: isize,
    owner_root: isize,
) -> bool {
    source_pid != 0
        && clipboard_pid != 0
        && (clipboard_pid == source_pid
            || (chord == CopyChord::WindowsTerminal
                && clipboard_class == "PseudoConsoleWindow"
                && source_window != 0
                && owner_root == source_window))
}

fn copy_chord(root: &str, focused: &str) -> Option<CopyChord> {
    if root.eq_ignore_ascii_case("CASCADIA_HOSTING_WINDOW_CLASS")
        || focused.eq_ignore_ascii_case("CASCADIA_HOSTING_WINDOW_CLASS")
    {
        return Some(CopyChord::WindowsTerminal);
    }
    if ["ConsoleWindowClass", "mintty", "VirtualConsoleClass"]
        .iter()
        .any(|name| root.eq_ignore_ascii_case(name) || focused.eq_ignore_ascii_case(name))
    {
        None
    } else {
        Some(CopyChord::Desktop)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CopyTarget {
    focus: HWND,
    chord: CopyChord,
}

fn copy_target(window: HWND) -> Result<CopyTarget, String> {
    let mut info = GUITHREADINFO {
        cbSize: size_of::<GUITHREADINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: live out-parameter with cbSize; this only inspects foreground focus metadata.
    let thread = unsafe { GetWindowThreadProcessId(window, None) };
    unsafe { GetGUIThreadInfo(thread, &mut info) }
        .map_err(|_| "Read aloud: cannot verify Copy target")?;
    if info.hwndFocus.is_invalid() {
        return Err("Read aloud: cannot verify Copy target".into());
    }
    let root_class = class(window);
    let focused_class = class(info.hwndFocus);
    let chord = copy_chord(&root_class, &focused_class).ok_or(
        "Read aloud: Copy fallback is unavailable in this terminal; Windows Terminal is supported",
    )?;
    // UIA password checks precede this path; additionally guard native EDIT controls.
    if focused_class.eq_ignore_ascii_case("Edit")
        && unsafe { GetWindowLongW(info.hwndFocus, GWL_STYLE) } & 0x20 != 0
    {
        return Err("Read aloud: password fields are excluded".into());
    }
    if focused_class == "Scintilla" {
        // SCI_GETSELECTIONEMPTY: numeric query, no cross-process pointers/text. A bare
        // Ctrl+C in Scintilla can copy the current line, so require a real selection.
        let mut empty = 1usize;
        let result = unsafe {
            SendMessageTimeoutW(
                info.hwndFocus,
                2650,
                WPARAM(0),
                LPARAM(0),
                SMTO_ABORTIFHUNG | SMTO_BLOCK,
                200,
                Some(&mut empty),
            )
        };
        if result.0 == 0 || empty != 0 {
            return Err("Read aloud: select text first".into());
        }
    }
    Ok(CopyTarget {
        focus: info.hwndFocus,
        chord,
    })
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
    // SAFETY: one balanced Ctrl+C (desktop) or Ctrl+Insert (Windows Terminal) batch. No paste/cut/Enter events.
    let sent = unsafe { SendInput(&events, size_of::<INPUT>() as i32) };
    if sent != events.len() as u32 {
        if sent > 0 {
            // Release only keys this batch could have pressed, with our injection marker.
            unsafe { SendInput(&events[2..], size_of::<INPUT>() as i32) };
        }
        return Err("Read aloud: Copy could not reach this app".into());
    }
    Ok(())
}

fn restore(
    owner: HWND,
    saved: &mut [SavedFormat],
    copied: u32,
    source: u32,
    copied_owner: HWND,
) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_millis(500);
    let _clipboard = loop {
        if let Ok(open) = Clipboard::open(owner) {
            break open;
        }
        if Instant::now() >= deadline {
            return Err("Read aloud: clipboard busy; copied text remains on clipboard".into());
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    // Check while locked: never overwrite a newer clipboard update.
    if unsafe { GetClipboardOwner() }.ok() != Some(copied_owner)
        || !may_restore(
            copied,
            unsafe { GetClipboardSequenceNumber() },
            owner_pid(),
            source,
        )
    {
        return Err(
            "Read aloud: clipboard changed again; left the newer contents untouched".into(),
        );
    }
    unsafe { EmptyClipboard() }.map_err(|_| "Read aloud: could not restore clipboard")?;
    for item in saved {
        // SAFETY: each preallocated HGLOBAL transfers ownership only on success.
        unsafe { SetClipboardData(item.format, Some(HANDLE(item.memory.0))) }
            .map_err(|_| "Read aloud: clipboard restoration incomplete")?;
        item.memory = HGLOBAL::default();
    }
    Ok(())
}

fn copy_on_worker(window: isize, current: &impl Fn() -> bool) -> Result<String, String> {
    source_current(window, current)?;
    let target = HWND(window as *mut _);
    let focus = copy_target(target)?;
    let mut source = 0;
    unsafe { GetWindowThreadProcessId(target, Some(&mut source)) };
    if source == 0 {
        return Err("Read aloud: cannot verify Copy target".into());
    }
    let owner = Owner::new()?;
    let (mut saved, before) = {
        let _clipboard = Clipboard::open(owner.0)?;
        let saved = snapshot()?;
        (saved, unsafe { GetClipboardSequenceNumber() })
    };
    if before == 0 {
        return Err(CLIPBOARD_BUSY.into());
    }
    source_current(window, current)?;
    if copy_target(target)? != focus {
        return Err("Read aloud: selected control changed".into());
    }
    // Snapshot rendering may have taken time: do not send if the user has pressed a key.
    wait_for_release(window, 0x43, current)?;
    source_current(window, current)?;
    if copy_target(target)? != focus {
        return Err("Read aloud: selected control changed".into());
    }
    if unsafe { GetClipboardSequenceNumber() } != before {
        return Err("Read aloud: clipboard changed; try again".into());
    }
    let sent = send_copy(focus.chord);
    let deadline = Instant::now() + COPY_WAIT;
    loop {
        // Even after cancellation, observe the Copy result long enough to restore it.
        // No text can leave this function unless source/current checks still pass.
        if let Ok(_clipboard) = Clipboard::open(owner.0) {
            let after = unsafe { GetClipboardSequenceNumber() };
            if after != before {
                let clipboard_pid = owner_pid();
                let clipboard_owner = unsafe { GetClipboardOwner() }.ok();
                let clipboard_class = clipboard_owner.map(class).unwrap_or_default();
                // A console application can perform Copy through its hidden console
                // window. Accept this only for the Terminal route and an OS ownership
                // chain rooted at the exact original Terminal HWND, never by name/PID
                // ancestry alone. Another Terminal window remains unrelated.
                let owner_root = clipboard_owner
                    .map(|window| unsafe { GetAncestor(window, GA_ROOTOWNER) }.0 as isize)
                    .unwrap_or_default();
                let attributed = source_owns_clipboard(
                    focus.chord,
                    source,
                    clipboard_pid.unwrap_or_default(),
                    &clipboard_class,
                    window,
                    owner_root,
                );
                if !attributed
                    || !same_copy(
                        before,
                        after,
                        clipboard_pid,
                        clipboard_pid.unwrap_or_default(),
                    )
                {
                    // Attribution failures need enough metadata to distinguish a helper
                    // process from a competing clipboard writer. Never include clipboard
                    // contents, window titles, document names or executable paths.
                    return Err(format!(
                        "Read aloud: unrelated clipboard update; try again (route={:?}, source_pid={}, owner_pid={:?}, root_class={}, focus_class={}, owner_class={}, owner_root_matches={}, sequence={}->{})",
                        focus.chord,
                        source,
                        clipboard_pid,
                        class(target),
                        class(focus.focus),
                        clipboard_class,
                        owner_root == window,
                        before,
                        after
                    ));
                }
                let result = source_current(window, current)
                    .and_then(|()| sent.clone())
                    .and_then(|()| {
                        if copy_target(target)? != focus {
                            return Err("Read aloud: selected control changed".into());
                        }
                        decode_text(&clipboard_bytes(
                            UNICODE_TEXT,
                            (crate::read_aloud::MAX_TEXT * 2 + 1) * 2,
                        )?)
                    });
                // Delayed rendering can advance the sequence. Retain the final locked value.
                let copied = unsafe { GetClipboardSequenceNumber() };
                drop(_clipboard);
                restore(
                    owner.0,
                    &mut saved,
                    copied,
                    clipboard_pid.unwrap_or_default(),
                    clipboard_owner.unwrap_or_default(),
                )?;
                source_current(window, current)?;
                return result;
            }
        }
        if Instant::now() >= deadline {
            return Err("Read aloud: no copied selection; select text and try again".into());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn terminal_copy_accepts_only_console_owners_linked_to_original_window() {
        assert!(source_owns_clipboard(
            CopyChord::WindowsTerminal,
            10,
            20,
            "PseudoConsoleWindow",
            100,
            100
        ));
        for (chord, source, owner, class, window, root) in [
            (CopyChord::Desktop, 10, 20, "PseudoConsoleWindow", 100, 100),
            (CopyChord::WindowsTerminal, 10, 20, "OtherWindow", 100, 100),
            (
                CopyChord::WindowsTerminal,
                10,
                20,
                "PseudoConsoleWindow",
                100,
                200,
            ),
            (
                CopyChord::WindowsTerminal,
                10,
                20,
                "PseudoConsoleWindow",
                100,
                0,
            ),
            (
                CopyChord::WindowsTerminal,
                10,
                20,
                "PseudoConsoleWindow",
                0,
                0,
            ),
            (
                CopyChord::WindowsTerminal,
                10,
                0,
                "PseudoConsoleWindow",
                100,
                100,
            ),
            (
                CopyChord::WindowsTerminal,
                0,
                20,
                "PseudoConsoleWindow",
                100,
                100,
            ),
        ] {
            assert!(!source_owns_clipboard(
                chord, source, owner, class, window, root
            ));
        }
        assert!(source_owns_clipboard(
            CopyChord::Desktop,
            10,
            10,
            "Edit",
            100,
            200
        ));
    }
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

    #[test]
    fn clipboard_round_trip_in_private_station() {
        // A child owns a private, noninteractive window station/desktop. These tests
        // never read, replace or enumerate the user's interactive clipboard/windows.
        const CHILD: &str = "BYOVOX_PRIVATE_CLIPBOARD_TEST";
        if std::env::var_os(CHILD).is_none() {
            use std::os::windows::process::CommandExt;
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "platform::windows::selection_copy::tests::clipboard_round_trip_in_private_station", "--nocapture"])
                .env(CHILD, "1").creation_flags(0x08000000).status().unwrap();
            assert!(status.success(), "isolated clipboard test failed");
            return;
        }
        use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
        use windows::Win32::System::StationsAndDesktops::*;
        use windows::Win32::System::Threading::{GetCurrentProcessId, GetCurrentThreadId};
        use windows::core::PCWSTR;
        // SAFETY: child-process-only desktop assignment, before any window creation.
        // Never SwitchDesktop, never access the old station's clipboard.
        unsafe {
            let old_station = GetProcessWindowStation().unwrap();
            let old_desktop = GetThreadDesktop(GetCurrentThreadId()).unwrap();
            let station = CreateWindowStationW(PCWSTR::null(), 0, 0x10000000, None).unwrap();
            SetProcessWindowStation(station).unwrap();
            let desktop = CreateDesktopW(
                w!("byovox-copy-test"),
                PCWSTR::null(),
                None,
                DESKTOP_CONTROL_FLAGS(0),
                DESKTOP_CREATEWINDOW.0 | DESKTOP_READOBJECTS.0 | DESKTOP_WRITEOBJECTS.0,
                None,
            )
            .unwrap();
            SetThreadDesktop(desktop).unwrap();
            {
                let owner = Owner::new().unwrap();
                let source = GetCurrentProcessId();
                // Exercise Windows' real ownership chain in the private desktop.
                // None of these windows is shown; no interactive desktop is touched.
                let terminal = Owner(
                    CreateWindowExW(
                        WINDOW_EX_STYLE(0),
                        w!("STATIC"),
                        w!(""),
                        windows::Win32::UI::WindowsAndMessaging::WS_POPUP,
                        0,
                        0,
                        0,
                        0,
                        None,
                        None,
                        None,
                        None,
                    )
                    .unwrap(),
                );
                let console = Owner(
                    CreateWindowExW(
                        WINDOW_EX_STYLE(0),
                        w!("STATIC"),
                        w!(""),
                        windows::Win32::UI::WindowsAndMessaging::WS_POPUP,
                        0,
                        0,
                        0,
                        0,
                        Some(terminal.0),
                        None,
                        None,
                        None,
                    )
                    .unwrap(),
                );
                assert_eq!(GetAncestor(console.0, GA_ROOTOWNER), terminal.0);
                assert_ne!(GetAncestor(console.0, GA_ROOTOWNER), owner.0);
                let html = RegisterClipboardFormatW(w!("HTML Format"));
                let rtf = RegisterClipboardFormatW(w!("Rich Text Format"));
                assert_ne!(html, 0);
                assert_ne!(rtf, 0);
                let originals = [
                    (UNICODE_TEXT, bytes("original synthetic text")),
                    (html, b"synthetic HTML\0".to_vec()),
                    (rtf, b"synthetic RTF\0".to_vec()),
                ];
                let mut saved = {
                    let _lock = Clipboard::open(owner.0).unwrap();
                    EmptyClipboard().unwrap();
                    for (format, data) in &originals {
                        set_fixture(*format, data);
                    }
                    snapshot().unwrap()
                };
                let copied = {
                    let _lock = Clipboard::open(owner.0).unwrap();
                    EmptyClipboard().unwrap();
                    set_fixture(UNICODE_TEXT, &bytes("copied fixture"));
                    drop(_lock); // source Copy publishes synthesized formats on close
                    GetClipboardSequenceNumber()
                };
                // Matching PID/sequence is not enough if the expected owner HWND differs.
                assert!(restore(owner.0, &mut saved, copied, source, console.0).is_err());
                restore(owner.0, &mut saved, copied, source, owner.0).unwrap_or_else(|e| {
                    panic!(
                        "{e}; copied={copied} now={} owner={:?} expected={source}",
                        GetClipboardSequenceNumber(),
                        owner_pid()
                    )
                });
                {
                    let _lock = Clipboard::open(owner.0).unwrap();
                    for (format, expected) in &originals {
                        assert_eq!(&clipboard_bytes(*format, MAX_SNAPSHOT).unwrap(), expected);
                    }
                }
                // A valid fresh Copy can outlive its owner window. This reproduces
                // the documented Windows limitation behind the live ownerless refusal;
                // it does not change admission policy or inspect the user's clipboard.
                let before_ownerless = GetClipboardSequenceNumber();
                {
                    let temporary_owner = Owner::new().unwrap();
                    let _lock = Clipboard::open(temporary_owner.0).unwrap();
                    EmptyClipboard().unwrap();
                    set_fixture(UNICODE_TEXT, &bytes("fresh ownerless fixture"));
                    drop(_lock);
                    drop(temporary_owner);
                }
                {
                    let _lock = Clipboard::open(owner.0).unwrap();
                    assert!(GetClipboardOwner().is_err());
                    assert_ne!(GetClipboardSequenceNumber(), before_ownerless);
                    assert_eq!(
                        decode_text(&clipboard_bytes(UNICODE_TEXT, 1000).unwrap()).unwrap(),
                        "fresh ownerless fixture"
                    );
                }
                // A newer source-owned write must not be overwritten even with the same PID.
                let mut saved = {
                    let _lock = Clipboard::open(owner.0).unwrap();
                    snapshot().unwrap()
                };
                let copied = {
                    let _lock = Clipboard::open(owner.0).unwrap();
                    EmptyClipboard().unwrap();
                    set_fixture(UNICODE_TEXT, &bytes("first copy"));
                    drop(_lock); // source Copy publishes synthesized formats on close
                    GetClipboardSequenceNumber()
                };
                {
                    let _lock = Clipboard::open(owner.0).unwrap();
                    EmptyClipboard().unwrap();
                    set_fixture(UNICODE_TEXT, &bytes("newer copy"));
                }
                assert!(restore(owner.0, &mut saved, copied, source, owner.0).is_err());
                {
                    let _lock = Clipboard::open(owner.0).unwrap();
                    assert_eq!(
                        decode_text(&clipboard_bytes(UNICODE_TEXT, 1000).unwrap()).unwrap(),
                        "newer copy"
                    );
                    EmptyClipboard().unwrap();
                }
                // Empty clipboard snapshots are preserved as empty.
                let mut saved = {
                    let _lock = Clipboard::open(owner.0).unwrap();
                    snapshot().unwrap()
                };
                assert!(saved.is_empty());
                let copied = {
                    let _lock = Clipboard::open(owner.0).unwrap();
                    set_fixture(UNICODE_TEXT, &bytes("temporary copy"));
                    drop(_lock); // source Copy publishes synthesized formats on close
                    GetClipboardSequenceNumber()
                };
                restore(owner.0, &mut saved, copied, source, owner.0).unwrap();
                let _lock = Clipboard::open(owner.0).unwrap();
                assert_eq!(EnumClipboardFormats(0), 0);
            }
            SetProcessWindowStation(old_station).unwrap();
            SetThreadDesktop(old_desktop).unwrap();
            CloseDesktop(desktop).unwrap();
            CloseWindowStation(station).unwrap();
        }
    }

    unsafe fn set_fixture(format: u32, bytes: &[u8]) {
        // SAFETY: test holds its private clipboard; memory is transferred to Windows.
        unsafe {
            let memory = GlobalAlloc(GMEM_MOVEABLE, bytes.len()).unwrap();
            let pointer = GlobalLock(memory);
            assert!(!pointer.is_null());
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), pointer.cast::<u8>(), bytes.len());
            let _ = GlobalUnlock(memory);
            SetClipboardData(format, Some(HANDLE(memory.0))).unwrap();
        }
    }

    #[test]
    fn stale_missing_owner_and_foreign_copies_are_never_accepted() {
        assert!(!same_copy(7, 7, Some(10), 10));
        assert!(!same_copy(0, 8, Some(10), 10));
        assert!(!same_copy(7, 0, Some(10), 10));
        assert!(!same_copy(7, 8, None, 10));
        assert!(!same_copy(7, 8, Some(11), 10));
        assert!(same_copy(u32::MAX, 1, Some(10), 10));
    }
    #[test]
    fn restoration_never_clobbers_a_newer_or_foreign_clipboard() {
        assert!(may_restore(8, 8, Some(10), 10));
        assert!(!may_restore(8, 9, Some(10), 10));
        assert!(!may_restore(8, 8, Some(11), 10));
        assert!(!may_restore(8, 8, None, 10));
        assert!(!may_restore(0, 0, Some(10), 10));
    }

    #[test]
    fn unsupported_clipboard_handle_formats_are_refused() {
        for format in [2, 3, 9, 14, 0x80, 0x200, 0x300] {
            assert!(!preservable(format));
        }
        for format in [1, 7, 8, 13, 15, 16, 17, 0xc000] {
            assert!(preservable(format));
        }
    }
    fn bytes(text: &str) -> Vec<u8> {
        text.encode_utf16()
            .chain([0])
            .flat_map(u16::to_le_bytes)
            .collect()
    }
    #[test]
    fn copied_text_is_bounded_valid_utf16_and_not_empty() {
        assert_eq!(
            decode_text(&bytes("Hello שלום 😀")).unwrap(),
            "Hello שלום 😀"
        );
        assert!(decode_text(&bytes("  ")).is_err());
        assert!(decode_text(&[1]).is_err());
        assert!(decode_text(&[1, 0]).is_err());
        assert!(decode_text(&[0, 0xd8, 0, 0]).is_err());
        assert!(decode_text(&bytes(&"x".repeat(crate::read_aloud::MAX_TEXT + 1))).is_err());
    }
}
