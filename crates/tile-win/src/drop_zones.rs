//! Drop-zone overlay windows.
//!
//! Painted on top of every tile (except the dragged source) while the
//! user is mid-drag. Shows a colored border + a "TAB GROUP" label so the
//! user can *see* where dropping will create a tab vs. fall back to
//! float/split — Windows' built-in Aero Snap gives no signal that we're
//! a tiling WM, so we paint our own.
//!
//! Architecture mirrors `tab_strip.rs`: one worker thread owns every
//! overlay window, the daemon pushes `Vec<DropZone>` updates over a
//! `Mutex<Option<...>>` + `PostThreadMessageW` wake-up, and the worker
//! diffs and applies. `show()` replaces the current set; `hide()` clears.

#![cfg(windows)]

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use parking_lot::Mutex;
use tile_core::Rect;
use tracing::warn;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontW, CreateSolidBrush, DeleteObject, DrawTextW, EndPaint, FillRect,
    FrameRect, InvalidateRect, SelectObject, SetBkMode, SetTextColor, CLIP_DEFAULT_PRECIS,
    DEFAULT_CHARSET, DEFAULT_QUALITY, DT_CENTER, DT_SINGLELINE, DT_VCENTER, FF_DONTCARE, FW_BOLD,
    HBRUSH, HFONT, HGDIOBJ, OUT_DEFAULT_PRECIS, PAINTSTRUCT, TRANSPARENT, VARIABLE_PITCH,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect,
    GetMessageW, GetWindowLongPtrW, PostThreadMessageW, RegisterClassExW,
    SetLayeredWindowAttributes, SetWindowLongPtrW, SetWindowPos, ShowWindow, TranslateMessage,
    GWLP_USERDATA, HCURSOR, HICON, HWND_TOPMOST, LWA_ALPHA, MSG, SW_SHOWNOACTIVATE,
    SWP_NOACTIVATE, SWP_NOZORDER, WM_DESTROY, WM_PAINT, WM_QUIT, WM_USER, WNDCLASSEXW,
    WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT,
    WS_POPUP,
};

const WM_DROPZONE_UPDATE: u32 = WM_USER + 1;

/// Tile dimensions below which the drop-zone overlay is suppressed entirely.
/// On a tile that small the 5 zones each shrink to ~50px or less and the
/// arrow glyphs become illegible noise — the user can't read which region
/// means what, so painting nothing is strictly better. Drag-to-edge still
/// works on suppressed tiles via the daemon's geometric hit-test; the user
/// just doesn't get a *visual* preview of where the drop will land.
const MIN_ZONE_W: i32 = 200;
const MIN_ZONE_H: i32 = 120;

/// Layered-window alpha for drop zones. Lower than the original 80
/// (which hid most of the underlying tile content during drag) — at ~26%
/// the user can still see what's underneath while the colored regions
/// remain readable. The contrast between muted-edge and bright-center
/// fills survives the lower alpha because they differ in *value* not
/// just hue.
const ZONE_ALPHA: u8 = 66;

/// Edge fill (muted accent). BGR. Paired with `CENTER_FILL` to make the
/// "split here" vs. "tab here" distinction read at a glance.
const EDGE_FILL: u32 = 0x00803319;

/// Center fill (bright accent). BGR. Slightly darkened from the original
/// `0x00C56A19` to match the tab strip's adjusted accent and improve
/// white-on-color text contrast.
const CENTER_FILL: u32 = 0x00B05010;

/// "Hot" fill — region the cursor is currently over. Visibly brighter
/// than CENTER_FILL so the user sees *exactly* which sub-zone their
/// drop will land in. The pulse against the muted backdrop is the whole
/// point: drop targeting is no longer guesswork.
const HOT_FILL: u32 = 0x0040E0FF;

/// Color used for the inter-region separators *and* the outer frame.
/// White at 1px reads as a clean dividing line between regions, helping
/// the 5 zones register as discrete drop targets.
const SEPARATOR_BGR: u32 = 0x00FFFFFF;

const CLASS_NAME_W: &[u16] = &[
    b'T' as u16, b'i' as u16, b'l' as u16, b'e' as u16, b'M' as u16, b'a' as u16, b'n' as u16,
    b'a' as u16, b'g' as u16, b'e' as u16, b'r' as u16, b'D' as u16, b'r' as u16, b'o' as u16,
    b'p' as u16, b'Z' as u16, b'o' as u16, b'n' as u16, b'e' as u16, 0,
];

/// Which sub-region of a tile the cursor is currently over. The matching
/// region in the rendered overlay paints in the bright "hot" fill color
/// so the user can see where their drop will land *before* they release.
/// `None` means cursor is somewhere else (a different tile, blank space,
/// or no drag in progress yet).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotZone { None, Center, Top, Bottom, Left, Right }

#[derive(Debug, Clone)]
pub struct DropZone {
    /// Full tile rect in physical (per-monitor-DPI-aware) screen coords.
    pub rect: Rect,
    /// Which of the five sub-regions to render brightened. Updated on
    /// every cursor-tick during a drag so the highlight follows the
    /// mouse in real time.
    pub hot: HotZone,
}

pub struct DropZoneManager {
    thread: Option<JoinHandle<()>>,
    thread_id: u32,
}

struct Shared {
    pending: Mutex<Option<Vec<DropZone>>>,
}

static SHARED: Mutex<Option<Arc<Shared>>> = Mutex::new(None);

impl DropZoneManager {
    pub fn start() -> Self {
        let shared = Arc::new(Shared { pending: Mutex::new(None) });
        *SHARED.lock() = Some(shared);

        let (tid_tx, tid_rx) = std::sync::mpsc::channel();
        let thread = thread::spawn(move || {
            unsafe { let _ = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2); }
            let tid = unsafe { windows::Win32::System::Threading::GetCurrentThreadId() };
            let _ = tid_tx.send(tid);
            if let Err(e) = register_class() {
                warn!("drop_zones: register_class failed: {e}");
                return;
            }
            run_pump();
            *SHARED.lock() = None;
        });

        let thread_id = tid_rx.recv().unwrap_or(0);
        Self { thread: Some(thread), thread_id }
    }

    /// Replace the current set of zones. Pass an empty vec to clear them.
    pub fn show(&self, zones: Vec<DropZone>) {
        if let Some(shared) = SHARED.lock().clone() {
            *shared.pending.lock() = Some(zones);
            if self.thread_id != 0 {
                unsafe {
                    let _ = PostThreadMessageW(
                        self.thread_id, WM_DROPZONE_UPDATE, WPARAM(0), LPARAM(0),
                    );
                }
            }
        }
    }

    pub fn hide(&self) { self.show(Vec::new()); }

    pub fn stop(mut self) {
        if self.thread_id != 0 {
            unsafe {
                let _ = PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
            }
        }
        if let Some(t) = self.thread.take() { let _ = t.join(); }
    }
}

struct Overlay {
    hwnd: HWND,
    /// Last hot-zone state we painted. Used to skip redundant repaints
    /// when the cursor poll fires but nothing changed.
    hot: HotZone,
}

unsafe impl Send for Overlay {}
unsafe impl Sync for Overlay {}

fn run_pump() {
    let mut overlays: HashMap<u64, Box<Overlay>> = HashMap::new();
    let mut msg = MSG::default();
    unsafe {
        loop {
            let r = GetMessageW(&mut msg, HWND::default(), 0, 0);
            if !r.as_bool() { break; }
            if msg.hwnd.is_invalid() && msg.message == WM_DROPZONE_UPDATE {
                if let Some(shared) = SHARED.lock().clone() {
                    if let Some(zones) = shared.pending.lock().take() {
                        diff_and_apply(&mut overlays, zones);
                    }
                }
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        for (_, ov) in overlays.drain() {
            let _ = DestroyWindow(ov.hwnd);
        }
    }
}

fn rect_key(r: &Rect) -> u64 {
    // Identity = (x, y, w, h) packed. Same rect across calls reuses the
    // same overlay window; rect changes destroy + recreate.
    let mut h: u64 = 0xcbf29ce484222325;
    for v in [r.x, r.y, r.width, r.height] {
        for byte in v.to_le_bytes() {
            h ^= byte as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
    }
    h
}

fn diff_and_apply(overlays: &mut HashMap<u64, Box<Overlay>>, zones: Vec<DropZone>) {
    // Drop zones below the legibility threshold are treated as if they
    // were never sent — any pre-existing overlay for them gets destroyed
    // by the `to_drop` pass below.
    let zones: Vec<DropZone> = zones
        .into_iter()
        .filter(|z| z.rect.width >= MIN_ZONE_W && z.rect.height >= MIN_ZONE_H)
        .collect();

    let incoming: HashSet<u64> = zones.iter().map(|z| rect_key(&z.rect)).collect();
    let to_drop: Vec<u64> = overlays.keys().copied().filter(|k| !incoming.contains(k)).collect();
    for key in to_drop {
        if let Some(ov) = overlays.remove(&key) {
            unsafe { let _ = DestroyWindow(ov.hwnd); }
        }
    }
    for z in zones {
        let key = rect_key(&z.rect);
        if let Some(existing) = overlays.get_mut(&key) {
            // Same tile, just maybe a different hot zone. Update + invalidate.
            if existing.hot != z.hot {
                existing.hot = z.hot;
                unsafe { let _ = InvalidateRect(existing.hwnd, None, true); }
            }
        } else if let Some(ov) = create_overlay(z) {
            overlays.insert(key, ov);
        }
    }
}

fn create_overlay(zone: DropZone) -> Option<Box<Overlay>> {
    let h_module = unsafe { GetModuleHandleW(None).ok()? };
    let r = zone.rect;
    let hwnd = unsafe {
        CreateWindowExW(
            // Click-through (TRANSPARENT) so clicks during drag don't get
            // eaten by us — important since drag is in progress and the
            // OS is still routing pointer events.
            WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_NOACTIVATE
                | WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            PCWSTR(CLASS_NAME_W.as_ptr()),
            PCWSTR(CLASS_NAME_W.as_ptr()),
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
        Ok(_) => { warn!("CreateWindowExW returned invalid HWND for drop zone"); return None; }
        Err(e) => { warn!("CreateWindowExW failed for drop zone: {e}"); return None; }
    };
    unsafe {
        // See `ZONE_ALPHA` — ~26%. Edge/center fills differ enough in
        // value that the contrast survives the lower opacity, while
        // underlying tile content stays visible during drag.
        let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), ZONE_ALPHA, LWA_ALPHA);
    }
    let mut overlay = Box::new(Overlay { hwnd, hot: zone.hot });
    let ptr = &mut *overlay as *mut Overlay;
    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, ptr as isize);
        let _ = SetWindowPos(hwnd, HWND_TOPMOST, r.x, r.y, r.width, r.height,
                             SWP_NOACTIVATE | SWP_NOZORDER);
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
        hbrBackground: HBRUSH::default(),
        lpszMenuName: PCWSTR::null(),
        lpszClassName: PCWSTR(CLASS_NAME_W.as_ptr()),
        hIconSm: HICON::default(),
    };
    let atom = unsafe { RegisterClassExW(&wc) };
    if atom == 0 {
        let err = unsafe { windows::Win32::Foundation::GetLastError() };
        if err.0 != 1410 { // ERROR_CLASS_ALREADY_EXISTS
            return Err(format!("RegisterClassExW failed: {:?}", err));
        }
    }
    Ok(())
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => { paint_zone(hwnd); LRESULT(0) }
        WM_DESTROY => {
            // Same crash-fix as tab_strip: clear the borrowed pointer
            // but DON'T free; the manager's Box is the owner.
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

unsafe fn paint_zone(hwnd: HWND) {
    // Pair BeginPaint/EndPaint on every return path. If WM_PAINT lands
    // after WM_DESTROY cleared USERDATA, we still owe Windows an
    // EndPaint — skipping it leaks the paint DC.
    let mut ps = PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);
    // Null HDC = device-loss / paint-storm; bail before allocating GDI
    // objects that would leak.
    if hdc.is_invalid() {
        let _ = EndPaint(hwnd, &ps);
        return;
    }

    let hot = match overlay_for(hwnd) {
        Some(ov) => ov.hot,
        None => {
            let _ = EndPaint(hwnd, &ps);
            return;
        }
    };

    let mut client = RECT::default();
    if GetClientRect(hwnd, &mut client).is_err() {
        let _ = EndPaint(hwnd, &ps);
        return;
    }

    let total_w = client.right - client.left;
    let total_h = client.bottom - client.top;

    // Defensive: if an existing overlay gets resized below the legibility
    // threshold (or to a degenerate rect), paint nothing rather than
    // squeezing five labelled regions into a too-small space and producing
    // pixel noise. Mirrors the no-create policy in `diff_and_apply`.
    if total_w < MIN_ZONE_W || total_h < MIN_ZONE_H {
        let _ = EndPaint(hwnd, &ps);
        return;
    }

    let third_w = total_w / 3;
    let third_h = total_h / 3;

    // Five-region split. The 3×3 grid divides the tile into:
    //   [LEFT] [TOP   ] [RIGHT]
    //   [LEFT] [CENTER] [RIGHT]    -- LEFT/RIGHT span full height
    //   [LEFT] [BOTTOM] [RIGHT]
    // Center is the tab-merge zone; edges are split-and-tile zones.
    // The two distinct fills make the choice visible at a glance.
    let edge_fill   = CreateSolidBrush(COLORREF(EDGE_FILL));
    let center_fill = CreateSolidBrush(COLORREF(CENTER_FILL));
    let hot_fill    = CreateSolidBrush(COLORREF(HOT_FILL));
    let separator   = CreateSolidBrush(COLORREF(SEPARATOR_BGR));

    let left_rect   = RECT { left: 0,             top: 0,            right: third_w,           bottom: total_h };
    let right_rect  = RECT { left: total_w - third_w, top: 0,         right: total_w,          bottom: total_h };
    let top_rect    = RECT { left: third_w,       top: 0,            right: total_w - third_w, bottom: third_h };
    let bottom_rect = RECT { left: third_w,       top: total_h - third_h, right: total_w - third_w, bottom: total_h };
    let center_rect = RECT { left: third_w,       top: third_h,      right: total_w - third_w, bottom: total_h - third_h };

    // Pick each region's fill: the cursor-active region gets the hot
    // brush, others get their idle brush.
    let pick = |zone: HotZone, idle: HBRUSH| -> HBRUSH {
        if hot == zone { hot_fill } else { idle }
    };
    FillRect(hdc, &left_rect,   pick(HotZone::Left,   edge_fill));
    FillRect(hdc, &right_rect,  pick(HotZone::Right,  edge_fill));
    FillRect(hdc, &top_rect,    pick(HotZone::Top,    edge_fill));
    FillRect(hdc, &bottom_rect, pick(HotZone::Bottom, edge_fill));
    FillRect(hdc, &center_rect, pick(HotZone::Center, center_fill));

    // Outer frame and the center frame are existing landmarks; on top of
    // them we draw 1-px lines along *every* boundary between regions so
    // the five zones read as discrete drop targets rather than a vague
    // colored mass. Each boundary is rendered as a 1px-tall/wide
    // FillRect, which is cheaper and pixel-aligned compared to drawing
    // individual lines.
    FrameRect(hdc, &center_rect, separator);
    FrameRect(hdc, &client,      separator);

    // Vertical seams: left/center and center/right.
    let seam_left  = RECT { left: third_w - 1,            top: 0, right: third_w,            bottom: total_h };
    let seam_right = RECT { left: total_w - third_w,      top: 0, right: total_w - third_w + 1, bottom: total_h };
    FillRect(hdc, &seam_left,  separator);
    FillRect(hdc, &seam_right, separator);

    // Horizontal seams within the center column: top/center and
    // center/bottom. (The full-width seams across the tile would cut
    // through the LEFT and RIGHT zones, which we don't want; these
    // stay scoped to the center column where TOP/CENTER/BOTTOM stack.)
    let seam_top    = RECT { left: third_w, top: third_h - 1,            right: total_w - third_w, bottom: third_h };
    let seam_bottom = RECT { left: third_w, top: total_h - third_h,      right: total_w - third_w, bottom: total_h - third_h + 1 };
    FillRect(hdc, &seam_top,    separator);
    FillRect(hdc, &seam_bottom, separator);

    SetBkMode(hdc, TRANSPARENT);
    SetTextColor(hdc, COLORREF(0x00FFFFFF));

    // Two fonts:
    //   * `arrow_font` for the big directional glyphs (top half of each
    //     edge zone) — instantly readable from a distance.
    //   * `label_font` for the descriptor word ("ABOVE" / "BELOW" / etc.)
    //     in the bottom half — turns "the up arrow zone" into "drop here
    //     to put this window above the target", which is what users
    //     actually need to know.
    // Center is rendered with `tab_font` (in between the two — bold and
    // big, since it's the headline action).
    let label_font = create_label_font();
    let arrow_font = create_arrow_font();
    let tab_font   = create_tab_font();
    let prev_font  = if !arrow_font.is_invalid() {
        SelectObject(hdc, arrow_font)
    } else {
        HGDIOBJ::default()
    };

    // Helper to split a zone rect into top-half (arrow) and bottom-half
    // (label) so the two render lines don't overlap.
    fn halves(r: RECT) -> (RECT, RECT) {
        let mid = (r.top + r.bottom) / 2;
        let top    = RECT { left: r.left, top: r.top, right: r.right, bottom: mid };
        let bottom = RECT { left: r.left, top: mid,  right: r.right, bottom: r.bottom };
        (top, bottom)
    }

    // Edge zones: arrow on top, descriptor below.
    let edges = [
        (top_rect,    "↑", "ABOVE"),
        (bottom_rect, "↓", "BELOW"),
        (left_rect,   "←", "LEFT"),
        (right_rect,  "→", "RIGHT"),
    ];
    for (rect, arrow, word) in edges {
        let (mut arrow_r, mut label_r) = halves(rect);
        // Arrow first, in the larger arrow_font we already selected.
        let mut wide_arrow: Vec<u16> = arrow.encode_utf16().collect();
        let _ = DrawTextW(hdc, &mut wide_arrow, &mut arrow_r,
                          DT_SINGLELINE | DT_VCENTER | DT_CENTER);
        // Switch to label_font for the descriptor.
        if !label_font.is_invalid() { SelectObject(hdc, label_font); }
        let mut wide_word: Vec<u16> = word.encode_utf16().collect();
        let _ = DrawTextW(hdc, &mut wide_word, &mut label_r,
                          DT_SINGLELINE | DT_VCENTER | DT_CENTER);
        // Back to arrow_font for the next iteration's arrow.
        if !arrow_font.is_invalid() { SelectObject(hdc, arrow_font); }
    }

    // Center: "TAB" — biggest text, single-line, no arrow.
    if !tab_font.is_invalid() { SelectObject(hdc, tab_font); }
    let mut tab_rect = center_rect;
    let mut wide_center: Vec<u16> = "TAB".encode_utf16().collect();
    let _ = DrawTextW(hdc, &mut wide_center, &mut tab_rect,
                      DT_SINGLELINE | DT_VCENTER | DT_CENTER);

    if !prev_font.is_invalid() { SelectObject(hdc, prev_font); }
    if !arrow_font.is_invalid() { let _ = DeleteObject(arrow_font); }
    if !label_font.is_invalid() { let _ = DeleteObject(label_font); }
    if !tab_font.is_invalid()   { let _ = DeleteObject(tab_font); }
    let _ = DeleteObject(edge_fill);
    let _ = DeleteObject(center_fill);
    let _ = DeleteObject(hot_fill);
    let _ = DeleteObject(separator);
    let _ = EndPaint(hwnd, &ps);
}

/// Recover the borrowed Overlay pointer stuffed in this window's
/// USERDATA. Returns `None` when the pointer is null — typically because
/// WM_DESTROY ran before this paint was processed. Mirrors the helper in
/// `tab_strip.rs`.
unsafe fn overlay_for(hwnd: HWND) -> Option<&'static mut Overlay> {
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Overlay;
    if ptr.is_null() { None } else { Some(&mut *ptr) }
}

/// Smaller secondary font for the descriptor word ("ABOVE", "BELOW",
/// "LEFT", "RIGHT") under each arrow. 11pt: just enough to read but
/// dominated by the arrow above it.
unsafe fn create_label_font() -> HFONT {
    // 11pt at 96 DPI ≈ 15px.
    let height = -15;
    let face: Vec<u16> = "Segoe UI\0".encode_utf16().collect();
    CreateFontW(
        height, 0, 0, 0,
        FW_BOLD.0 as i32,
        0, 0, 0,
        DEFAULT_CHARSET.0 as u32,
        OUT_DEFAULT_PRECIS.0 as u32,
        CLIP_DEFAULT_PRECIS.0 as u32,
        DEFAULT_QUALITY.0 as u32,
        (VARIABLE_PITCH.0 | FF_DONTCARE.0) as u32,
        PCWSTR(face.as_ptr()),
    )
}

/// Big font for the directional arrow glyphs on edge zones — they
/// need to read at a glance from across a 4K monitor.
unsafe fn create_arrow_font() -> HFONT {
    let height = -32;
    let face: Vec<u16> = "Segoe UI\0".encode_utf16().collect();
    CreateFontW(
        height, 0, 0, 0,
        FW_BOLD.0 as i32,
        0, 0, 0,
        DEFAULT_CHARSET.0 as u32,
        OUT_DEFAULT_PRECIS.0 as u32,
        CLIP_DEFAULT_PRECIS.0 as u32,
        DEFAULT_QUALITY.0 as u32,
        (VARIABLE_PITCH.0 | FF_DONTCARE.0) as u32,
        PCWSTR(face.as_ptr()),
    )
}

/// Center "TAB" headline font — heaviest emphasis since it's the
/// alternative-to-tile action and we want it to read as the primary
/// drop target when the user lands in the middle.
unsafe fn create_tab_font() -> HFONT {
    let height = -28;
    let face: Vec<u16> = "Segoe UI\0".encode_utf16().collect();
    CreateFontW(
        height, 0, 0, 0,
        FW_BOLD.0 as i32,
        0, 0, 0,
        DEFAULT_CHARSET.0 as u32,
        OUT_DEFAULT_PRECIS.0 as u32,
        CLIP_DEFAULT_PRECIS.0 as u32,
        DEFAULT_QUALITY.0 as u32,
        (VARIABLE_PITCH.0 | FF_DONTCARE.0) as u32,
        PCWSTR(face.as_ptr()),
    )
}
