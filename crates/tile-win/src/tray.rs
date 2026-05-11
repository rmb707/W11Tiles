//! Win32 system-tray icon.
//!
//! Adds a single icon to the notification area (the cluster near the
//! taskbar clock). Right-click pops a small menu — Reload Config / About /
//! Quit — and each pick lands as a [`TrayCommand`] on the daemon's mpsc
//! channel. Without a tray entry the daemon is invisible: there's no
//! window, no taskbar button, nothing the user can interact with except
//! keybinds. The tray is the smallest "this is running, here's how to
//! talk to it" affordance Windows offers, which is why it's worth the
//! Win32 ceremony below.
//!
//! ## Threading model
//!
//! Mirrors [`crate::tab_strip`] and [`crate::drop_zones`]:
//!
//! - One worker thread owns the hidden message-only window and pumps
//!   `GetMessageW`. Win32 routes the tray callback (`Shell_NotifyIconW`)
//!   to that window, which means the WndProc must run on a thread with a
//!   live message loop.
//! - The daemon (any thread) gets a handle back from [`TrayManager::start`]
//!   and uses [`TrayManager::stop`] to post `WM_QUIT` and join.
//! - User picks travel back to the daemon over an `UnboundedSender<TrayCommand>`
//!   stashed in a `static SHARED: Mutex<Option<Arc<Shared>>>` so the
//!   `extern "system"` WndProc — which can't capture closures — can reach it.
//!
//! ## Icon
//!
//! v1 uses `IDI_APPLICATION` (the generic Win32 app icon) so we don't
//! ship a binary asset. A custom `.ico` can be loaded with `LoadImageW`
//! later and slotted into the same `NIM_MODIFY` flow without changing the
//! public surface.
//!
//! ## Sharp edges
//!
//! - `Shell_NotifyIconW(NIM_ADD)` can fail silently if the user runs two
//!   daemon instances: both register the same `(hwnd, uID)` pair against
//!   their own hidden window, but Explorer dedupes on icon identity at
//!   the shell level and the second add may not appear. First instance
//!   wins; nothing in the daemon currently prevents a second instance,
//!   so the integrator should rely on the IPC singleton check upstream.
//! - On Explorer crash + restart the icon is lost. We don't currently
//!   listen for `TaskbarCreated` and re-add — for v1 the user can restart
//!   the daemon. Worth adding before 1.0; tracked for the integrator.

#![cfg(windows)]

use std::sync::Arc;
use std::thread::{self, JoinHandle};

use parking_lot::Mutex;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, warn};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DispatchMessageW,
    GetCursorPos, GetMessageW, LoadIconW, PostMessageW, PostThreadMessageW, RegisterClassExW,
    SetForegroundWindow, TrackPopupMenu, TranslateMessage, HCURSOR, HICON, HMENU, HWND_MESSAGE,
    IDI_APPLICATION, MF_STRING, MSG, TPM_LEFTALIGN, TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_COMMAND,
    WM_LBUTTONUP, WM_NULL, WM_QUIT, WM_RBUTTONUP, WM_USER, WNDCLASSEXW,
};

/// Custom callback message Shell_NotifyIconW posts to our hidden window
/// when the user interacts with the tray icon. Win32 reserves anything
/// `>= WM_USER` for app-defined; +17 is arbitrary but distinct from the
/// `WM_USER + 1` values our overlay modules use, in case a shared static
/// ever winds up cross-routing messages during debugging.
const WM_TILETRAY_CALLBACK: u32 = WM_USER + 17;

/// Tag identifying *our* icon in `NOTIFYICONDATAW.uID`. A single process
/// can register multiple tray icons against the same window; the tag is
/// how the shell tells them apart. We only ever have one, so the value
/// is arbitrary — just keep it stable across `NIM_ADD`/`NIM_DELETE`.
const TRAY_ICON_UID: u32 = 1;

/// Menu command IDs. `LOWORD(wparam)` of `WM_COMMAND` carries the picked
/// item's ID, which we map back to a [`TrayCommand`] in the WndProc. The
/// numeric values are private to this module — the daemon never sees them.
const CMD_RELOAD:    u32 = 1001;
const CMD_ABOUT:     u32 = 1002;
const CMD_QUIT:      u32 = 1003;
const CMD_SHORTCUTS: u32 = 1004;

/// Class name registered with `RegisterClassExW`. UTF-16, NUL-terminated,
/// matches the byte-literal style used by `tab_strip.rs` / `drop_zones.rs`
/// so all three modules look the same to a reviewer.
const CLASS_NAME_W: &[u16] = &[
    b'T' as u16, b'i' as u16, b'l' as u16, b'e' as u16, b'M' as u16, b'a' as u16, b'n' as u16,
    b'a' as u16, b'g' as u16, b'e' as u16, b'r' as u16, b'T' as u16, b'r' as u16, b'a' as u16,
    b'y' as u16, 0,
];

/// Tooltip shown when the user hovers the tray icon. UTF-16, must fit in
/// 128 chars including the trailing NUL — see [`copy_tip_into`].
const TOOLTIP: &str = "TileManager";

// ---- public surface -------------------------------------------------------

/// What the user picked from the tray's right-click menu. The daemon
/// owns the policy for how to react (e.g., About may show a MessageBox
/// or simply log; Quit triggers a clean shutdown). This module just
/// translates clicks into intent.
#[derive(Debug, Clone, Copy)]
pub enum TrayCommand {
    ReloadConfig,
    About,
    /// Show the keyboard-shortcuts dialog. Daemon spawns a worker
    /// thread that pops a Win32 MessageBox listing every binding.
    Shortcuts,
    Quit,
}

pub struct TrayManager {
    thread: Option<JoinHandle<()>>,
    thread_id: u32,
}

struct Shared {
    cmd_tx: UnboundedSender<TrayCommand>,
}

/// Holds the cmd_tx sender so the `extern "system"` WndProc — which can't
/// capture environment — can reach it. Populated in [`TrayManager::start`],
/// cleared when the pump exits. Same pattern as the other tray-adjacent
/// modules in this crate.
static SHARED: Mutex<Option<Arc<Shared>>> = Mutex::new(None);

impl TrayManager {
    /// Spawn the worker thread, register the window class, install the
    /// tray icon, and return a handle. `cmd_tx` receives a [`TrayCommand`]
    /// each time the user picks a menu item. Failure to register the
    /// class or add the icon is logged and the worker exits silently —
    /// callers that need a hard "tray is up" signal should observe the
    /// channel for a probe message instead (not currently implemented;
    /// the daemon treats the tray as best-effort).
    pub fn start(cmd_tx: UnboundedSender<TrayCommand>) -> Self {
        let shared = Arc::new(Shared { cmd_tx });
        *SHARED.lock() = Some(shared);

        let (tid_tx, tid_rx) = std::sync::mpsc::channel();
        let thread = thread::spawn(move || {
            let tid = unsafe { windows::Win32::System::Threading::GetCurrentThreadId() };
            let _ = tid_tx.send(tid);

            if let Err(e) = register_class() {
                warn!("tray: register_class failed: {e}");
                *SHARED.lock() = None;
                return;
            }
            let hwnd = match create_message_window() {
                Some(h) => h,
                None => {
                    warn!("tray: create_message_window failed");
                    *SHARED.lock() = None;
                    return;
                }
            };
            if let Err(e) = add_tray_icon(hwnd) {
                warn!("tray: add_tray_icon failed: {e}");
                // Don't bail: even without an icon, the pump must drain
                // the WM_QUIT that `stop()` will post, otherwise the
                // worker thread leaks. Run the pump anyway; the user just
                // won't see an icon.
            }

            run_pump();

            // Best-effort cleanup. If NIM_ADD failed earlier this is a
            // no-op for a non-existent icon — the shell ignores deletes
            // for unknown (hwnd, uID) pairs, so it's safe to always call.
            unsafe {
                let mut data = base_notify_data(hwnd);
                let _ = Shell_NotifyIconW(NIM_DELETE, &mut data);
            }
            *SHARED.lock() = None;
        });

        let thread_id = tid_rx.recv().unwrap_or(0);
        Self { thread: Some(thread), thread_id }
    }

    /// Post `WM_QUIT` to the worker and join. Idempotent in the sense
    /// that calling this on an already-stopped manager is harmless —
    /// `PostThreadMessageW` to a dead thread fails gracefully and the
    /// join returns immediately. We don't expose a `Drop` impl that does
    /// this implicitly because the daemon's shutdown order matters and
    /// implicit teardown would mask ordering bugs.
    pub fn stop(mut self) {
        if self.thread_id != 0 {
            unsafe {
                let _ = PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
            }
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

// ---- worker-thread internals ---------------------------------------------

fn run_pump() {
    let mut msg = MSG::default();
    unsafe {
        loop {
            let r = GetMessageW(&mut msg, HWND::default(), 0, 0);
            // GetMessageW returns 0 on WM_QUIT and -1 on error; both
            // signal "stop pumping". `BOOL::as_bool()` collapses 0 → false
            // and any non-zero (including -1) → true, so we explicitly
            // check the raw value so an error doesn't spin forever.
            if r.0 <= 0 {
                break;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

fn register_class() -> Result<(), String> {
    let h_module =
        unsafe { GetModuleHandleW(None) }.map_err(|e| format!("GetModuleHandleW: {e}"))?;
    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: windows::Win32::UI::WindowsAndMessaging::WNDCLASS_STYLES(0),
        lpfnWndProc: Some(wnd_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: h_module.into(),
        hIcon: HICON::default(),
        hCursor: HCURSOR::default(),
        hbrBackground: windows::Win32::Graphics::Gdi::HBRUSH::default(),
        lpszMenuName: PCWSTR::null(),
        lpszClassName: PCWSTR(CLASS_NAME_W.as_ptr()),
        hIconSm: HICON::default(),
    };
    let atom = unsafe { RegisterClassExW(&wc) };
    if atom == 0 {
        // Re-registering the same class on a second `start()` in the same
        // process returns ERROR_CLASS_ALREADY_EXISTS (1410). Fine — the
        // existing registration is functionally identical.
        let err = unsafe { windows::Win32::Foundation::GetLastError() };
        if err.0 != 1410 {
            return Err(format!("RegisterClassExW failed: {:?}", err));
        }
    }
    Ok(())
}

/// Create the hidden message-only window. `HWND_MESSAGE` as parent makes
/// the window invisible to the desktop and excluded from window
/// enumeration — exactly what we want for a callback receiver. It still
/// has a real HWND, which is what `Shell_NotifyIconW` requires.
fn create_message_window() -> Option<HWND> {
    let h_module = unsafe { GetModuleHandleW(None).ok()? };
    let hwnd = unsafe {
        CreateWindowExW(
            windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE(0),
            PCWSTR(CLASS_NAME_W.as_ptr()),
            PCWSTR(CLASS_NAME_W.as_ptr()),
            windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            None,
            h_module,
            None,
        )
    };
    match hwnd {
        Ok(h) if !h.is_invalid() => Some(h),
        Ok(_) => None,
        Err(e) => {
            warn!("tray: CreateWindowExW failed: {e}");
            None
        }
    }
}

/// Build the boilerplate part of `NOTIFYICONDATAW` shared by ADD and
/// DELETE calls. ADD then layers on icon/tip/callback fields; DELETE
/// only needs hWnd + uID + cbSize, but it's harmless to over-supply.
fn base_notify_data(hwnd: HWND) -> NOTIFYICONDATAW {
    let mut data = NOTIFYICONDATAW::default();
    // CRITICAL: cbSize must be the exact struct size or the shell
    // rejects the call as ERROR_INVALID_PARAMETER. NOTIFYICONDATAW has
    // grown across Windows versions; using the current Rust binding's
    // `size_of` keeps us aligned with whatever shell we're talking to.
    data.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    data.hWnd = hwnd;
    data.uID = TRAY_ICON_UID;
    data
}

/// Copy a Rust `&str` into a fixed `[u16; 128]` buffer (UTF-16,
/// NUL-terminated). The shell silently truncates anything past index 127
/// in `szTip`, but it's better to truncate ourselves so callers see a
/// predictable string. Anything we'd put here is short enough to fit; the
/// guard is defensive.
fn copy_tip_into(dst: &mut [u16; 128], s: &str) {
    let mut i = 0usize;
    for u in s.encode_utf16() {
        if i >= dst.len() - 1 {
            break;
        }
        dst[i] = u;
        i += 1;
    }
    dst[i] = 0;
}

fn add_tray_icon(hwnd: HWND) -> Result<(), String> {
    let mut data = base_notify_data(hwnd);
    data.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
    data.uCallbackMessage = WM_TILETRAY_CALLBACK;
    // Try our runtime-generated "TM" monogram first (accent square, white
    // bold letters). Fall back to the generic Win32 IDI_APPLICATION if
    // the GDI render fails for any reason — Shell_NotifyIcon requires a
    // valid HICON to register at all.
    data.hIcon = match crate::icon::create_app_icon() {
        Some(h) => h,
        None => unsafe {
            LoadIconW(None, IDI_APPLICATION).map_err(|e| format!("LoadIconW: {e}"))?
        },
    };
    copy_tip_into(&mut data.szTip, TOOLTIP);

    let ok = unsafe { Shell_NotifyIconW(NIM_ADD, &mut data) };
    if !ok.as_bool() {
        return Err("Shell_NotifyIconW(NIM_ADD) returned false".into());
    }
    Ok(())
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_TILETRAY_CALLBACK => {
            // Per Shell_NotifyIcon docs, the *mouse event* lives in the
            // low word of lparam; the high word is the icon ID (always
            // TRAY_ICON_UID for us, so we don't bother checking).
            let event = (lparam.0 as u32) & 0xFFFF;
            match event {
                WM_RBUTTONUP => show_context_menu(hwnd),
                WM_LBUTTONUP => {
                    // No-op for v1. Could route to a Toggle command later;
                    // simplest behavior now is "right-click is the only
                    // affordance" which matches OneDrive / GitHub Desktop /
                    // most utility tray apps.
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_COMMAND => {
            // Menu picks land here. LOWORD(wParam) = command ID.
            let id = (wparam.0 as u32) & 0xFFFF;
            if let Some(cmd) = command_from_id(id) {
                if let Some(shared) = SHARED.lock().clone() {
                    if let Err(e) = shared.cmd_tx.send(cmd) {
                        debug!("tray: cmd channel closed: {e}");
                    }
                }
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// Show the right-click popup menu at the current cursor position.
/// Splitting this out keeps the WndProc body small and lets the GDI
/// resources (HMENU) be cleaned up on every code path.
unsafe fn show_context_menu(hwnd: HWND) {
    let mut pt = POINT::default();
    if GetCursorPos(&mut pt).is_err() {
        return;
    }

    let menu = match CreatePopupMenu() {
        Ok(m) => m,
        Err(e) => {
            warn!("tray: CreatePopupMenu failed: {e}");
            return;
        }
    };

    append_menu_str(menu, CMD_SHORTCUTS, "Keyboard Shortcuts…");
    append_menu_str(menu, CMD_RELOAD,    "Reload Config");
    append_menu_str(menu, CMD_ABOUT,     "About");
    append_menu_str(menu, CMD_QUIT,      "Quit");

    // SetForegroundWindow on our hidden window before TrackPopupMenu is
    // required by the Win32 docs; without it the menu can be left
    // dangling if the user clicks outside without making a selection.
    // The companion PostMessageW(WM_NULL, ...) after the call is the
    // documented "flush" that prevents the same dangling-menu state on
    // some shell versions. Both are cheap and well worth it.
    let _ = SetForegroundWindow(hwnd);

    // TPM_RETURNCMD makes TrackPopupMenu return the picked ID directly
    // instead of posting WM_COMMAND. We then post WM_COMMAND ourselves,
    // which keeps the dispatch path uniform (everything routes through
    // the WndProc's WM_COMMAND arm). This sidesteps a known issue where
    // WM_COMMAND posted to a message-only window from inside
    // TrackPopupMenu's modal loop can be eaten on some Windows builds.
    let cmd_id = TrackPopupMenu(
        menu,
        TPM_LEFTALIGN | TPM_RIGHTBUTTON | TPM_RETURNCMD,
        pt.x,
        pt.y,
        0,
        hwnd,
        None,
    );
    let _ = PostMessageW(hwnd, WM_NULL, WPARAM(0), LPARAM(0));

    // TrackPopupMenu returns the picked ID, or 0 if the user dismissed.
    if cmd_id.0 != 0 {
        let _ = PostMessageW(hwnd, WM_COMMAND, WPARAM(cmd_id.0 as usize), LPARAM(0));
    }

    let _ = DestroyMenu(menu);
}

/// Append a single string item to `menu`. Wraps the UTF-16 conversion +
/// MF_STRING boilerplate so the call site at `show_context_menu` reads
/// like a list of items, not a wall of FFI.
unsafe fn append_menu_str(menu: HMENU, id: u32, text: &str) {
    let mut wide: Vec<u16> = text.encode_utf16().collect();
    wide.push(0);
    let _ = AppendMenuW(menu, MF_STRING, id as usize, PCWSTR(wide.as_ptr()));
}

/// Map a menu command ID back to the public [`TrayCommand`] enum. Kept
/// pure (no I/O, no globals) so it stays trivially correct under review;
/// the WndProc handles channel send separately.
fn command_from_id(id: u32) -> Option<TrayCommand> {
    match id {
        CMD_RELOAD    => Some(TrayCommand::ReloadConfig),
        CMD_ABOUT     => Some(TrayCommand::About),
        CMD_SHORTCUTS => Some(TrayCommand::Shortcuts),
        CMD_QUIT      => Some(TrayCommand::Quit),
        _ => None,
    }
}
