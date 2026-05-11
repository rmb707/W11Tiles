use serde::{Deserialize, Serialize};

/// Stable identifier for a managed window. We don't expose the raw HWND
/// out of `tile-win` — HWNDs are recyclable and crossing the daemon
/// boundary with a transparent integer would invite use-after-free bugs.
/// `tile-win` maintains the HWND ↔ WindowId mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct WindowId(pub u64);

impl std::fmt::Display for WindowId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "win#{}", self.0)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowInfo {
    pub id: WindowId,
    pub title: String,
    /// Win32 window class — used for floating-rules matching.
    pub class: String,
    /// Process exe basename (e.g. "chrome.exe"). Optional — comes from a
    /// best-effort `QueryFullProcessImageNameW` lookup.
    pub exe: Option<String>,
    /// True if user/rules have asked for this window to float instead of tile.
    pub floating: bool,
}

impl WindowInfo {
    pub fn new(id: WindowId, title: impl Into<String>, class: impl Into<String>) -> Self {
        Self {
            id,
            title: title.into(),
            class: class.into(),
            exe: None,
            floating: false,
        }
    }
}
