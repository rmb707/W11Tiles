//! Process-wide DPI awareness setup.
//!
//! Without this, the OS silently virtualizes any SetWindowPos coordinates we
//! pass — fine on 100%-scale displays, broken on anything else. The fix is
//! to declare per-monitor-aware-v2 once at process start. Every production
//! Win32 tiling WM does this; failing to declare also breaks GetMonitorInfo
//! consistency on multi-DPI multi-monitor setups.

#![cfg(windows)]

use tracing::warn;
use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};

pub fn declare_per_monitor_aware() {
    unsafe {
        if let Err(e) = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) {
            warn!("SetProcessDpiAwarenessContext failed: {e} (placements may misalign on HiDPI)");
        }
    }
}
