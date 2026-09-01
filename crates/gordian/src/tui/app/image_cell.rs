//! Render-preview links in the transcript.
//!
//! The text transcript stays a flat [`Vec<Entry>`](super::Entry) so the state
//! machine and its tests are unchanged. Previews — a rendered board or
//! schematic PNG a tool produced — ride alongside in a parallel [`ImageCell`]
//! list, each pinned to a transcript position so the renderer can interleave
//! them in scroll order. A preview renders as a one-row link; the renderer
//! records its screen rect in [`App::preview_zones`](super::App) and a click
//! there opens the PNG in the system viewer (no inline terminal graphics).

/// One render-preview link, pinned just after a transcript entry.
///
/// `after` is the transcript length at the moment the preview was posted, so
/// the renderer draws it once entries `0..after` have been laid out (and drops
/// it when an unwind truncates the transcript past that point).
pub struct ImageCell {
    /// The transcript length when this preview was posted — its sort key.
    pub after: usize,
    /// The PNG path on disk (the render tool's `png_path`).
    pub path: String,
    /// A one-line caption (e.g. the tool name or `/preview`).
    pub caption: String,
}

impl ImageCell {
    /// A fresh cell pinned after transcript index `after`.
    pub fn new(after: usize, path: impl Into<String>, caption: impl Into<String>) -> Self {
        Self {
            after,
            path: path.into(),
            caption: caption.into(),
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

    /// The link row's text.
    pub fn label(&self) -> String {
        format!(
            "▸ {} · {} · {}",
            self.preview_label(),
            self.caption,
            self.path
        )
    }
}
