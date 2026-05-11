//! Runtime-generated app icon.
//!
//! Renders a 32×32 "TM" monogram into an `HICON` via GDI. Used for the
//! system-tray icon. Building a real `.ico` asset would mean a designed
//! multi-resolution file plus a build-time resource compiler in the
//! cargo pipeline; instead we paint into a DIB section at process
//! start and hand the bitmap to `CreateIconIndirect`. Cheap (~ms),
//! self-contained, no external assets to ship.
//!
//! Color palette matches the rest of the chrome: accent BGR
//! `0x00B05010` (same as the active-tab fill in `tab_strip`), white
//! foreground. The monogram is `TM` in Segoe UI Black, ~18pt scaled
//! to the icon size. At 16×16 (small tray) the letters still read.
//!
//! ## Why not a .ico file
//! Adding an icon resource to the EXE requires `build.rs` + a resource
//! compiler (`windres` for our MinGW target, `rc.exe` for MSVC), plus
//! a real designed multi-resolution `.ico` asset. We can ship one later
//! when there's a real design; this runtime renderer is the "good
//! enough at v1" path. Same approach works for the EXE icon once we
//! add the resource step — `create_app_icon` is reusable.

#![cfg(windows)]

use windows::core::PCWSTR;
use windows::Win32::Foundation::{BOOL, COLORREF, HWND, RECT};
use windows::Win32::Graphics::Gdi::{
    CreateBitmap, CreateCompatibleDC, CreateDIBSection, CreateFontW, CreateSolidBrush,
    DeleteDC, DeleteObject, DrawTextW, FillRect, GetDC, ReleaseDC, SelectObject, SetBkMode,
    SetTextColor, BITMAPV5HEADER, BI_BITFIELDS, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET,
    DEFAULT_QUALITY, DIB_RGB_COLORS, DT_CENTER, DT_NOCLIP, DT_SINGLELINE, DT_VCENTER,
    FF_DONTCARE, FW_BLACK, HGDIOBJ, OUT_DEFAULT_PRECIS, TRANSPARENT, VARIABLE_PITCH,
};
use windows::Win32::UI::WindowsAndMessaging::{CreateIconIndirect, HICON, ICONINFO};

/// Accent BGR — must match `tab_strip::ACCENT_BGR` so the tray icon
/// reads as the same brand-color square as the rest of our chrome.
const ICON_BG_BGR: u32 = 0x00B05010;

/// Build a 32×32 ARGB icon containing a centered white "TM" on an
/// accent-color square. Caller owns the returned `HICON` — call
/// `DestroyIcon` on shutdown if you want to be tidy. (`Shell_NotifyIcon`
/// keeps a reference internally; freeing while the tray uses it is
/// a use-after-free, so most code just leaks the icon for process
/// lifetime.)
pub fn create_app_icon() -> Option<HICON> {
    const SIZE: i32 = 32;
    unsafe {
        let screen_dc = GetDC(HWND::default());
        if screen_dc.is_invalid() { return None; }

        let bmi = BITMAPV5HEADER {
            bV5Size:        std::mem::size_of::<BITMAPV5HEADER>() as u32,
            bV5Width:       SIZE,
            bV5Height:      -SIZE, // negative = top-down DIB
            bV5Planes:      1,
            bV5BitCount:    32,
            bV5Compression: BI_BITFIELDS,
            bV5RedMask:     0x00FF0000,
            bV5GreenMask:   0x0000FF00,
            bV5BlueMask:    0x000000FF,
            bV5AlphaMask:   0xFF000000,
            ..Default::default()
        };

        let mut bits_ptr: *mut std::ffi::c_void = std::ptr::null_mut();
        let color_bmp = CreateDIBSection(
            screen_dc,
            &bmi as *const _ as *const _,
            DIB_RGB_COLORS,
            &mut bits_ptr,
            None,
            0,
        );
        let color_bmp = match color_bmp {
            Ok(b) if !b.is_invalid() => b,
            _ => {
                ReleaseDC(HWND::default(), screen_dc);
                return None;
            }
        };

        let mem_dc = CreateCompatibleDC(screen_dc);
        if mem_dc.is_invalid() {
            let _ = DeleteObject(color_bmp);
            ReleaseDC(HWND::default(), screen_dc);
            return None;
        }
        let prev_bmp = SelectObject(mem_dc, color_bmp);

        // Fill the whole icon with the accent color. The DIB came back
        // zero-initialized (alpha = 0 everywhere) so we need to paint
        // every pixel — otherwise un-touched pixels stay transparent and
        // the icon will look bitten-out around the corners. FillRect
        // sets RGB but leaves alpha at the BMP's mask; we backfill
        // alpha=255 manually after drawing.
        let bg_brush = CreateSolidBrush(COLORREF(ICON_BG_BGR));
        let rect = RECT { left: 0, top: 0, right: SIZE, bottom: SIZE };
        FillRect(mem_dc, &rect, bg_brush);
        let _ = DeleteObject(bg_brush);

        // Draw the "TM" monogram. Segoe UI Black at -22 pixel height —
        // big enough that the two letters fill most of the icon, small
        // enough that there's breathing room. Tracking is left to the
        // font's natural metrics; bold-black weight keeps the letters
        // readable at 16×16 if Windows scales us down for the small tray.
        SetBkMode(mem_dc, TRANSPARENT);
        SetTextColor(mem_dc, COLORREF(0x00FFFFFF));

        let face: Vec<u16> = "Segoe UI\0".encode_utf16().collect();
        let font = CreateFontW(
            -22, 0, 0, 0,
            FW_BLACK.0 as i32,
            0, 0, 0,
            DEFAULT_CHARSET.0 as u32,
            OUT_DEFAULT_PRECIS.0 as u32,
            CLIP_DEFAULT_PRECIS.0 as u32,
            DEFAULT_QUALITY.0 as u32,
            (VARIABLE_PITCH.0 | FF_DONTCARE.0) as u32,
            PCWSTR(face.as_ptr()),
        );
        let prev_font = if !font.is_invalid() { SelectObject(mem_dc, font) } else { HGDIOBJ::default() };

        let mut text: Vec<u16> = "TM".encode_utf16().collect();
        let mut text_rect = rect;
        let _ = DrawTextW(
            mem_dc,
            &mut text,
            &mut text_rect,
            DT_SINGLELINE | DT_VCENTER | DT_CENTER | DT_NOCLIP,
        );

        // GDI text rendering writes RGB but leaves alpha untouched on the
        // glyph pixels. Walk the bitmap and set alpha=255 everywhere so
        // the icon renders opaque against the taskbar — without this, the
        // accent square would be invisible (alpha=0) and only the AA-edge
        // pixels of the letters would be visible.
        if !bits_ptr.is_null() {
            let pixels = bits_ptr as *mut u32;
            let count = (SIZE * SIZE) as usize;
            for i in 0..count {
                *pixels.add(i) |= 0xFF000000;
            }
        }

        // Mask bitmap. With a 32-bit ARGB color bitmap, Windows uses the
        // alpha channel and ignores the mask — but `ICONINFO` still
        // requires a valid mask handle. Use a 1×1 black bitmap.
        let mask_bmp = CreateBitmap(SIZE, SIZE, 1, 1, None);

        let icon_info = ICONINFO {
            fIcon:    BOOL(1),
            xHotspot: 0,
            yHotspot: 0,
            hbmMask:  mask_bmp,
            hbmColor: color_bmp,
        };
        let hicon = CreateIconIndirect(&icon_info);

        // Restore + cleanup. The DIB section and font outlive selection.
        if !prev_font.is_invalid() { SelectObject(mem_dc, prev_font); }
        SelectObject(mem_dc, prev_bmp);
        if !font.is_invalid()  { let _ = DeleteObject(font); }
        let _ = DeleteObject(mask_bmp);
        let _ = DeleteObject(color_bmp);
        let _ = DeleteDC(mem_dc);
        ReleaseDC(HWND::default(), screen_dc);

        hicon.ok()
    }
}
