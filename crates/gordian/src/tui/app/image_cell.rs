//! Render-preview links attached to tool entries in the transcript.

/// One render-preview link attached to the tool row that produced it.
pub struct ImageCell {
    /// Index of the producing tool entry in [`App::transcript`](super::App).
    pub tool_entry: usize,
    /// The PNG path on disk (the render tool's `png_path`).
    pub path: String,
}

impl ImageCell {
    /// A fresh cell attached to `tool_entry`.
    pub fn new(tool_entry: usize, path: impl Into<String>) -> Self {
        Self {
            tool_entry,
            path: path.into(),
        }
    }
}
