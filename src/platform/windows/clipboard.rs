//! What a borrowed clipboard gets back: its text, else its image, else its file list.
use std::path::PathBuf;

use arboard::{Clipboard, ImageData};

#[derive(Debug)]
pub(crate) enum Saved {
    Text(String),
    Image(ImageData<'static>),
    Files(Vec<PathBuf>),
}

impl Saved {
    /// None when the clipboard is empty or holds none of the three kinds.
    pub(crate) fn of(clipboard: &mut Clipboard) -> Option<Saved> {
        if let Ok(text) = clipboard.get_text() {
            return Some(Saved::Text(text));
        }
        if let Ok(image) = clipboard.get_image() {
            return Some(Saved::Image(image));
        }
        clipboard.get().file_list().ok().map(Saved::Files)
    }

    pub(crate) fn into_text(self) -> Option<String> {
        match self {
            Saved::Text(text) => Some(text),
            _ => None,
        }
    }

    pub(crate) fn restore(self, clipboard: &mut Clipboard) {
        let restored = match self {
            Saved::Text(text) => clipboard.set_text(text),
            Saved::Image(image) => clipboard.set_image(image),
            // arboard adds the file list beside what the clipboard holds without emptying it.
            Saved::Files(files) => clipboard
                .clear()
                .and_then(|()| clipboard.set().file_list(&files)),
        };
        if let Err(e) = restored {
            tracing::warn!(error = %e, "could not restore the previous clipboard");
        }
    }
}
