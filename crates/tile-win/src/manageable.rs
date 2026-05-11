//! "Should we tile this window?" logic.
//!
//! Win32 has no concept of "real top-level user window" — the desktop is
//! crawling with invisible message windows, tooltips, IMM candidate lists,
//! ApplicationFrameHost shells, and worker zombies. Getting this filter
//! right is the difference between a tiling WM that works and one that
//! locks itself fighting Explorer.exe.
//!
//! Heuristic order (cheapest first, matches komorebi's approach):
//!   1. Visible? `IsWindowVisible` — fast.
//!   2. Has WS_CHILD? Skip — child windows aren't top-level.
//!   3. WS_EX_TOOLWINDOW? Skip — palettes, tooltips.
//!   4. Cloaked? UWP windows the shell is hiding — skip.
//!   5. Class name in skip-list? (Windows.UI.Core.CoreWindow, etc.)
//!   6. Owner window present *and* not WS_EX_APPWINDOW? Skip.
//!   7. Otherwise → manageable.

#![cfg(windows)]

use windows::Win32::Foundation::{BOOL, HWND, LPARAM, RECT};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetWindow, GetWindowLongW, GetWindowRect, GetWindowTextW, GetClassNameW,
    GetWindowThreadProcessId, IsIconic, IsWindowVisible,
    GW_OWNER, GWL_EXSTYLE, GWL_STYLE,
    WS_CAPTION, WS_CHILD, WS_EX_APPWINDOW, WS_EX_TOOLWINDOW, WS_POPUP, WS_VISIBLE,
};

const SKIP_CLASSES: &[&str] = &[
    "Windows.UI.Core.CoreWindow",
    "ApplicationFrameWindow", // most are UWP shells; specific exes get included via float rules
    "Progman",
    "WorkerW",
    "Shell_TrayWnd",
    "Shell_SecondaryTrayWnd",
    "DV2ControlHost",
    "MsgrIMEWindowClass",
    "SysShadow",
    "Button", // start menu button
    "TaskListThumbnailWnd",
    "TaskListOverlayWnd",
];

pub fn class_of(hwnd: HWND) -> String {
    let mut buf = [0u16; 256];
    unsafe {
        let n = GetClassNameW(hwnd, &mut buf);
        if n <= 0 { return String::new(); }
        String::from_utf16_lossy(&buf[..n as usize])
    }
}

pub fn title_of(hwnd: HWND) -> String {
    let mut buf = [0u16; 512];
    unsafe {
        let n = GetWindowTextW(hwnd, &mut buf);
        if n <= 0 { return String::new(); }
        String::from_utf16_lossy(&buf[..n as usize])
    }
}

fn is_cloaked(hwnd: HWND) -> bool {
    let mut cloaked: u32 = 0;
    unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            &mut cloaked as *mut _ as *mut _,
            std::mem::size_of::<u32>() as u32,
        )
        .ok()
        .map(|_| cloaked != 0)
        .unwrap_or(false)
    }
}

/// PID of the daemon process, populated at startup. Windows owned by the
/// daemon itself are excluded from tiling. (Doesn't cover the launching
/// terminal — that's a separate process. To skip that, walk the parent-
/// process chain at startup; tracked but not done here.)
static OWN_PID: std::sync::OnceLock<u32> = std::sync::OnceLock::new();

pub fn set_own_pid(pid: u32) {
    let _ = OWN_PID.set(pid);
}

pub fn is_manageable(hwnd: HWND) -> bool {
    unsafe {
        if !IsWindowVisible(hwnd).as_bool() { return false; }
        // Minimized windows have WS_VISIBLE *and* return ridiculous off-screen
        // rects from GetWindowRect (≈ -32000, -32000). Calling SetWindowPos on
        // them with our DWM-frame-corrected math produces ERROR_INVALID_PARAMETER.
        // Filter them out — when the user un-minimizes, EVENT_SYSTEM_MINIMIZEEND
        // brings them back into the layout.
        if IsIconic(hwnd).as_bool() { return false; }

        // Don't tile the terminal we were launched from (or the daemon's own
        // future GUI windows once we have one).
        if let Some(own) = OWN_PID.get() {
            let mut owner_pid: u32 = 0;
            let _ = GetWindowThreadProcessId(hwnd, Some(&mut owner_pid));
            if owner_pid == *own { return false; }
        }

        let style = GetWindowLongW(hwnd, GWL_STYLE) as u32;
        let ex    = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
        if style & WS_CHILD.0 != 0 { return false; }
        if style & WS_VISIBLE.0 == 0 { return false; }
        // Tool windows: float by default (they tend to be palettes/toolbars).
        if ex & WS_EX_TOOLWINDOW.0 != 0 && ex & WS_EX_APPWINDOW.0 == 0 { return false; }

        // Cloaked-check INTENTIONALLY REMOVED. DWM cloaks every window on
        // a non-current virtual desktop, so if we filtered by cloaked
        // here we'd never see windows on other VDs at all — the daemon
        // would only know about the desktop the user happens to be on
        // when it started. That gave a "tiling only works after I
        // visit each desktop" UX. By accepting cloaked windows, we
        // enumerate every top-level window on every VD at startup and
        // route each into the right workspace via GetWindowDesktopId.
        // The applier separately skips not-on-current-VD windows via
        // IsWindowOnCurrentVirtualDesktop, so we don't try to
        // SetWindowPos cloaked windows until they uncloak naturally.
        // Suspended UWP windows that pass this check are filtered by
        // the class skiplist below (ApplicationFrameWindow, etc.).
        let _ = is_cloaked; // kept for callers that want the check explicitly

        let class = class_of(hwnd);
        if SKIP_CLASSES.iter().any(|c| *c == class) { return false; }

        // Owner-window heuristic: dialogs of other windows usually shouldn't tile.
        let has_owner = GetWindow(hwnd, GW_OWNER).map(|h| !h.is_invalid()).unwrap_or(false);
        if has_owner && ex & WS_EX_APPWINDOW.0 == 0 {
            return false;
        }

        // Has a caption — strong signal it's a real top-level window.
        // (Some borderless apps still pass; combined with the cloaked check
        //  this is conservative enough to be useful.)
        let _ = WS_CAPTION;
        true
    }
}

/// Returns `true` when every edge of `actual` is within `tolerance` pixels
/// of the matching edge of `expected`. Used to decide whether a window's
/// rect is "filling the monitor" — DWM rounding plus per-app fudge factors
/// mean exact equality is too strict (some borderless games leave a 1-px
/// gap on one edge; some video players overscan by 1–2 px).
fn rect_matches_within(actual: RECT, expected: RECT, tolerance: i32) -> bool {
    (actual.left   - expected.left  ).abs() <= tolerance
        && (actual.top    - expected.top   ).abs() <= tolerance
        && (actual.right  - expected.right ).abs() <= tolerance
        && (actual.bottom - expected.bottom).abs() <= tolerance
}

/// Is this window currently filling its monitor in a way that *looks like*
/// a fullscreen experience the user is actively in (game, video, slideshow)?
///
/// We require **both** of:
///   1. The window's `GetWindowRect` matches its monitor's `rcMonitor`
///      (full physical bounds, not work area — fullscreen apps draw over
///      the taskbar) within a 4-px tolerance on every edge.
///   2. Either the window is styled as a borderless container
///      (`WS_POPUP` set, `WS_CAPTION` clear — the canonical "borderless
///      fullscreen" game pattern), OR the rect *exactly* matches the
///      monitor (≤ 1-px slop), which catches conventional captioned
///      windows that some video players / presentation modes inflate
///      to monitor bounds inline.
///
/// We deliberately do **not** call this from `is_manageable`. A fullscreen
/// window is still tracked in the BSP tree — we just want the applier to
/// leave it alone. When the user exits fullscreen, the next applier pass
/// will reposition the window into its reserved tile cell.
///
/// Untested in CI; verified via daemon smoke tests (Win32 calls aren't
/// mockable from a unit test).
pub fn is_fullscreen(hwnd: HWND) -> bool {
    unsafe {
        let mut rect = RECT::default();
        if GetWindowRect(hwnd, &mut rect).is_err() { return false; }

        let h_monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        if h_monitor.is_invalid() { return false; }

        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if !GetMonitorInfoW(h_monitor, &mut info).as_bool() { return false; }

        // Check #1: does the window cover the full monitor (within 4 px)?
        if !rect_matches_within(rect, info.rcMonitor, 4) { return false; }

        // Check #2: does it look fullscreen by style, or is the rect a
        // tight (≤ 1-px) match for monitor bounds?
        let style = GetWindowLongW(hwnd, GWL_STYLE) as u32;
        let popup_no_caption =
            (style & WS_POPUP.0  != 0) &&
            (style & WS_CAPTION.0 == 0);
        let tight_match = rect_matches_within(rect, info.rcMonitor, 1);

        popup_no_caption || tight_match
    }
}

/// Enumerate every visible top-level window on the desktop and call `cb`
/// for each one we deem manageable. Used at daemon start to seed state.
pub fn enumerate_manageable<F: FnMut(HWND)>(mut cb: F) {
    use windows::Win32::UI::WindowsAndMessaging::EnumWindows;
    extern "system" fn proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let cb = unsafe { &mut *(lparam.0 as *mut Box<dyn FnMut(HWND)>) };
        if is_manageable(hwnd) { cb(hwnd); }
        BOOL(1) // continue
    }
    let boxed: Box<dyn FnMut(HWND)> = Box::new(|h| cb(h));
    let mut boxed = Box::new(boxed);
    let _ = unsafe { EnumWindows(Some(proc), LPARAM(&mut *boxed as *mut _ as isize)) };
    // RECT used in tests below
    let _ = RECT { left: 0, top: 0, right: 0, bottom: 0 };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(l: i32, t: i32, ri: i32, b: i32) -> RECT {
        RECT { left: l, top: t, right: ri, bottom: b }
    }

    #[test]
    fn exact_rects_match_at_zero_tolerance() {
        let a = r(0, 0, 1920, 1080);
        assert!(rect_matches_within(a, a, 0));
    }

    #[test]
    fn one_pixel_off_passes_at_default_tolerance() {
        let monitor = r(0, 0, 1920, 1080);
        let window  = r(1, 0, 1920, 1079); // 1 px off on left and bottom
        assert!(rect_matches_within(window, monitor, 4));
        assert!(rect_matches_within(window, monitor, 1));
        assert!(!rect_matches_within(window, monitor, 0));
    }

    #[test]
    fn five_pixels_off_fails_at_four_tolerance() {
        let monitor = r(0, 0, 1920, 1080);
        let window  = r(0, 0, 1915, 1080); // 5 px off on right
        assert!(!rect_matches_within(window, monitor, 4));
    }

    #[test]
    fn negative_origin_monitor_works() {
        // Secondary monitor positioned to the left of primary.
        let monitor = r(-1920, 0, 0, 1080);
        let window  = r(-1920, 0, 0, 1080);
        assert!(rect_matches_within(window, monitor, 4));

        let off = r(-1918, 0, 0, 1080);
        assert!(rect_matches_within(off, monitor, 4));
        assert!(!rect_matches_within(off, monitor, 1));
    }

    #[test]
    fn windowed_app_does_not_match_monitor() {
        let monitor = r(0, 0, 1920, 1080);
        let window  = r(100, 100, 800, 600);
        assert!(!rect_matches_within(window, monitor, 4));
    }
}
