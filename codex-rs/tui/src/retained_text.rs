//! Keep oversized transcript text on disk while preserving it for expansion and copying.

use std::borrow::Cow;
use std::io::Write;
use std::sync::Arc;
use tempfile::NamedTempFile;

const INLINE_TEXT_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug)]
pub(crate) struct RetainedText(Arc<Storage>);

#[derive(Debug)]
enum Storage {
    Inline(String),
    File {
        file: NamedTempFile,
        preview: String,
    },
}

impl RetainedText {
    pub(crate) fn new(text: String) -> Self {
        if text.len() > INLINE_TEXT_BYTES {
            match NamedTempFile::new().and_then(|mut file| {
                file.write_all(text.as_bytes())?;
                Ok(file)
            }) {
                Ok(file) => {
                    let head = text.floor_char_boundary(INLINE_TEXT_BYTES / 2);
                    let tail =
                        text.ceil_char_boundary(text.len().saturating_sub(INLINE_TEXT_BYTES / 2));
                    let preview = format!(
                        "{}\n[… output retained on disk; expand to read the full result …]\n{}",
                        &text[..head],
                        &text[tail..]
                    );
                    return Self(Arc::new(Storage::File { file, preview }));
                }
                Err(error) => {
                    tracing::warn!(%error, bytes = text.len(), "could not spill transcript text")
                }
            }
        }
        Self(Arc::new(Storage::Inline(text)))
    }

    /// Compact presentation never reads an arbitrarily large spill back into the heap.
    pub(crate) fn preview(&self) -> &str {
        match self.0.as_ref() {
            Storage::Inline(text) => text,
            Storage::File { preview, .. } => preview,
        }
    }

    pub(crate) fn read(&self) -> Cow<'_, str> {
        match self.0.as_ref() {
            Storage::Inline(text) => Cow::Borrowed(text),
            Storage::File { file, .. } => match std::fs::read_to_string(file.path()) {
                Ok(text) => Cow::Owned(text),
                Err(error) => {
                    tracing::warn!(%error, "could not read retained transcript text");
                    Cow::Borrowed(
                        "[Retained output unavailable; reopen the saved session to reload it.]",
                    )
                }
            },
        }
    }
}

#[cfg(test)]
#[path = "retained_text_tests.rs"]
mod tests;
