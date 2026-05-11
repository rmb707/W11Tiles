//! Multi-monitor enumeration via `EnumDisplayMonitors`.
//!
//! We model each physical monitor as a `tile_core::Monitor` with its
//! work area (full bounds minus taskbar/AppBars) — that's what the layout
//! engine wants. Per-monitor DPI is read via `GetDpiForMonitor` so HiDPI
//! mixed setups don't end up half-tiled.
//!
//! `enumerate()` populates a static `HMONITOR → MonitorId` map so the
//! daemon can route a window to the right `MonitorId` via
//! `monitor_id_for_window()`. Without this the daemon hardcoded
//! `MonitorId(1)` and crowded every window onto the primary display.

#![cfg(windows)]

use parking_lot::RwLock;
use tile_core::geom::Rect;
use tile_core::state::{Monitor, MonitorId};
use tile_core::workspace::Workspace;

use windows::Win32::Foundation::{BOOL, HMODULE, HWND, LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, MonitorFromWindow, HDC, HMONITOR,
    MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};

/// HMONITOR → MonitorId. We keep raw `isize` instead of `HMONITOR` because
/// the windows-rs HMONITOR isn't `Send`/`Sync` by default (it wraps a raw
/// pointer). isize is fine since HMONITOR is fundamentally a handle value.
static HMONITOR_INDEX: RwLock<Vec<(isize, MonitorId)>> = RwLock::new(Vec::new());

pub fn enumerate(outer_gap: i32, inner_gap: i32, workspaces_per_monitor: u16) -> Vec<Monitor> {
    let mut monitors: Vec<Monitor> = Vec::new();
    extern "system" fn proc(h: HMONITOR, _hdc: HDC, _r: *mut RECT, lparam: LPARAM) -> BOOL {
        let out = unsafe { &mut *(lparam.0 as *mut Vec<HMONITOR>) };
        out.push(h);
        BOOL(1)
    }
    let mut handles: Vec<HMONITOR> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(
            HDC::default(),
            None,
            Some(proc),
            LPARAM(&mut handles as *mut _ as isize),
        );
    }

    let mut index: Vec<(isize, MonitorId)> = Vec::new();
    for (idx, h) in handles.into_iter().enumerate() {
        let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        unsafe {
            if !GetMonitorInfoW(h, &mut info).as_bool() { continue; }
        }
        let bounds    = rect_of(info.rcMonitor);
        let work_area = rect_of(info.rcWork);

        let mut dpi_x: u32 = 96;
        let mut dpi_y: u32 = 96;
        unsafe { let _ = GetDpiForMonitor(h, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y); }

        let id = MonitorId(idx as u32 + 1);
        index.push((h.0 as isize, id));
        monitors.push(Monitor {
            id,
            bounds,
            work_area,
            dpi: dpi_x,
            workspaces: (1..=workspaces_per_monitor)
                .map(|i| Workspace::new(tile_core::workspace::WorkspaceId(i), outer_gap, inner_gap))
                .collect(),
            active_workspace: tile_core::workspace::WorkspaceId(1),
        });
    }
    *HMONITOR_INDEX.write() = index;
    let _ = HMODULE::default();
    monitors
}

/// Find which monitor a given window belongs to. Falls back to
/// `MonitorId(1)` (the primary, by enumeration order) when the lookup
/// fails — better than refusing to tile.
pub fn monitor_id_for_window(hwnd: HWND) -> MonitorId {
    let h_monitor = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
    let raw = h_monitor.0 as isize;
    HMONITOR_INDEX
        .read()
        .iter()
        .find_map(|(h, id)| if *h == raw { Some(*id) } else { None })
        .unwrap_or(MonitorId(1))
}

fn rect_of(r: RECT) -> Rect {
    Rect::new(r.left, r.top, r.right - r.left, r.bottom - r.top)
}
