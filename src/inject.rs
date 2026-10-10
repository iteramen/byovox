//! Text insertion rungs. The pipeline tries them in order; the last rung is `none`.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InjectMode {
    Auto,
    Type,
    Paste,
    ClipboardOnly,
}

impl InjectMode {
    pub fn parse(s: &str) -> Option<InjectMode> {
        match s {
            "auto" => Some(InjectMode::Auto),
            "type" => Some(InjectMode::Type),
            "paste" => Some(InjectMode::Paste),
            "clipboard-only" => Some(InjectMode::ClipboardOnly),
            _ => None,
        }
    }
}

pub trait Inject: Send {
    fn name(&self) -> &'static str;
    fn inject(&mut self, text: &str) -> Result<(), String>;
}

/// The window a dictation started in; insertion goes back to it even if focus moved.
pub trait Target: Send {
    /// Opaque window handle; `None` when nothing is focused (on Windows the desktop and taskbar count as nothing).
    fn current(&self) -> Option<isize>;
    /// Err when the window was closed or could not be raised; the caller types nothing.
    fn restore(&self, window: isize) -> Result<(), String>;
    fn accepts_text(&self) -> bool;
}
