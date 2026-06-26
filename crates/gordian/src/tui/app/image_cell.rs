//! Inline render previews in the transcript.
//!
//! The text transcript stays a flat [`Vec<Entry>`](super::Entry) so the state
//! machine and its tests are unchanged. Image previews — a rendered board or
//! schematic PNG a tool produced — ride alongside in a parallel [`ImageCell`]
//! list, each pinned to a transcript position so the renderer can interleave them
//! in scroll order.
//!
//! [`ImageCell`] keeps the protocol state OUT of [`Entry`] on purpose:
//! `ratatui_image`'s `StatefulProtocol` is `!Clone`/`!PartialEq`, which would
//! poison the `PartialEq`-deriving `Entry`. The state is built lazily on the
//! first draw (once the [`ratatui_image::picker::Picker`] and the cell width are
//! both known) and cached here across frames.

use ratatui_image::protocol::StatefulProtocol;

/// One inline image preview, pinned just after a transcript entry.
///
/// `after` is the transcript length at the moment the image was posted, so the
/// renderer draws it once entries `0..after` have been laid out (and drops it
/// when an unwind truncates the transcript past that point). The heavy
/// `StatefulProtocol` is `None` until the first draw decodes the PNG.
pub struct ImageCell {
    /// The transcript length when this image was posted — its sort key.
    pub after: usize,
    /// The PNG path on disk (the render tool's `png_path`).
    pub path: String,
    /// A one-line caption (e.g. the tool name or `/preview`).
    pub caption: String,
    /// The decoded image protocol, built lazily on first draw and cached.
    /// `Failed` once decoding errored, so the renderer falls back to a text
    /// label without retrying every frame.
    pub state: ImageState,
}

/// The lazy decode lifecycle of an [`ImageCell`]'s protocol.
#[derive(Default)]
pub enum ImageState {
    /// Not yet decoded (the default until the first draw).
    #[default]
    Pending,
    /// Decoded; ready to render.
    Ready {
        proto: Box<StatefulProtocol>,
        cols: u16,
        font_size: (u16, u16),
    },
    /// The file was unreadable or failed to decode — show the text label.
    Failed,
}

impl ImageCell {
    /// A fresh, undecoded cell pinned after transcript index `after`.
    pub fn new(after: usize, path: impl Into<String>, caption: impl Into<String>) -> Self {
        Self {
            after,
            path: path.into(),
            caption: caption.into(),
            state: ImageState::Pending,
        }
    }

    /// Human label for the type of preview this render produced.
    pub fn preview_label(&self) -> &'static str {
        match self.caption.as_str() {
            "render_board" => "board preview",
            "render_schematic" => "schematic preview",
            _ => "render preview",
        }
    }

    /// The stable text-label fallback used under the screenshot harness, while a
    /// decode is pending, and after a decode failure — so a preview never crashes
    /// the UI and the SVG snapshots stay byte-stable.
    pub fn label(&self) -> String {
        format!(
            "▸ {} · {} · {}",
            self.preview_label(),
            self.caption,
            self.path
        )
    }
}
