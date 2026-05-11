//! Translates a `LayoutPlan` into `SetWindowPos` calls.
//!
//! Two non-obvious bits:
//!  - We use `BeginDeferWindowPos` / `EndDeferWindowPos` so all window
//!    moves commit in one frame. Without this, applying a plan to 5+
//!    windows produces visible "shuffling" as each window moves in turn.
//!  - DWM extends the visible window frame outside the actual window
//!    bounds (rounded-corner shadow on Win11). We pull the extended frame
//!    bounds via `DwmGetWindowAttribute(DWMWA_EXTENDED_FRAME_BOUNDS)` and
//!    inset our placement by the difference, so visual edges align with
//!    the gap math, not the invisible padding.

#![cfg(windows)]

use std::sync::Arc;

use tile_core::layout::LayoutPlan;
use tile_core::WindowId;
use tracing::{debug, warn};

use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS};
use windows::Win32::UI::WindowsAndMessaging::{
    GetWindowRect, IsIconic, SetWindowPos,
    HWND_TOP, SWP_ASYNCWINDOWPOS, SWP_NOACTIVATE,
};

use crate::hwnd_map::HwndMap;
use crate::manageable::{class_of, title_of};
use crate::vdesktop::is_on_current_desktop;

pub struct Applier {
    map: Arc<HwndMap>,
}

impl Applier {
    pub fn new(map: Arc<HwndMap>) -> Self { Self { map } }

    /// Apply a plan. Returns the list of `WindowId`s for which the OS
    /// rejected our placement — the daemon should auto-float those so they
    /// don't keep poisoning every subsequent repaint.
    pub fn apply(&self, plan: &LayoutPlan) -> Vec<WindowId> {
        let mut failed = Vec::new();
        if plan.placements.is_empty() { return failed; }
        // Min cell size we'll bother applying. Below this the DWM extended-
        // frame correction tends to push w/h negative, and SetWindowPos
        // returns ERROR_INVALID_PARAMETER. A 20-pixel cell wouldn't be
        // usable anyway — the user has to close a window or change workspace.
        const MIN_DIM: i32 = 64;

        // We use plain SetWindowPos rather than the Begin/Defer/End trio.
        // DeferWindowPos has stricter requirements (the HDWP carries some
        // implicit state about the input thread / DPI context) and rejects
        // certain real-app windows — Windows Terminal, Chromium-based
        // browsers, anything that talks to its own per-window DPI in a way
        // that confuses the deferred batch. komorebi switched to plain
        // SetWindowPos for the same reason. The cost is a tiny visible
        // shuffle when retiling 5+ windows; the win is 100% of real apps
        // accept it.
        unsafe {
            for p in &plan.placements {
                let Some(hwnd) = self.map.lookup_hwnd(p.window) else { continue };
                if IsIconic(hwnd).as_bool() { continue; }
                // A window in fullscreen (game, video player, slideshow) is
                // already covering its monitor by the user's explicit choice.
                // Re-issuing SetWindowPos against it either yanks it out of
                // exclusive fullscreen (D3D apps lose their swap-chain) or
                // visibly flickers the borderless variant. Leave it alone;
                // its tile slot is reserved in the BSP tree, so the next
                // applier pass after it exits fullscreen will reposition it.
                if crate::manageable::is_fullscreen(hwnd) {
                    debug!(window=%p.window, "skipping: window is fullscreen — leaving it alone until it exits");
                    continue;
                }
                // Don't fight the OS while it's animating a virtual-desktop
                // switch. Repositioning a window that just left the current
                // desktop interrupts the transition and feels like the
                // built-in `Ctrl+Win+Arrow` shortcut "broke."
                if !is_on_current_desktop(hwnd) {
                    debug!(window=%p.window, "skipping: not on current virtual desktop");
                    continue;
                }
                let (dx, dy, dw, dh) = invisible_frame_offset(hwnd);
                let x = p.rect.x      - dx;
                let y = p.rect.y      - dy;
                let w = p.rect.width  + dw;
                let h = p.rect.height + dh;
                if w < MIN_DIM || h < MIN_DIM {
                    debug!(window=%p.window, w, h, "skipping degenerate placement");
                    continue;
                }
                // Disable the OS's built-in window-move/resize animation
                // on this window, every pass. Otherwise Windows wraps our
                // SetWindowPos in its own "shove" animation that stacks
                // on top of our animator's tween — gummy double-bounce.
                // Idempotent and microseconds; cheap to call every frame.
                crate::dwm_polish::prepare_window(hwnd);
                // No SWP_NOZORDER: we *want* the call to also raise the
                // window in Z-order. Plan-order then becomes Z-order, so
                // tab groups (where multiple tabs share the same rect)
                // get the active tab on top — the layout emits the active
                // tab last, so its `SetWindowPos` runs last and wins Z.
                if let Err(e) = SetWindowPos(
                    hwnd, HWND_TOP, x, y, w, h,
                    SWP_NOACTIVATE | SWP_ASYNCWINDOWPOS,
                ) {
                    let title = title_of(hwnd);
                    let class = class_of(hwnd);
                    warn!(
                        window=%p.window, title=%title, class=%class,
                        x, y, w, h,
                        "SetWindowPos rejected this window — auto-floating: {e}"
                    );
                    failed.push(p.window);
                }
            }
        }
        failed
    }

    pub fn focus(&self, id: WindowId) {
        let Some(hwnd) = self.map.lookup_hwnd(id) else { return };
        unsafe {
            use windows::Win32::UI::WindowsAndMessaging::{
                BringWindowToTop, GetForegroundWindow, GetWindowThreadProcessId, SetForegroundWindow,
            };
            use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};

            // SetForegroundWindow has anti-focus-stealing rules. When the
            // calling thread isn't the current foreground thread (which is
            // our case — daemon is a separate process), Windows refuses
            // the call and instead *flashes the taskbar button* to alert
            // the user. The user sees a "loading" pulse on the taskbar
            // and the focus change feels broken.
            //
            // The standard workaround is to attach our input queue to the
            // current foreground thread's input queue for the duration of
            // the SetForegroundWindow call — Windows then sees the call
            // as coming from the foreground process itself, and accepts
            // it without the flash.
            let fg = GetForegroundWindow();
            let fg_tid = GetWindowThreadProcessId(fg, None);
            let target_tid = GetWindowThreadProcessId(hwnd, None);
            let cur_tid = GetCurrentThreadId();

            let attached_fg = fg_tid != 0 && fg_tid != cur_tid
                && AttachThreadInput(cur_tid, fg_tid, true).as_bool();
            let attached_target = target_tid != 0 && target_tid != cur_tid
                && AttachThreadInput(cur_tid, target_tid, true).as_bool();

            let _ = SetForegroundWindow(hwnd);
            let _ = BringWindowToTop(hwnd);

            if attached_target { let _ = AttachThreadInput(cur_tid, target_tid, false); }
            if attached_fg     { let _ = AttachThreadInput(cur_tid, fg_tid,     false); }
        }
    }

    /// Ask a window to close, the same way the user clicking its native
    /// close button would. `WM_CLOSE` is the polite-shutdown message —
    /// apps get to prompt for unsaved work, save state, etc. We don't use
    /// `DestroyWindow` because that's a hard kill that bypasses the app's
    /// own teardown logic (and it'd fail across process boundaries
    /// anyway — DestroyWindow only works on windows owned by the calling
    /// thread).
    pub fn close(&self, id: WindowId) {
        let Some(hwnd) = self.map.lookup_hwnd(id) else { return };
        unsafe {
            use windows::Win32::Foundation::{LPARAM, WPARAM};
            use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_CLOSE};
            let _ = PostMessageW(hwnd, WM_CLOSE, WPARAM(0), LPARAM(0));
        }
    }
}

/// Returns (left, top, width-extra, height-extra) — how much wider/taller
/// to make the SetWindowPos rect so the visible frame matches the target.
fn invisible_frame_offset(hwnd: HWND) -> (i32, i32, i32, i32) {
    unsafe {
        let mut visible = RECT::default();
        let mut actual  = RECT::default();
        if DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut visible as *mut _ as *mut _,
            std::mem::size_of::<RECT>() as u32,
        ).is_err() {
            return (0, 0, 0, 0);
        }
        if GetWindowRect(hwnd, &mut actual).is_err() {
            return (0, 0, 0, 0);
        }
        let dx = visible.left - actual.left;
        let dy = visible.top  - actual.top;
        let dw = (actual.right - actual.left) - (visible.right - visible.left);
        let dh = (actual.bottom - actual.top) - (visible.bottom - visible.top);
        (dx, dy, dw, dh)
    }
}
