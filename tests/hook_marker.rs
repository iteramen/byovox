#![cfg(windows)]
//! The hook skips events carrying `INJECT_MARKER`: `SendInput`'s `dwExtraInfo` must reach `KBDLLHOOKSTRUCT`.
//! Ignored in CI: global hooks never unhook and it injects real keystrokes.

use std::sync::mpsc::{RecvTimeoutError, channel};
use std::thread::sleep;
use std::time::Duration;

use byovox::hotkey::{Hotkey, HotkeyEvent};
use byovox::platform::windows::hotkey::{HookHotkey, INJECT_MARKER};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, SendInput,
    VIRTUAL_KEY,
};

const RCONTROL: u16 = 0xA3;

fn tap_right_ctrl(extra: usize) {
    for flags in [
        KEYEVENTF_EXTENDEDKEY,
        KEYEVENTF_EXTENDEDKEY | KEYEVENTF_KEYUP,
    ] {
        let input = INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(RCONTROL),
                    wScan: 0,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: extra,
                },
            },
        };
        // SAFETY: one well-formed INPUT, size declared as the API requires.
        let sent = unsafe { SendInput(&[input], size_of::<INPUT>() as i32) };
        assert_eq!(sent, 1, "SendInput refused Right Ctrl");
    }
}

#[test]
#[ignore]
fn events_stamped_with_the_inject_marker_are_invisible_to_the_hotkey() {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        Box::new(HookHotkey::new("ControlRight", "Escape").unwrap()).run(tx)
    });
    // `INSTALLED` is private to the backend; the unmarked tap below is the liveness check.
    sleep(Duration::from_millis(700));

    tap_right_ctrl(0);
    let mut got = Vec::new();
    while let Ok(ev) = rx.recv_timeout(Duration::from_millis(700)) {
        got.push(ev);
    }
    println!("unmarked Right Ctrl -> {got:?}");
    assert_eq!(
        got,
        vec![HotkeyEvent::Pressed, HotkeyEvent::Released],
        "the hook was not live, so this test proves nothing"
    );

    tap_right_ctrl(INJECT_MARKER);
    match rx.recv_timeout(Duration::from_millis(700)) {
        Err(RecvTimeoutError::Timeout) => println!("marked Right Ctrl -> no event"),
        Err(RecvTimeoutError::Disconnected) => panic!("the hook thread exited"),
        Ok(ev) => panic!("a marked event reached the hotkey as {ev:?}"),
    }
}
