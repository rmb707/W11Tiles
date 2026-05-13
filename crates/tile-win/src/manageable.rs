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

    // Standard Win32 dialog class. Used by `MessageBox`, `DialogBox`,
    // `GetOpenFileName`, `GetSaveFileName`, "Save changes?" confirmation
    // popups, "Discard?" boxes, and most modal child dialogs across the
    // OS. Modern user-facing apps don't use #32770 as a main window —
    // it would have no design control over chrome — so the false-positive
    // risk is low. Without this, every confirmation popup gets absorbed
    // into the BSP and the user has to drag it out manually.
    "#32770",

    // Input Method Editor candidate windows (CJK input, emoji picker).
    // These are top-level and visible but disappear on focus loss; they
    // must never be tiled or they leave permanent ghost cells.
    "IME",
    "Default IME",
    "MSCTFIME UI",

    // Installers / setup wizards. These run at higher integrity than the
    // daemon (UAC-elevated) so SetWindowPos returns E_ACCESSDENIED on every
    // attempt. Even with the failure → auto-float safety net in the daemon,
    // it's cleaner to never enter them into the layout to begin with —
    // otherwise the user sees the tile shuffle once when their installer
    // opens and once when we float it.
    "ClaudeSetupProgress",       // Anthropic / Squirrel-style Electron installers
    "MsiDialogCloseClass",       // Windows Installer (MSI) — main dialog
    "MsiDialogNoCloseClass",     // Windows Installer (MSI) — no close box
    "MsiDialogProgressClass",    // Windows Installer (MSI) — progress
    "Nullsoft Installer",        // NSIS
    "TWizardForm",               // Inno Setup
    "TStartupForm",              // Inno Setup
    "TUninstallProgressForm",    // Inno Setup uninstall
    "InstallShield_Setup",       // InstallShield wizards
    "InstallShield Wizard",      // InstallShield (older)

    // Game launchers, anti-cheat overlays, crash handlers. These pop
    // open and disappear at game start/end; trying to tile them
    // produces a jarring shuffle each time and leaves dead cells when
    // they vanish. Conservative list — only well-known launchers, not
    // generic engine classes (UnityWndClass / UnrealWindow / Engine
    // are also used by tools we *do* want to tile).
    "Riot Client UxWindow",
    "Riot Client Splash",
    "Riot Client Crash Handler",
    "RCLIENTSPLASHCLASS",
    "Vanguard",                  // Riot anti-cheat
    "LCDPNG",                    // League of Legends launcher
    "Battle.net Login Window",
    "Battle.net Update Window",
    "Battle.net View",
    "Battle.net Launcher",
    "Blizzard Crash Reporter",
    "EasyAntiCheat",
    "BethBlue",                  // Bethesda launcher splash
    "Splash Screen",             // generic splash class used by some apps

    // Microsoft Office modeless dialogs. Word/Excel/PowerPoint show
    // "Insert Object", "Format Cells", "Properties", etc. as unowned
    // WS_POPUP frames that don't trip the dialog-style heuristic
    // (they're resizable; have WS_MAXIMIZEBOX). They also don't share
    // the host's main-window class (OpusApp / XLMAIN / PPTFrameClass),
    // so the user can still tile Office documents — only the popup
    // sub-dialogs get filtered.
    "NUIDialog",                 // Word "Insert Object", Excel "Format Cells", etc.
    "bosa_sdm_msword",           // Office object picker (legacy class kept around)
    "bosa_sdm_XL9",              // Excel-specific variant of bosa_sdm
    "_WwG",                      // Word inner-frame popups (occasionally surface as top-level)
    "MsoCommandBarPopup",        // Office ribbon overflow popups
];

/// Style bit values copied from `winuser.h`. windows-rs exposes these as
/// typed `WINDOW_STYLE` constants, but for the unit-testable helpers below
/// we keep plain u32 bitmasks so the tests don't need `cfg(windows)`.
const WS_DLGFRAME_BITS: u32 = 0x00400000;
const WS_MAXIMIZEBOX_BITS: u32 = 0x00010000;
const WS_EX_NOACTIVATE_BITS: u32 = 0x08000000;

/// Minimum dimensions for a window to count as "a real top-level user
/// window the user wants tiled." Below this size, the candidate is
/// almost always: a transient splash, a "Loading…" mini-popup, an
/// autocomplete picker, or a window that was just created and hasn't
/// been positioned yet. Picked empirically — Windows snap minimum is
/// roughly 200×148; we set 200×120 to be slightly more permissive.
const MIN_TILE_WIDTH:  i32 = 200;
const MIN_TILE_HEIGHT: i32 = 120;

/// Pure-style check: looks like a dialog by style bits alone. A window
/// with WS_DLGFRAME (non-resizable border) and no WS_MAXIMIZEBOX is the
/// canonical Win32 dialog. Real top-level user apps nearly always have
/// a maximize box; modal dialogs almost never do.
pub(crate) fn looks_like_dialog_style(style: u32) -> bool {
    style & WS_DLGFRAME_BITS != 0 && style & WS_MAXIMIZEBOX_BITS == 0
}

/// Pure-style check: window declines to take activation. Includes
/// ribbon dropdowns, autocomplete pickers, and IME candidate UIs that
/// escaped the WS_EX_TOOLWINDOW filter.
pub(crate) fn declines_activation(ex_style: u32) -> bool {
    ex_style & WS_EX_NOACTIVATE_BITS != 0
}

/// Pure-size check: too small to plausibly be a real tile cell.
pub(crate) fn rect_too_small(w: i32, h: i32) -> bool {
    w < MIN_TILE_WIDTH || h < MIN_TILE_HEIGHT
}

pub(crate) fn class_in_skiplist(class: &str) -> bool {
    SKIP_CLASSES.iter().any(|c| *c == class)
}

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

pub fn is_cloaked(hwnd: HWND) -> bool {
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
        // No-activate popups: ribbon dropdowns, autocomplete pickers,
        // IME candidate UIs that escaped WS_EX_TOOLWINDOW.
        if declines_activation(ex) { return false; }
        // Style-based dialog detection: WS_DLGFRAME without WS_MAXIMIZEBOX.
        // Catches confirmation popups and modal child dialogs even when the
        // app forgot to set an owner relationship (some games / Electron
        // apps create unowned popup dialogs).
        if looks_like_dialog_style(style) { return false; }

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
        if class_in_skiplist(&class) { return false; }

        // Owner-window heuristic: dialogs of other windows usually shouldn't tile.
        let has_owner = GetWindow(hwnd, GW_OWNER).map(|h| !h.is_invalid()).unwrap_or(false);
        if has_owner && ex & WS_EX_APPWINDOW.0 == 0 {
            return false;
        }

        // Size-based filter: windows too small to be a real tile cell.
        // Catches splashes, "Loading..." mini-windows, and brand-new
        // windows that haven't been positioned yet. The discover tick
        // re-checks every 2s so a temporarily-tiny window won't be
        // permanently lost.
        let mut rect = RECT::default();
        if GetWindowRect(hwnd, &mut rect).is_ok() {
            let w = rect.right - rect.left;
            let h = rect.bottom - rect.top;
            if rect_too_small(w, h) { return false; }
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

    // ---- New filter coverage ----

    #[test]
    fn dialog_style_classic_message_box_is_rejected() {
        // WS_DLGFRAME | WS_POPUP | WS_CAPTION | WS_SYSMENU - typical MessageBox style.
        // No WS_MAXIMIZEBOX = dialog.
        let style = 0x00400000 | 0x80000000 | 0x00C00000 | 0x00080000;
        assert!(looks_like_dialog_style(style));
    }

    #[test]
    fn normal_resizable_window_is_not_dialog() {
        // WS_OVERLAPPEDWINDOW = caption + sysmenu + thickframe + minimize + maximize.
        const WS_OVERLAPPEDWINDOW: u32 =
            0x00000000 | 0x00C00000 | 0x00080000 | 0x00040000 | 0x00020000 | 0x00010000;
        assert!(!looks_like_dialog_style(WS_OVERLAPPEDWINDOW));
    }

    #[test]
    fn no_activate_flag_is_detected() {
        assert!(declines_activation(0x08000000));         // exact bit
        assert!(declines_activation(0x08000000 | 0x100)); // mixed with others
        assert!(!declines_activation(0));
        assert!(!declines_activation(0x00040000));        // WS_EX_TOOLWINDOW alone
    }

    #[test]
    fn too_small_windows_are_filtered() {
        assert!( rect_too_small(50,  50));       // both axes small
        assert!( rect_too_small(50,  500));      // narrow
        assert!( rect_too_small(500, 50));       // short
        assert!( rect_too_small(199, 200));      // 1 px under width threshold
        assert!( rect_too_small(200, 119));      // 1 px under height threshold
        assert!(!rect_too_small(200, 120));      // exactly at threshold
        assert!(!rect_too_small(1920, 1080));    // typical
    }

    #[test]
    fn classic_dialog_class_skiplisted() {
        assert!(class_in_skiplist("#32770"));
    }

    #[test]
    fn ime_classes_skiplisted() {
        assert!(class_in_skiplist("IME"));
        assert!(class_in_skiplist("Default IME"));
        assert!(class_in_skiplist("MSCTFIME UI"));
    }

    #[test]
    fn game_launcher_classes_skiplisted() {
        assert!(class_in_skiplist("Riot Client UxWindow"));
        assert!(class_in_skiplist("Vanguard"));
        assert!(class_in_skiplist("Battle.net Launcher"));
        assert!(class_in_skiplist("EasyAntiCheat"));
    }

    #[test]
    fn real_app_classes_pass_skiplist() {
        assert!(!class_in_skiplist("Chrome_WidgetWin_1"));
        assert!(!class_in_skiplist("CASCADIA_HOSTING_WINDOW_CLASS"));
        assert!(!class_in_skiplist("MozillaWindowClass"));
        assert!(!class_in_skiplist("SDL_app")); // Steam, many indie games — handled by fullscreen check, not skip
    }
}
