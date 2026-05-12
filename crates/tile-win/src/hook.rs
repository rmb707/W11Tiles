//! WinEventHook plumbing.
//!
//! `SetWinEventHook(WINEVENT_OUTOFCONTEXT)` lets us subscribe to system-
//! wide window lifecycle events without injecting a DLL. The hook callback
//! runs on a thread that pumps a Win32 message loop — this module owns
//! that thread and forwards events as `tile_core::Event`s into a
//! tokio mpsc channel that the daemon awaits.
//!
//! Events we care about:
//!   EVENT_OBJECT_CREATE      → maybe-window-opened (re-check is_manageable)
//!   EVENT_OBJECT_DESTROY     → window-closed
//!   EVENT_OBJECT_SHOW        → un-cloaked (UWP windows open this way)
//!   EVENT_OBJECT_HIDE        → cloaked (treat as close until re-shown)
//!   EVENT_SYSTEM_FOREGROUND  → focus changed
//!   EVENT_SYSTEM_MOVESIZEEND → user dragged → toggle to floating
//!   EVENT_SYSTEM_MINIMIZEEND → restored → re-tile
//!
//! We deliberately ignore EVENT_OBJECT_LOCATIONCHANGE — it fires hundreds
//! of times during a single drag and would saturate our channel.

#![cfg(windows)]

use std::sync::Arc;
use std::thread::{self, JoinHandle};

use parking_lot::Mutex;
use tile_core::{WindowId, WindowInfo};
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, warn};

/// Events as the hook sees them. The daemon translates these into
/// `tile_core::Event`s, which requires resolving each window's virtual
/// desktop (a COM call we don't want to make from the hook callback).
/// `raw_hwnd` is a transparent isize so it's `Send` across the channel —
/// the daemon reconstructs `HWND` on its own thread.
#[derive(Debug, Clone)]
pub enum HookEvent {
    Opened   { raw_hwnd: isize, id: WindowId, info: WindowInfo },
    Closed   { id: WindowId },
    Focused  { raw_hwnd: isize, id: WindowId },
    Floated  { id: WindowId },
    /// User minimized the window (clicked _ or pressed Win+Down).
    /// The daemon treats this as a temporary float: drop from the BSP
    /// so the cell collapses, then re-insert on `Restored`. Without
    /// this, the minimized window's tile becomes a "ghost" — invisible
    /// but reserved — and new windows can't claim the slot.
    Minimized { id: WindowId },
    Restored { raw_hwnd: isize, id: WindowId },
    /// User finished dragging `src`. `cursor` is the screen position
    /// where they released the mouse. The daemon decides whether this is
    /// a merge (cursor on another tile) or a float (cursor on its own
    /// tile / nothing) by hit-testing the cursor against the active
    /// layout — much more reliable than `WindowFromPoint` peeks, which
    /// can't actually see beneath the dragged window itself.
    DragEnded { src: WindowId, cursor_x: i32, cursor_y: i32 },
    /// User started dragging `src`. The daemon can use this to paint
    /// drop-zone highlights on every other tile so the user can see
    /// which cells are valid merge targets.
    DragStarted { src: WindowId },
}

use windows::Win32::Foundation::{HMODULE, HWND, LPARAM, POINT, WPARAM};
use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetCursorPos, GetMessageW, PostThreadMessageW, TranslateMessage, MSG,
    EVENT_OBJECT_CREATE, EVENT_OBJECT_DESTROY, EVENT_OBJECT_HIDE, EVENT_OBJECT_SHOW,
    EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_MINIMIZEEND, EVENT_SYSTEM_MINIMIZESTART,
    EVENT_SYSTEM_MOVESIZEEND, EVENT_SYSTEM_MOVESIZESTART,
    OBJID_WINDOW, WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS, WM_QUIT,
};

use crate::hwnd_map::HwndMap;
use crate::manageable::{class_of, is_manageable, title_of};

/// Owns the background thread that pumps WinEvents.
pub struct EventHook {
    thread: Option<JoinHandle<()>>,
    thread_id: u32,
}

struct HookCtx {
    map: Arc<HwndMap>,
    tx: UnboundedSender<HookEvent>,
}

// SAFETY: Only ever read from the hook callback. Set once before the hook is
// installed, cleared after it's removed. The Mutex makes the once-init safe.
static CTX: Mutex<Option<Arc<HookCtx>>> = Mutex::new(None);

impl EventHook {
    pub fn start(map: Arc<HwndMap>, tx: UnboundedSender<HookEvent>) -> Self {
        let ctx = Arc::new(HookCtx { map, tx });
        *CTX.lock() = Some(ctx);

        let (tid_tx, tid_rx) = std::sync::mpsc::channel();
        let thread = thread::spawn(move || {
            let tid = unsafe { windows::Win32::System::Threading::GetCurrentThreadId() };
            let _ = tid_tx.send(tid);

            // Single hook covers the full event range we want — they're contiguous
            // enough that one call is cheaper than five.
            let h_create_destroy = unsafe {
                SetWinEventHook(
                    EVENT_OBJECT_CREATE,
                    EVENT_OBJECT_HIDE,
                    HMODULE::default(),
                    Some(callback),
                    0, 0,
                    WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
                )
            };
            let h_system = unsafe {
                SetWinEventHook(
                    EVENT_SYSTEM_FOREGROUND,
                    EVENT_SYSTEM_MINIMIZEEND,
                    HMODULE::default(),
                    Some(callback),
                    0, 0,
                    WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
                )
            };
            if h_create_destroy.is_invalid() || h_system.is_invalid() {
                warn!("SetWinEventHook returned an invalid handle");
            }

            // Pump messages until WM_QUIT.
            let mut msg = MSG::default();
            unsafe {
                while GetMessageW(&mut msg, HWND::default(), 0, 0).into() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }

            unsafe {
                let _ = UnhookWinEvent(h_create_destroy);
                let _ = UnhookWinEvent(h_system);
            }
            *CTX.lock() = None;
        });

        let thread_id = tid_rx.recv().unwrap_or(0);
        Self { thread: Some(thread), thread_id }
    }

    pub fn stop(mut self) {
        if self.thread_id != 0 {
            unsafe {
                let _ = PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
            }
        }
        if let Some(t) = self.thread.take() { let _ = t.join(); }
    }
}

unsafe extern "system" fn callback(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    id_object: i32,
    id_child: i32,
    _thread: u32,
    _ms: u32,
) {
    // Only window-level events; ignore caret/menu/etc. and child sub-events.
    if id_object != OBJID_WINDOW.0 || id_child != 0 || hwnd.is_invalid() {
        return;
    }

    let Some(ctx) = CTX.lock().clone() else { return };

    let raw_hwnd = hwnd.0 as isize;
    let mapped = match event {
        EVENT_OBJECT_CREATE | EVENT_OBJECT_SHOW => {
            if !is_manageable(hwnd) { return; }
            // Killer heuristic for confirmation dialogs: if this window's
            // owner is already a tracked window, it's a transient dialog
            // of that window — "Save changes?", "Discard?", login prompts,
            // EULA pages, options sub-windows. is_manageable already
            // catches the WS_EX_APPWINDOW-less cases; this catches dialogs
            // that incorrectly mark themselves WS_EX_APPWINDOW (some games,
            // some Electron apps), which otherwise slip through.
            unsafe {
                use windows::Win32::UI::WindowsAndMessaging::{GetWindow, GW_OWNER};
                if let Ok(owner) = GetWindow(hwnd, GW_OWNER) {
                    if !owner.is_invalid() {
                        let owner_raw = owner.0 as isize;
                        if ctx.map.peek(owner_raw).is_some() {
                            tracing::debug!(
                                class = %class_of(hwnd),
                                title = %title_of(hwnd),
                                "skipping owned-by-tracked: looks like a dialog"
                            );
                            return;
                        }
                    }
                }
            }
            let id = ctx.map.intern(hwnd);
            let info = WindowInfo::new(id, title_of(hwnd), class_of(hwnd));
            Some(HookEvent::Opened { raw_hwnd, id, info })
        }
        EVENT_OBJECT_DESTROY => {
            // True window destruction. Forget the HWND so we don't carry a
            // stale entry. Daemon removes the window from layout state.
            ctx.map.forget(hwnd).map(|id| HookEvent::Closed { id })
        }
        EVENT_OBJECT_HIDE => {
            // HIDE fires for both true close-via-hide AND cloak (the OS
            // hides windows on non-current virtual desktops). Treating
            // HIDE the same as DESTROY meant every VD switch tore down
            // the user's careful tile arrangement on the leaving VD —
            // they'd come back and have to re-arrange. Now we ignore
            // HIDE entirely; DESTROY catches real closes, and the
            // periodic discover sweep will re-route any window we
            // mistakenly retained. A small risk: an app that hides its
            // main window without destroying it (e.g., minimize-to-tray)
            // keeps its tile slot until the periodic discover walks past
            // it and finds no matching HWND. Acceptable trade-off.
            None
        }
        EVENT_SYSTEM_FOREGROUND => {
            if !is_manageable(hwnd) { return; }
            let id = ctx.map.intern(hwnd);
            Some(HookEvent::Focused { raw_hwnd, id })
        }
        EVENT_SYSTEM_MOVESIZESTART => {
            let src = ctx.map.intern(hwnd);
            Some(HookEvent::DragStarted { src })
        }
        EVENT_SYSTEM_MOVESIZEEND => {
            // User finished a manual drag/resize. Capture the cursor
            // position and let the daemon decide what to do — it has the
            // layout, so it can cell-hit-test the cursor far more
            // reliably than we can hit-test windows from here.
            let src = ctx.map.intern(hwnd);
            let mut pt = POINT::default();
            let (cursor_x, cursor_y) = if GetCursorPos(&mut pt).is_ok() {
                (pt.x, pt.y)
            } else {
                (0, 0)
            };
            Some(HookEvent::DragEnded { src, cursor_x, cursor_y })
        }
        EVENT_SYSTEM_MINIMIZESTART => {
            // Don't gate on is_manageable: by the time MINIMIZESTART
            // fires the window is already iconic, which our manageability
            // filter rejects. We just need to know the HWND was something
            // we were tracking.
            let raw = hwnd.0 as isize;
            ctx.map.peek(raw).map(|id| HookEvent::Minimized { id })
        }
        EVENT_SYSTEM_MINIMIZEEND => {
            if !is_manageable(hwnd) { return; }
            let id = ctx.map.intern(hwnd);
            Some(HookEvent::Restored { raw_hwnd, id })
        }
        _ => None,
    };

    if let Some(ev) = mapped {
        if let Err(e) = ctx.tx.send(ev) {
            debug!("hook channel closed: {e}");
        }
    }
}

// Sanity: WindowId import used.
const _: fn() = || { let _: WindowId = WindowId(0); };
