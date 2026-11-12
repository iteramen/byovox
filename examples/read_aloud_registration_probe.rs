//! Native hotkey lifecycle only: no input injection, selection, microphone, clipboard or network.
#[cfg(all(windows, debug_assertions))]
fn main() {
    use byovox::hotkey::{Hotkey, HotkeyEvent};
    use byovox::platform::windows::hotkey::{HookHotkey, read_hook_diagnostics, set_armed};
    use std::time::{Duration, Instant};
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, RegisterHotKey, UnregisterHotKey,
    };
    fn wait(test: impl Fn() -> bool) {
        let end = Instant::now() + Duration::from_secs(3);
        while !test() {
            assert!(Instant::now() < end, "registration lifecycle timeout");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    const ID: i32 = 0x0B72;
    // Claim the fixture's configured chord first to prove a visible conflict event.
    unsafe {
        RegisterHotKey(None, ID, MOD_ALT | MOD_CONTROL | MOD_NOREPEAT, 0x52)
            .expect("fixture chord free");
    }
    let (tx, rx) = std::sync::mpsc::channel();
    let hook = HookHotkey::new("F23", "Escape")
        .unwrap()
        .with_read_aloud("ControlLeft+AltLeft+R", "Escape")
        .unwrap();
    std::thread::spawn(move || Box::new(hook).run(tx));
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(3)).unwrap(),
        HotkeyEvent::ReadAloudUnavailable
    );
    unsafe {
        UnregisterHotKey(None, ID).unwrap();
    }
    set_armed(true);
    wait(|| read_hook_diagnostics().1);
    set_armed(false);
    wait(|| !read_hook_diagnostics().1);
    unsafe {
        RegisterHotKey(None, ID, MOD_ALT | MOD_CONTROL | MOD_NOREPEAT, 0x52)
            .expect("Disable releases the actual OS shortcut");
        UnregisterHotKey(None, ID).unwrap();
    }
    set_armed(true);
    wait(|| read_hook_diagnostics().1);
    set_armed(false);
    wait(|| !read_hook_diagnostics().1);
    assert!(rx.try_recv().is_err(), "no dictation or unexpected action");
    println!(
        "PASS: native shortcut conflict reported; Disable releases OS registration; Enable reacquires; no dictation/input/network."
    );
}
#[cfg(not(all(windows, debug_assertions)))]
fn main() {
    println!("Run this registration probe in a Windows debug build.");
}
