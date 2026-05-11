//! Virtual-desktop awareness via `IVirtualDesktopManager`.
//!
//! Without this, our `SetWindowPos` calls run against windows on every
//! virtual desktop — including ones the user just switched away from —
//! which interrupts Windows' built-in desktop-switch animation and makes
//! `Ctrl+Win+Arrow` feel broken. The check is a single COM call, cheap.
//!
//! `IVirtualDesktopManager` is the *public* COM interface that's been
//! stable since Windows 10. (There's an undocumented internal interface
//! that lets you enumerate desktops and move windows between them — that's
//! what komorebi uses for full VD integration. We start with the public
//! one, which is enough to *avoid disturbing* the user's desktop layout.)

#![cfg(windows)]

use std::cell::RefCell;

use tracing::{debug, warn};
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
};
use windows::Win32::UI::Shell::{IVirtualDesktopManager, VirtualDesktopManager};

thread_local! {
    static VDM: RefCell<Option<IVirtualDesktopManager>> = const { RefCell::new(None) };
    static COM_READY: RefCell<bool> = const { RefCell::new(false) };
}

fn ensure_com() {
    COM_READY.with(|r| {
        if !*r.borrow() {
            unsafe {
                // S_FALSE if already initialized on this thread — both are fine.
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            }
            *r.borrow_mut() = true;
        }
    });
}

fn vdm() -> Option<IVirtualDesktopManager> {
    ensure_com();
    VDM.with(|cell| {
        if cell.borrow().is_none() {
            let new = unsafe {
                CoCreateInstance::<_, IVirtualDesktopManager>(&VirtualDesktopManager, None, CLSCTX_INPROC_SERVER)
            };
            match new {
                Ok(inst) => *cell.borrow_mut() = Some(inst),
                Err(e) => {
                    warn!("CoCreateInstance(VirtualDesktopManager) failed: {e}");
                    return None;
                }
            }
        }
        cell.borrow().clone()
    })
}

/// Returns `true` if the window is on the user's currently-visible virtual
/// desktop. On any error or COM unavailability, returns `true` — we'd
/// rather mis-tile a window than refuse to tile it.
pub fn is_on_current_desktop(hwnd: HWND) -> bool {
    let Some(vdm) = vdm() else { return true };
    unsafe {
        match vdm.IsWindowOnCurrentVirtualDesktop(hwnd) {
            Ok(b) => b.as_bool(),
            Err(e) => {
                debug!("IsWindowOnCurrentVirtualDesktop failed: {e} (treating as current)");
                true
            }
        }
    }
}

/// Stable identifier for a Windows virtual desktop. The public
/// `IVirtualDesktopManager` interface only lets us *query* per-window VD
/// GUIDs — there's no public enumeration of all VDs. So we discover them
/// lazily as we see windows on them. The 16-byte payload is the GUID.
pub type VdKey = [u8; 16];

/// Get the GUID of the virtual desktop a given window lives on. Returns
/// `None` if the call fails — caller treats that as "unknown / current."
pub fn window_desktop_key(hwnd: HWND) -> Option<VdKey> {
    let vdm = vdm()?;
    unsafe {
        match vdm.GetWindowDesktopId(hwnd) {
            Ok(g) => Some(guid_to_bytes(g)),
            Err(e) => {
                debug!("GetWindowDesktopId failed: {e}");
                None
            }
        }
    }
}

/// Best-effort lookup of the *currently visible* virtual desktop's GUID.
/// We don't have a direct API for this in the public interface, so we
/// query the foreground window's VD. It's an oblique signal but reliable
/// in practice — the foreground window is, by definition, on the visible
/// desktop.
pub fn current_desktop_key() -> Option<VdKey> {
    use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
    unsafe {
        let fg = GetForegroundWindow();
        if fg.is_invalid() { return None; }
        window_desktop_key(fg)
    }
}

fn guid_to_bytes(g: windows::core::GUID) -> VdKey {
    // GUID layout: u32 + u16 + u16 + [u8; 8]. Pack to little-endian bytes
    // so the bytewise representation is stable regardless of struct layout.
    let mut out = [0u8; 16];
    out[0..4].copy_from_slice(&g.data1.to_le_bytes());
    out[4..6].copy_from_slice(&g.data2.to_le_bytes());
    out[6..8].copy_from_slice(&g.data3.to_le_bytes());
    out[8..16].copy_from_slice(&g.data4);
    out
}
