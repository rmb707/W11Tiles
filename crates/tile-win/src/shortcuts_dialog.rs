//! Keyboard-shortcuts dialog (Win32 `MessageBoxW`).
//!
//! Triggered from the tray menu's "Keyboard Shortcuts" entry. We pop a
//! native MessageBox listing every default binding plus mouse gestures.
//! Cheap path to discoverability — users who don't know the chords can
//! reach for the tray. A proper styled dialog (custom window, sortable
//! list) is a follow-up; MessageBox gets the info into their hands
//! without us shipping a UI framework.
//!
//! ## Why a separate thread
//!
//! `MessageBoxW` blocks the calling thread until the user dismisses the
//! dialog. The daemon's tokio runtime would freeze if we called it
//! inline. The caller (`tile_daemon`) spawns a regular OS thread, that
//! thread calls into here, and the daemon's main loop keeps pumping.

#![cfg(windows)]

use windows::core::PCWSTR;
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{
    MessageBoxW, MB_ICONINFORMATION, MB_OK, MB_SETFOREGROUND,
};

/// Hardcoded shortcut listing — matches the defaults in
/// `tile_core::config::default_keybinds`. We hand-format with section
/// headers because the default-keybinds table is dense (29 entries)
/// and reads better grouped. If the user customizes their config, the
/// listing won't reflect that; reading the live config is a follow-up.
const TEXT: &str = "\
WIN+ALT is the prefix for every keybind below.\n\
\n\
NAVIGATION\n\
  Win+Alt+H/J/K/L                Focus left / down / up / right\n\
  Win+Alt+Shift+H/J/K/L          Swap focused window with neighbor\n\
  Win+Alt+Ctrl+H/J/K/L           Resize current split (5%)\n\
\n\
WORKSPACES (virtual desktops)\n\
  Win+Alt+1 … 9                  Switch to workspace 1 – 9\n\
  Win+Alt+Shift+1 … 9            Move focused window to workspace\n\
\n\
TAB GROUPS\n\
  Win+Alt+Tab                    Cycle to next tab in current group\n\
  Win+Alt+Shift+Tab              Cycle to previous tab\n\
  Win+Alt+U                      Untab focused window\n\
\n\
OTHER\n\
  Win+Alt+Space                  Toggle floating on focused window\n\
  Win+Alt+Q                      Quit W11 Tiles\n\
\n\
MOUSE\n\
  Drag onto tile center          Merge as tab group\n\
  Drag onto tile edge (T/B/L/R)  Split tile in that direction\n\
  Drag onto blank space          Snap back to current tile\n\
  Click a tab on the strip       Switch active tab\n\
  Click the X on a tab           Close that window\n\
";

/// Block on a Win32 `MessageBoxW` listing every shortcut. Returns when
/// the user dismisses the dialog. Safe to call from any OS thread; the
/// caller is responsible for ensuring this isn't blocking a runtime
/// (the daemon spawns a dedicated `std::thread::spawn` for it).
pub fn show() {
    // Encode title + body to UTF-16 + NUL.
    let title: Vec<u16> = "W11 Tiles — Keyboard Shortcuts\0".encode_utf16().collect();
    let body:  Vec<u16> = format!("{TEXT}\0").encode_utf16().collect();
    unsafe {
        let _ = MessageBoxW(
            HWND::default(),
            PCWSTR(body.as_ptr()),
            PCWSTR(title.as_ptr()),
            MB_OK | MB_ICONINFORMATION | MB_SETFOREGROUND,
        );
    }
}
