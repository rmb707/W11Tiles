//! Per-window DWM tuning for a clean, native-feeling experience.
//!
//! Windows' default behavior wraps every `SetWindowPos` call in a built-in
//! "shove" animation: the window scales/translates with a stock spring
//! curve before settling in place. That's fine when the user is the one
//! moving windows, but when *we* are repositioning them every layout
//! change, it stacks on top of our own animator — the result is a visible
//! double-bounce that feels gummy.
//!
//! Fix: tell DWM to leave each managed window alone. We set:
//!   * `DWMWA_TRANSITIONS_FORCEDISABLED = TRUE` — kills minimize/restore/
//!     move animations on this specific window. Our animator becomes the
//!     only thing producing motion.
//!   * `DWMWA_WINDOW_CORNER_PREFERENCE = ROUND` (Win11 only) — opts into
//!     Win11's standard rounded corners even on apps that don't request
//!     them. Visual coherence across the layout.
//!
//! Both calls are idempotent and cheap (~microseconds). Failing calls are
//! ignored — pre-Win11 systems just don't get the corner preference, and
//! certain apps refuse the transition-disable; neither is fatal.

#![cfg(windows)]

use windows::Win32::Foundation::{BOOL, HWND};
use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWINDOWATTRIBUTE};

/// DWM attribute IDs not exposed by windows-rs 0.58 as named constants
/// for our crate's enabled features. They're stable Win32 values:
/// see `<dwmapi.h>` in the Windows SDK.
const DWMWA_TRANSITIONS_FORCEDISABLED:    DWMWINDOWATTRIBUTE = DWMWINDOWATTRIBUTE(3);
const DWMWA_USE_IMMERSIVE_DARK_MODE:      DWMWINDOWATTRIBUTE = DWMWINDOWATTRIBUTE(20);
const DWMWA_WINDOW_CORNER_PREFERENCE:     DWMWINDOWATTRIBUTE = DWMWINDOWATTRIBUTE(33);
const DWMWA_SYSTEMBACKDROP_TYPE:          DWMWINDOWATTRIBUTE = DWMWINDOWATTRIBUTE(38);

/// `DWM_WINDOW_CORNER_PREFERENCE::DWMWCP_ROUND` — Win11's standard
/// rounded corner radius. `2` per the SDK header.
const DWMWCP_ROUND: u32 = 2;

/// `DWM_SYSTEMBACKDROP_TYPE::DWMSBT_TRANSIENTWINDOW` = 3 (Acrylic). Picks
/// up the desktop background through a frosted blur. The other choice
/// here is `DWMSBT_MAINWINDOW` (Mica = 2), which is more solid and
/// adapts to the wallpaper's average color. Transient/Acrylic is the
/// better visual for short-lived overlays (drop zones during drag) and
/// translucent chrome (tab strips on top of tile content).
const DWMSBT_TRANSIENTWINDOW: u32 = 3;

/// Apply our preferred DWM settings to a managed window. Safe to call
/// every layout pass; both attribute writes are cheap and idempotent.
pub fn prepare_window(hwnd: HWND) {
    unsafe {
        let disabled: BOOL = BOOL(1);
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_TRANSITIONS_FORCEDISABLED,
            &disabled as *const _ as *const _,
            std::mem::size_of::<BOOL>() as u32,
        );
        let round: u32 = DWMWCP_ROUND;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &round as *const _ as *const _,
            std::mem::size_of::<u32>() as u32,
        );
    }
}

/// Apply the "overlay chrome" look to one of our own windows
/// (tab-strip overlay, drop-zone overlay): rounded corners,
/// Acrylic system backdrop, dark-mode title bar. Picks up the
/// desktop wallpaper colors through a frosted blur — visual
/// coherence with Win11's own chrome. Silently no-ops on
/// pre-Win11-22H2.
pub fn polish_overlay(hwnd: HWND) {
    unsafe {
        let dark: BOOL = BOOL(1);
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            &dark as *const _ as *const _,
            std::mem::size_of::<BOOL>() as u32,
        );
        let round: u32 = DWMWCP_ROUND;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &round as *const _ as *const _,
            std::mem::size_of::<u32>() as u32,
        );
        let backdrop: u32 = DWMSBT_TRANSIENTWINDOW;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_SYSTEMBACKDROP_TYPE,
            &backdrop as *const _ as *const _,
            std::mem::size_of::<u32>() as u32,
        );
    }
}
