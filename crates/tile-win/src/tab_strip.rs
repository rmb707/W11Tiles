//! Tab-strip overlay windows.
//!
//! For each `Node::Tabbed` group in the active layout we paint a small
//! borderless overlay across the top `TAB_STRIP_HEIGHT` of the cell.
//! Clicking a tab on the strip switches which window in the group is
//! visible, mirroring i3/Hyprland's tabbed container UX. The strip is
//! the *only* visible indicator that a tab group exists, so without
//! this module the user has no way to know which windows are stacked
//! together (or to switch between them with the mouse).
//!
//! ## Threading model
//!
//! - One worker thread owns every overlay window. Win32 lets a single
//!   thread own arbitrarily many top-level windows, so we don't need
//!   one thread per overlay.
//! - The worker pumps `GetMessageW` so DWM can drive `WM_PAINT` and
//!   user clicks land as `WM_LBUTTONDOWN`.
//! - The daemon (any thread) calls [`TabStripManager::update`] with a
//!   fresh `Vec<StripDescriptor>` after every layout repaint. Internally
//!   that updates a `Mutex<Vec<...>>` and posts a thread message to wake
//!   the pump; the pump drains the pending updates on `WM_USER`.
//! - Clicks travel back to the daemon via an `mpsc::UnboundedSender<WindowId>`.
//!
//! ## Diffing strategy
//!
//! We identify a tab group by the *sorted* list of its `WindowId`s. As
//! long as the same windows stay together (no merge/untab), the same
//! overlay HWND is reused across layouts — we just `SetWindowPos` it.
//! When membership changes, the old overlay is destroyed and a new one
//! created. This avoids flicker on every keystroke that triggers a
//! repaint without growing the overlay set unboundedly.
//!
//! ## What we deliberately don't do
//!
//! No animations, no hover effects, no Mica/Acrylic. GDI solid fills +
//! `DrawTextW`. Polish is a follow-up; what's here is functional and
//! visible.

#![cfg(windows)]

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use parking_lot::Mutex;
use tile_core::{Rect, TabGroupView, WindowId};
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, warn};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontW, CreatePen, CreateSolidBrush, DeleteObject, DrawTextW, EndPaint,
    FillRect, InvalidateRect, LineTo, MoveToEx, SelectObject, SetBkMode, SetTextColor,
    CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DEFAULT_QUALITY, DT_END_ELLIPSIS, DT_SINGLELINE,
    DT_VCENTER, FF_DONTCARE, FW_NORMAL, HBRUSH, HFONT, HGDIOBJ, OUT_DEFAULT_PRECIS, PAINTSTRUCT,
    PS_SOLID, TRANSPARENT, VARIABLE_PITCH,
};
use windows::Win32::UI::HiDpi::{SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect, GetMessageW,
    GetWindowLongPtrW, PostThreadMessageW, RegisterClassExW, SetLayeredWindowAttributes,
    SetWindowLongPtrW, SetWindowPos, ShowWindow, TranslateMessage, GWLP_USERDATA, HCURSOR, HICON,
    HWND_TOPMOST, LWA_ALPHA, MSG, SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SWP_NOZORDER, WM_DESTROY,
    WM_LBUTTONDOWN, WM_PAINT, WM_QUIT, WM_USER, WNDCLASSEXW, WS_EX_LAYERED, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

/// Reserved height for the strip — must match `tile_core::layout::TAB_STRIP_HEIGHT`.
const STRIP_HEIGHT: i32 = tile_core::layout::TAB_STRIP_HEIGHT;

/// Width of the close-X button on the right side of each tab. The visible
/// "X" glyph itself is smaller; the button is the click hit-zone.
const CLOSE_BTN_W: i32 = 22;

/// Minimum cell width below which the tab strip is suppressed entirely.
/// At this width the strip is too cramped for even a single tab's title +
/// X to be legible — better to render nothing than illegible noise. The
/// user can still cycle tabs via keybind/CLI.
const MIN_STRIP_WIDTH: i32 = 100;

/// Per-tab width below which the close-X is hidden. The X overlaps with
/// title text on narrow tabs and reads as visual garbage; hiding it
/// preserves readable titles. Closing falls back to keybind/CLI on a
/// too-narrow tab — acceptable trade-off, since the strip itself is the
/// only mouse-discoverable surface and the strip still works.
const MIN_TAB_W_FOR_CLOSE: i32 = 80;

/// Layered-window alpha for the strip. 245/255 ≈ 96% — reads as proper UI
/// chrome rather than translucent overlay. The strip is small (28px tall)
/// so we don't sacrifice meaningful visibility of the tab body beneath.
const STRIP_ALPHA: u8 = 245;

/// Accent BGR. Slightly darker than the original `0x00C56A19` so white
/// text reads with comfortable contrast across both Light and Dark themes
/// (the original tested borderline against pure white under Light).
const ACCENT_BGR: u32 = 0x00B05010;

/// What the user did to a tab on the strip. The daemon decides what to
/// do with each: activate switches the visible tab; close asks the
/// underlying window to close (`WM_CLOSE`, which lets unsaved-work
/// dialogs prompt normally).
#[derive(Debug, Clone)]
pub enum TabAction {
    Activate(WindowId),
    Close(WindowId),
}

/// Custom thread message: pump should drain [`SHARED.pending`].
const WM_TILESTRIP_UPDATE: u32 = WM_USER + 1;

/// Class name registered with Win32 — wide string, NUL-terminated.
const CLASS_NAME_W: &[u16] = &[
    b'T' as u16, b'i' as u16, b'l' as u16, b'e' as u16, b'M' as u16, b'a' as u16, b'n' as u16,
    b'a' as u16, b'g' as u16, b'e' as u16, b'r' as u16, b'T' as u16, b'a' as u16, b'b' as u16,
    b'S' as u16, b't' as u16, b'r' as u16, b'i' as u16, b'p' as u16, 0,
];

#[derive(Debug, Clone)]
pub struct StripDescriptor {
    /// Full cell rect of the tabbed node — strip paints in the top
    /// `STRIP_HEIGHT` band of this.
    pub cell: Rect,
    /// (id, title) pairs, in tab order.
    pub tabs: Vec<(WindowId, String)>,
    pub active: usize,
}

impl StripDescriptor {
    pub fn from_view(view: &TabGroupView, lookup_title: impl Fn(WindowId) -> String) -> Self {
        Self {
            cell: view.cell,
            tabs: view.tabs.iter().map(|id| (*id, lookup_title(*id))).collect(),
            active: view.active,
        }
    }

    /// Stable identifier for this tab group based on the *set* of
    /// windows in it. Tab order or active index doesn't change identity —
    /// only adding/removing tabs does. Lets the overlay manager reuse
    /// HWNDs across layout repaints.
    fn group_key(&self) -> u64 {
        let mut ids: Vec<u64> = self.tabs.iter().map(|(id, _)| id.0).collect();
        ids.sort_unstable();
        // FNV-1a is fine; we just need a stable hash with low collision risk.
        let mut h: u64 = 0xcbf29ce484222325;
        for id in ids {
            for byte in id.to_le_bytes() {
                h ^= byte as u64;
                h = h.wrapping_mul(0x100000001b3);
            }
        }
        h
    }
}

// ---- public manager -------------------------------------------------------

pub struct TabStripManager {
    thread: Option<JoinHandle<()>>,
    thread_id: u32,
}

struct Shared {
    pending: Mutex<Option<Vec<StripDescriptor>>>,
    click_tx: UnboundedSender<TabAction>,
}

static SHARED: Mutex<Option<Arc<Shared>>> = Mutex::new(None);

impl TabStripManager {
    /// Spawn the worker thread, register the window class, return a handle.
    /// `click_tx` receives a `TabAction` whenever the user interacts with a
    /// tab strip — `Activate(id)` for tab-body clicks and `Close(id)` for
    /// the X button.
    pub fn start(click_tx: UnboundedSender<TabAction>) -> Self {
        let shared = Arc::new(Shared {
            pending: Mutex::new(None),
            click_tx,
        });
        *SHARED.lock() = Some(shared);

        let (tid_tx, tid_rx) = std::sync::mpsc::channel();
        let thread = thread::spawn(move || {
            // Inherit per-monitor DPI awareness on this thread explicitly.
            // The process default carries through, but a thread that paints
            // and creates windows on multiple monitors with different
            // scales can otherwise get virtualized coordinates. Cheap to
            // be defensive.
            unsafe {
                let _ = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            }
            let tid = unsafe { windows::Win32::System::Threading::GetCurrentThreadId() };
            let _ = tid_tx.send(tid);
            if let Err(e) = register_class() {
                warn!("tab_strip: register_class failed: {e}");
                return;
            }
            run_pump();
            *SHARED.lock() = None;
        });

        let thread_id = tid_rx.recv().unwrap_or(0);
        Self { thread: Some(thread), thread_id }
    }

    /// Replace the entire current set of strips with `strips`. The worker
    /// diffs against its current overlay set and creates / updates /
    /// destroys windows accordingly.
    pub fn update(&self, strips: Vec<StripDescriptor>) {
        if let Some(shared) = SHARED.lock().clone() {
            *shared.pending.lock() = Some(strips);
            if self.thread_id != 0 {
                unsafe {
                    let _ = PostThreadMessageW(
                        self.thread_id, WM_TILESTRIP_UPDATE, WPARAM(0), LPARAM(0),
                    );
                }
            }
        }
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

// ---- worker-thread state --------------------------------------------------

/// Per-overlay state. Stored as a heap pointer in the HWND's `GWLP_USERDATA`
/// so the WndProc can recover it on every message.
struct Overlay {
    hwnd: HWND,
    descriptor: StripDescriptor,
}

// SAFETY: HWND is just an opaque handle. The pointer the worker thread
// stuffs into GWLP_USERDATA is only ever read by the WndProc on the same
// thread. The Send/Sync bounds aren't crossed in practice — `Overlay` is
// never moved off the worker.
unsafe impl Send for Overlay {}
unsafe impl Sync for Overlay {}

fn run_pump() {
    let mut overlays: HashMap<u64, Box<Overlay>> = HashMap::new();
    let mut msg = MSG::default();
    unsafe {
        loop {
            let r = GetMessageW(&mut msg, HWND::default(), 0, 0);
            if !r.as_bool() { break; }
            if msg.hwnd.is_invalid() && msg.message == WM_TILESTRIP_UPDATE {
                if let Some(shared) = SHARED.lock().clone() {
                    if let Some(strips) = shared.pending.lock().take() {
                        diff_and_apply(&mut overlays, strips);
                    }
                }
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        // Cleanup: destroy every overlay before exiting.
        for (_, ov) in overlays.drain() {
            let _ = DestroyWindow(ov.hwnd);
        }
    }
}

fn diff_and_apply(overlays: &mut HashMap<u64, Box<Overlay>>, strips: Vec<StripDescriptor>) {
    // Filter out cells too small for a useful strip *before* computing the
    // incoming key set — sub-threshold strips are treated as if they were
    // never sent, so any existing overlay for them gets destroyed below.
    let strips: Vec<StripDescriptor> = strips
        .into_iter()
        .filter(|s| s.cell.width >= MIN_STRIP_WIDTH && s.cell.height >= STRIP_HEIGHT)
        .collect();

    let incoming_keys: HashSet<u64> = strips.iter().map(|s| s.group_key()).collect();

    // Drop overlays whose group disappeared.
    let to_drop: Vec<u64> = overlays.keys().copied().filter(|k| !incoming_keys.contains(k)).collect();
    for key in to_drop {
        if let Some(ov) = overlays.remove(&key) {
            unsafe { let _ = DestroyWindow(ov.hwnd); }
        }
    }

    // Create or update.
    for strip in strips {
        let key = strip.group_key();
        match overlays.get_mut(&key) {
            Some(existing) => {
                // Reposition + refresh title/active state.
                existing.descriptor = strip.clone();
                let r = strip_rect(&strip.cell);
                unsafe {
                    let _ = SetWindowPos(
                        existing.hwnd, HWND_TOPMOST, r.x, r.y, r.width, r.height,
                        SWP_NOACTIVATE | SWP_NOZORDER,
                    );
                    let _ = InvalidateRect(existing.hwnd, None, true);
                }
            }
            None => {
                if let Some(ov) = create_overlay(strip) {
                    overlays.insert(key, ov);
                }
            }
        }
    }
}

fn strip_rect(cell: &Rect) -> Rect {
    Rect::new(cell.x, cell.y, cell.width, STRIP_HEIGHT.min(cell.height))
}

fn create_overlay(strip: StripDescriptor) -> Option<Box<Overlay>> {
    let r = strip_rect(&strip.cell);
    let h_module = unsafe { GetModuleHandleW(None).ok()? };

    let hwnd = unsafe {
        CreateWindowExW(
            // Layered for alpha, NOACTIVATE so clicks don't steal focus,
            // TOOLWINDOW so we don't show in taskbar/Alt+Tab, TOPMOST so
            // we paint above the active tab.
            WS_EX_LAYERED | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            PCWSTR(CLASS_NAME_W.as_ptr()),
            PCWSTR(CLASS_NAME_W.as_ptr()), // window text — never visible
            WS_POPUP,
            r.x, r.y, r.width, r.height,
            HWND::default(),
            None,
            h_module,
            None,
        )
    };
    let hwnd = match hwnd {
        Ok(h) if !h.is_invalid() => h,
        Ok(_) => { warn!("CreateWindowExW returned invalid HWND for tab strip"); return None; }
        Err(e) => { warn!("CreateWindowExW failed for tab strip: {e}"); return None; }
    };

    // Near-opaque (see `STRIP_ALPHA`): reads as proper UI chrome. A tiny
    // amount of transparency still hints this isn't owned by the underlying
    // window, but the strip is too thin to gain meaningful visibility from
    // a lower alpha.
    unsafe {
        let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), STRIP_ALPHA, LWA_ALPHA);
    }

    let mut overlay = Box::new(Overlay { hwnd, descriptor: strip });
    let ptr = &mut *overlay as *mut Overlay;
    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, ptr as isize);
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    }
    Some(overlay)
}

fn register_class() -> Result<(), String> {
    let h_module = unsafe { GetModuleHandleW(None) }.map_err(|e| format!("GetModuleHandleW: {e}"))?;
    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: windows::Win32::UI::WindowsAndMessaging::WNDCLASS_STYLES(0),
        lpfnWndProc: Some(wnd_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: h_module.into(),
        hIcon: HICON::default(),
        hCursor: HCURSOR::default(),
        hbrBackground: HBRUSH::default(), // we paint the entire client area
        lpszMenuName: PCWSTR::null(),
        lpszClassName: PCWSTR(CLASS_NAME_W.as_ptr()),
        hIconSm: HICON::default(),
    };
    let atom = unsafe { RegisterClassExW(&wc) };
    if atom == 0 {
        // ERROR_CLASS_ALREADY_EXISTS (1410) is fine — second invocation
        // of `start()` in the same process should re-use the class.
        let err = unsafe { windows::Win32::Foundation::GetLastError() };
        if err.0 != 1410 {
            return Err(format!("RegisterClassExW failed: {:?}", err));
        }
    }
    Ok(())
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            paint_strip(hwnd);
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            let x = (lparam.0 & 0xffff) as i16 as i32;
            handle_click(hwnd, x);
            LRESULT(0)
        }
        WM_DESTROY => {
            // CRITICAL: do NOT free the Box here. The manager's HashMap
            // owns the Box<Overlay>; this window's USERDATA holds a
            // *borrowed* pointer for the WndProc to dereference. Freeing
            // here would double-free when the manager subsequently drops
            // the Box. Just clear the pointer so any in-flight messages
            // see a null and bail out.
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

unsafe fn overlay_for(hwnd: HWND) -> Option<&'static mut Overlay> {
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Overlay;
    if ptr.is_null() { None } else { Some(&mut *ptr) }
}

unsafe fn paint_strip(hwnd: HWND) {
    // BeginPaint must be matched by EndPaint on every return path,
    // including when USERDATA is null (WM_DESTROY ran). Skipping EndPaint
    // leaks the paint DC and Windows eventually starves on internal GDI
    // handles. So pair the BeginPaint/EndPaint *first*, then bail out.
    let mut ps = PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);
    // Defensive: BeginPaint can return a null HDC under heavy paint storms
    // or device loss. Subsequent GDI calls on a null HDC are no-ops at
    // best, undefined at worst — and any GDI handle we'd create after
    // this point would leak before reaching the cleanup block.
    if hdc.is_invalid() {
        let _ = EndPaint(hwnd, &ps);
        return;
    }

    let Some(ov) = overlay_for(hwnd) else {
        let _ = EndPaint(hwnd, &ps);
        return;
    };

    let count = ov.descriptor.tabs.len() as i32;
    if count == 0 {
        let _ = EndPaint(hwnd, &ps);
        return;
    }

    // CRITICAL: paint based on the *full* client area, not `ps.rcPaint`.
    // rcPaint is just the dirty region; if Windows invalidates only one
    // tab and we lay out tabs across rcPaint's width, every tab crowds
    // into the dirty rect and the rest of the strip stays stale.
    let mut client = RECT::default();
    if GetClientRect(hwnd, &mut client).is_err() {
        let _ = EndPaint(hwnd, &ps);
        return;
    }
    let total_w = client.right - client.left;
    let total_h = client.bottom - client.top;

    // Defensive: a sub-threshold client rect (e.g. an existing overlay
    // resized by `SetWindowPos` below the suppression threshold) paints
    // nothing rather than rendering garbage. Matches the no-create policy
    // in `diff_and_apply`. Also guards against zero/negative dimensions
    // that would degenerate the GDI calls below.
    if total_w < MIN_STRIP_WIDTH || total_h <= 0 {
        let _ = EndPaint(hwnd, &ps);
        return;
    }
    let tab_w = (total_w / count).max(1);

    // Native Win11-style colors. BGR byte order, not RGB.
    let bg_inactive = CreateSolidBrush(COLORREF(0x00282828)); // dark gray
    let bg_active   = CreateSolidBrush(COLORREF(ACCENT_BGR)); // accent
    let separator   = CreateSolidBrush(COLORREF(0x00404040)); // mid gray
    let stripe_color = CreateSolidBrush(COLORREF(0x00FFFFFF)); // top stripe on active

    SetBkMode(hdc, TRANSPARENT);

    // Real font: Segoe UI 9pt is what Windows 11 uses for its own chrome.
    // Without this, GDI defaults to System (a chunky 90s pixel font).
    let font = create_strip_font();
    let prev_font = if !font.is_invalid() { SelectObject(hdc, font) } else { HGDIOBJ::default() };

    // Whether the close-X is shown depends on per-tab width. With many
    // tabs the per-tab width drops below `MIN_TAB_W_FOR_CLOSE` and the
    // X would overlap the title; hide it but keep titles readable.
    let show_close_x = tab_w >= MIN_TAB_W_FOR_CLOSE;

    for (i, (_, title)) in ov.descriptor.tabs.iter().enumerate() {
        let x0 = i as i32 * tab_w;
        let x1 = if i as i32 == count - 1 { total_w } else { x0 + tab_w };
        let active = i == ov.descriptor.active;
        let mut tab_rect = RECT { left: x0, top: 0, right: x1, bottom: total_h };
        let brush = if active { bg_active } else { bg_inactive };
        FillRect(hdc, &tab_rect, brush);

        // 1-px separator between tabs (skip after the last).
        if i as i32 != count - 1 {
            let sep = RECT { left: x1 - 1, top: 0, right: x1, bottom: total_h };
            FillRect(hdc, &sep, separator);
        }

        // Active tab gets a 2-px accent stripe along the bottom edge AND
        // a 2-px white stripe along the top edge. Framing both edges
        // makes "this is selected" land even at a glance; mirrors how
        // Win11's command-bar pills work but with stronger emphasis.
        if active {
            let bottom_stripe = RECT { left: x0, top: total_h - 2, right: x1, bottom: total_h };
            FillRect(hdc, &bottom_stripe, bg_active);
            let top_stripe = RECT { left: x0, top: 0, right: x1, bottom: 2 };
            FillRect(hdc, &top_stripe, stripe_color);
        }

        // Inset for text + ellipsis. Active text pure white; inactive
        // slightly dimmed for hierarchy. Right edge reserved for the X
        // *only* when we're actually drawing the X; otherwise we hand the
        // full tab width back to the title so DT_END_ELLIPSIS has room.
        SetTextColor(hdc, if active { COLORREF(0x00FFFFFF) } else { COLORREF(0x00BFBFBF) });
        let title_left = x0 + 10;
        let title_right = if show_close_x { x1 - CLOSE_BTN_W } else { x1 - 4 };

        // Skip drawing the title entirely if the tab is so narrow that the
        // text rect would be degenerate (left >= right). DrawTextW with a
        // zero/negative-width rect has been observed to crash on some GDI
        // versions; safer to draw nothing than risk the call.
        if title_left + 1 < title_right {
            tab_rect.left  = title_left;
            tab_rect.right = title_right;
            let mut wide: Vec<u16> = title.encode_utf16().collect();
            if wide.is_empty() { wide.push(0u16); }
            let _ = DrawTextW(
                hdc,
                &mut wide,
                &mut tab_rect,
                DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS,
            );
        }

        // Close button: a small X on the right side of every tab. The
        // glyph is drawn manually with two diagonal lines so we don't
        // depend on font glyph availability or bidi rendering quirks.
        // Suppressed on too-narrow tabs (see `MIN_TAB_W_FOR_CLOSE`).
        if show_close_x {
            draw_close_x(hdc, x1, total_h, active);
        }
    }

    if !prev_font.is_invalid() { SelectObject(hdc, prev_font); }
    if !font.is_invalid()      { let _ = DeleteObject(font); }
    let _ = DeleteObject(bg_inactive);
    let _ = DeleteObject(bg_active);
    let _ = DeleteObject(separator);
    let _ = DeleteObject(stripe_color);
    let _ = EndPaint(hwnd, &ps);
}

/// Paint a small X glyph centered in the close-button hit-zone of a tab
/// whose right edge is at `tab_right` and whose strip is `total_h` tall.
/// The X is drawn 8px square; the surrounding `CLOSE_BTN_W`px is
/// click-only padding to make the button forgiving on small tabs.
unsafe fn draw_close_x(hdc: windows::Win32::Graphics::Gdi::HDC, tab_right: i32, total_h: i32, active: bool) {
    const GLYPH: i32 = 8;
    let cx = tab_right - CLOSE_BTN_W / 2;
    let cy = total_h / 2;
    let half = GLYPH / 2;
    let color = if active { COLORREF(0x00FFFFFF) } else { COLORREF(0x00BFBFBF) };
    let pen = CreatePen(PS_SOLID, 1, color);
    let prev = SelectObject(hdc, pen);

    // Two diagonal lines forming an X.
    let _ = MoveToEx(hdc, cx - half, cy - half, None);
    let _ = LineTo(hdc, cx + half, cy + half);
    let _ = MoveToEx(hdc, cx + half, cy - half, None);
    let _ = LineTo(hdc, cx - half, cy + half);

    SelectObject(hdc, prev);
    let _ = DeleteObject(pen);
}

/// Create a Segoe UI 9pt font handle. Caller owns it (must `DeleteObject`).
/// 9pt at 96 DPI = 12 logical pixels; on the per-monitor-aware thread
/// Windows scales correctly for the actual monitor DPI.
unsafe fn create_strip_font() -> HFONT {
    // 9pt at 96 DPI = 9 * 96 / 72 = 12. Negate to specify character height
    // rather than cell height (matches DrawText sizing semantics better).
    let height = -12;
    let face: Vec<u16> = "Segoe UI\0".encode_utf16().collect();
    CreateFontW(
        height, 0, 0, 0,
        FW_NORMAL.0 as i32,
        0, 0, 0,
        DEFAULT_CHARSET.0 as u32,
        OUT_DEFAULT_PRECIS.0 as u32,
        CLIP_DEFAULT_PRECIS.0 as u32,
        DEFAULT_QUALITY.0 as u32,
        (VARIABLE_PITCH.0 | FF_DONTCARE.0) as u32,
        PCWSTR(face.as_ptr()),
    )
}

unsafe fn handle_click(hwnd: HWND, x: i32) {
    let Some(ov) = overlay_for(hwnd) else { return };
    let count = ov.descriptor.tabs.len() as i32;
    if count == 0 { return; }

    // Same arithmetic as paint_strip.
    let mut rect = RECT::default();
    if GetClientRect(hwnd, &mut rect).is_err() { return; }
    let total_w = rect.right - rect.left;
    let tab_w = (total_w / count).max(1);
    let idx = (x / tab_w).clamp(0, count - 1) as usize;

    let Some((id, _)) = ov.descriptor.tabs.get(idx).cloned() else { return };

    // Right-edge close button — last `CLOSE_BTN_W` pixels of the tab,
    // but only when the X is actually painted. On narrow tabs we hide
    // the X visually; routing a click to Close in that case would be
    // a phantom hit-zone the user can't see.
    let tab_right = if idx as i32 == count - 1 { total_w } else { ((idx as i32) + 1) * tab_w };
    let show_close_x = tab_w >= MIN_TAB_W_FOR_CLOSE;
    let action = if show_close_x && x >= tab_right - CLOSE_BTN_W {
        TabAction::Close(id)
    } else {
        TabAction::Activate(id)
    };

    if let Some(shared) = SHARED.lock().clone() {
        if let Err(e) = shared.click_tx.send(action) {
            debug!("tab strip click channel closed: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(cell: Rect, tabs: &[(u64, &str)], active: usize) -> StripDescriptor {
        StripDescriptor {
            cell,
            tabs: tabs.iter().map(|(id, t)| (WindowId(*id), (*t).to_string())).collect(),
            active,
        }
    }

    /// group_key depends only on the *set* of WindowIds, not on order or
    /// the active index. Layout repaints that don't change membership
    /// must produce a stable key so the overlay HWND is reused.
    #[test]
    fn group_key_is_stable_across_active_and_order_changes() {
        let cell = Rect::new(0, 0, 800, 600);
        let a = d(cell, &[(1, "a"), (2, "b")], 0);
        let b = d(cell, &[(2, "b"), (1, "a")], 1);
        assert_eq!(a.group_key(), b.group_key());
    }

    #[test]
    fn group_key_changes_when_membership_changes() {
        let cell = Rect::new(0, 0, 800, 600);
        let a = d(cell, &[(1, "a"), (2, "b")], 0);
        let b = d(cell, &[(1, "a"), (2, "b"), (3, "c")], 0);
        assert_ne!(a.group_key(), b.group_key());
    }
}
