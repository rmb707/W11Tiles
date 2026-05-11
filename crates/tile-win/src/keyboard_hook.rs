//! Global hotkeys via a low-level keyboard hook (`WH_KEYBOARD_LL`).
//!
//! ## Why not `RegisterHotKey`?
//! `RegisterHotKey` is a *cooperative* API — Windows itself owns most of
//! the useful chords (`WIN+L`, `WIN+H`, `WIN+SPACE`, `WIN+1..9`, …) and
//! refuses to register them with `0x80070581`. komorebi and GlazeWM both
//! solved this by installing a low-level keyboard hook, which sits in the
//! input pipeline *ahead* of the shell: we see the keystroke before
//! Explorer does, and returning a non-zero LRESULT swallows it.
//!
//! ## Threading model (matches `hook.rs`)
//!   - One dedicated thread owns the hook handle.
//!   - That thread pumps `GetMessageW` until `WM_QUIT` — the hook is
//!     silently uninstalled by Windows if its owning thread stops pumping.
//!   - The callback is `extern "system"`, can't take user data, so we
//!     reach the bindings map and the channel through a `static CTX`.
//!   - `register()` mutates the bindings map under a `parking_lot::Mutex`,
//!     so it is safe to call from any thread (including the daemon's
//!     tokio runtime) at any time, including while the callback is firing.
//!
//! ## Key parsing
//! Same surface as `hotkey.rs::parse_keybind`: case-insensitive,
//! `+`-separated, aliases SUPER/WIN/META/MOD4 → Win, ALT/MOD1 → Alt,
//! CTRL/CONTROL → Ctrl, SHIFT → Shift. The modifier *flags* however are
//! our own (`MOD_WIN`/`MOD_ALT`/`MOD_SHIFT`/`MOD_CTRL`) — `HOT_KEY_MODIFIERS`
//! from `RegisterHotKey` use a different bit layout and aren't reusable here.

#![cfg(windows)]

use std::collections::HashMap;
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use parking_lot::Mutex;
use tile_core::config::Action;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, warn};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, VK_CONTROL, VK_DOWN, VK_ESCAPE, VK_F1, VK_F10, VK_F11, VK_F12, VK_F2, VK_F3,
    VK_F4, VK_F5, VK_F6, VK_F7, VK_F8, VK_F9, VK_LEFT, VK_LWIN, VK_MENU, VK_RETURN, VK_RIGHT,
    VK_RWIN, VK_SHIFT, VK_SPACE, VK_TAB, VK_UP,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetMessageW, PostThreadMessageW, SetWindowsHookExW,
    TranslateMessage, UnhookWindowsHookEx, HC_ACTION, HHOOK, KBDLLHOOKSTRUCT, MSG, WH_KEYBOARD_LL,
    WM_KEYDOWN, WM_QUIT, WM_SYSKEYDOWN,
};

/// Our own modifier bit layout. Deliberately *not* `HOT_KEY_MODIFIERS` —
/// those are `MOD_ALT=1, MOD_CONTROL=2, MOD_SHIFT=4, MOD_WIN=8`, but for
/// the LL hook we synthesize the mask ourselves from `GetAsyncKeyState`,
/// so the values are arbitrary as long as we're consistent internally.
pub const MOD_WIN:   u32 = 1 << 0;
pub const MOD_ALT:   u32 = 1 << 1;
pub const MOD_SHIFT: u32 = 1 << 2;
pub const MOD_CTRL:  u32 = 1 << 3;

/// A parsed binding key: modifier mask + virtual-key code.
/// Used as the HashMap key, so `Eq + Hash` is required.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeyChord {
    pub mods: u32,
    pub vk:   u32,
}

/// State the callback needs. `extern "system"` can't carry user data,
/// so this lives in a static `Mutex<Option<Arc<_>>>` (same trick as
/// `hook.rs::CTX`). The `Arc` lets the callback clone out the pointer
/// while holding the lock for as short as possible.
struct HookCtx {
    bindings: Arc<Mutex<HashMap<KeyChord, Action>>>,
    tx:       UnboundedSender<Action>,
}

static CTX: Mutex<Option<Arc<HookCtx>>> = Mutex::new(None);

/// Owner of the message-pump thread that hosts the hook.
///
/// The public surface is intentionally identical in *shape* to
/// `HotkeyManager` so the daemon can swap one for the other:
/// `start` → `register` → … → `stop`. The only meaningful difference
/// is `register` no longer needs an `id: i32` — the hook dispatches on
/// the chord itself, not on a Win32-assigned ID.
pub struct KeyboardHook {
    thread:     Option<JoinHandle<()>>,
    thread_id:  u32,
    bindings:   Arc<Mutex<HashMap<KeyChord, Action>>>,
}

impl KeyboardHook {
    /// Install the hook on a freshly-spawned thread and return immediately.
    /// The returned `KeyboardHook` is dormant until `register()` is called.
    pub fn start(tx: UnboundedSender<Action>) -> Self {
        let bindings: Arc<Mutex<HashMap<KeyChord, Action>>> =
            Arc::new(Mutex::new(HashMap::new()));
        *CTX.lock() = Some(Arc::new(HookCtx { bindings: bindings.clone(), tx }));

        let (tid_tx, tid_rx) = std::sync::mpsc::channel();
        let thread = thread::spawn(move || {
            let tid = unsafe { windows::Win32::System::Threading::GetCurrentThreadId() };
            let _ = tid_tx.send(tid);

            // hMod must be a real module handle (NOT NULL) for an LL hook;
            // Microsoft docs are explicit that NULL is rejected for
            // `WH_KEYBOARD_LL`. `GetModuleHandleW(None)` returns the EXE's
            // own module which is fine — we're not injecting into other
            // processes for LL hooks, the OS calls us back in our process.
            let h_module = match unsafe { GetModuleHandleW(None) } {
                Ok(h) => h,
                Err(e) => {
                    warn!("GetModuleHandleW failed: {e}");
                    return;
                }
            };

            let hhook: HHOOK = match unsafe {
                SetWindowsHookExW(WH_KEYBOARD_LL, Some(ll_callback), h_module, 0)
            } {
                Ok(h) => h,
                Err(e) => {
                    warn!("SetWindowsHookExW(WH_KEYBOARD_LL) failed: {e}");
                    return;
                }
            };

            // Pump messages on this thread or Windows silently removes
            // the hook (LL hooks have a per-thread timeout — see
            // LowLevelHooksTimeout in HKCU\Control Panel\Desktop).
            let mut msg = MSG::default();
            unsafe {
                while GetMessageW(&mut msg, HWND::default(), 0, 0).into() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }

            unsafe { let _ = UnhookWindowsHookEx(hhook); }
            *CTX.lock() = None;
        });

        let thread_id = tid_rx.recv().unwrap_or(0);
        Self { thread: Some(thread), thread_id, bindings }
    }

    /// Bind a key chord to an `Action`. Re-binding an existing chord
    /// silently overwrites — matching the Hyprland model. Returns
    /// `Err` only if the keys string can't be parsed; the hook
    /// itself can't fail to "register" (no Win32 call here, just a
    /// HashMap insert).
    pub fn register(&self, keys: &str, action: Action) -> Result<(), String> {
        let chord = parse_chord(keys)?;
        self.bindings.lock().insert(chord, action);
        Ok(())
    }

    /// Drop every binding. The hook stays installed — the callback
    /// will simply pass every event through `CallNextHookEx`.
    pub fn unregister_all(&self) {
        self.bindings.lock().clear();
    }

    /// Stop the pump thread, uninstall the hook, and join. Idempotent
    /// in the sense that calling it twice is a compile error (consumes
    /// `self`), but you should call it exactly once on shutdown.
    pub fn stop(mut self) {
        self.unregister_all();
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

/// The actual hook procedure. Runs on the pump thread, called by Windows
/// for every keystroke before it reaches any application.
///
/// Returning `LRESULT(1)` (or really any non-zero) tells the OS "don't
/// dispatch this further" — the keystroke vanishes from Windows' point
/// of view, including from foreground apps and the shell. That's what
/// gives us bindings on chords Windows would normally claim.
unsafe extern "system" fn ll_callback(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // Per docs, anything other than HC_ACTION must be passed through
    // untouched and we should not inspect wparam/lparam.
    if code != HC_ACTION as i32 {
        return CallNextHookEx(HHOOK::default(), code, wparam, lparam);
    }

    // We only act on key-down. WM_KEYUP / WM_SYSKEYUP must still be
    // forwarded — if we ate the down but not the up, apps would see a
    // stuck key. Forwarding both up events keeps the OS state machine
    // consistent. (We don't need to *match* on up; we just don't swallow.)
    let is_down = wparam.0 as u32 == WM_KEYDOWN || wparam.0 as u32 == WM_SYSKEYDOWN;
    if !is_down {
        return CallNextHookEx(HHOOK::default(), code, wparam, lparam);
    }

    // KBDLLHOOKSTRUCT lives in the LL hook's address space — safe to
    // read but not to retain a pointer past the callback return.
    let kb = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
    let vk = kb.vkCode;

    // Skip injected events to avoid feedback loops if anything in this
    // process ever calls SendInput. (komorebi-style guard.)
    // LLKHF_INJECTED = 0x00000010
    if (kb.flags.0 & 0x00000010) != 0 {
        return CallNextHookEx(HHOOK::default(), code, wparam, lparam);
    }

    // GetAsyncKeyState's high bit is "currently down". Build our mask.
    // VK_MENU / VK_SHIFT / VK_CONTROL are the *generic* virtual keys —
    // they fire for both left and right physical keys, which is what
    // we want for binding purposes.
    let mut mods = 0u32;
    if (GetAsyncKeyState(VK_LWIN.0    as i32) as u16) & 0x8000 != 0 { mods |= MOD_WIN; }
    if (GetAsyncKeyState(VK_RWIN.0    as i32) as u16) & 0x8000 != 0 { mods |= MOD_WIN; }
    if (GetAsyncKeyState(VK_MENU.0    as i32) as u16) & 0x8000 != 0 { mods |= MOD_ALT; }
    if (GetAsyncKeyState(VK_SHIFT.0   as i32) as u16) & 0x8000 != 0 { mods |= MOD_SHIFT; }
    if (GetAsyncKeyState(VK_CONTROL.0 as i32) as u16) & 0x8000 != 0 { mods |= MOD_CTRL; }

    // Don't treat the modifier keys themselves as a chord's main key —
    // otherwise *every* press of Win/Alt/Ctrl/Shift would do a HashMap
    // lookup with no main key set, which is harmless but wasteful.
    let is_modifier = matches!(
        vk,
        v if v == VK_LWIN.0    as u32
          || v == VK_RWIN.0    as u32
          || v == VK_MENU.0    as u32
          || v == VK_SHIFT.0   as u32
          || v == VK_CONTROL.0 as u32
    );
    if is_modifier {
        return CallNextHookEx(HHOOK::default(), code, wparam, lparam);
    }

    // Take a snapshot of the Arc so we can drop the lock before sending.
    let ctx = match CTX.lock().clone() {
        Some(c) => c,
        None => return CallNextHookEx(HHOOK::default(), code, wparam, lparam),
    };

    let chord = KeyChord { mods, vk };
    let hit = ctx.bindings.lock().get(&chord).cloned();

    if let Some(action) = hit {
        if let Err(e) = ctx.tx.send(action) {
            debug!("keyboard hook channel closed: {e}");
        }
        // Swallow.
        return LRESULT(1);
    }

    CallNextHookEx(HHOOK::default(), code, wparam, lparam)
}

/// Parse a chord string like `"SUPER+SHIFT+H"` into our `KeyChord`.
/// Same alias rules as `hotkey.rs::parse_keybind` so configs are
/// drop-in compatible — only the *output* type differs.
pub fn parse_chord(s: &str) -> Result<KeyChord, String> {
    let mut mods: u32 = 0;
    let mut vk: Option<u32> = None;
    for tok in s.split('+').map(|t| t.trim().to_ascii_uppercase()) {
        match tok.as_str() {
            "SUPER" | "WIN" | "META" | "MOD4" => mods |= MOD_WIN,
            "CTRL"  | "CONTROL"               => mods |= MOD_CTRL,
            "ALT"   | "MOD1"                  => mods |= MOD_ALT,
            "SHIFT"                           => mods |= MOD_SHIFT,
            other => {
                if let Some(c) = vk_for(other) { vk = Some(c); }
                else { return Err(format!("unknown key token '{other}'")); }
            }
        }
    }
    let vk = vk.ok_or_else(|| format!("no main key in '{s}'"))?;
    Ok(KeyChord { mods, vk })
}

/// Map a textual key name to its Win32 VK code.
/// Single ASCII letters/digits map directly (VK codes for A–Z and 0–9
/// are their ASCII values). Named keys go through the table.
fn vk_for(s: &str) -> Option<u32> {
    let bytes = s.as_bytes();
    if bytes.len() == 1 {
        let c = bytes[0];
        if c.is_ascii_alphabetic() { return Some(c.to_ascii_uppercase() as u32); }
        if c.is_ascii_digit()      { return Some(c as u32); }
    }
    Some(match s {
        "SPACE"            => VK_SPACE.0  as u32,
        "TAB"              => VK_TAB.0    as u32,
        "ESC" | "ESCAPE"   => VK_ESCAPE.0 as u32,
        "ENTER" | "RETURN" => VK_RETURN.0 as u32,
        "LEFT"  => VK_LEFT.0  as u32,
        "RIGHT" => VK_RIGHT.0 as u32,
        "UP"    => VK_UP.0    as u32,
        "DOWN"  => VK_DOWN.0  as u32,
        "F1"  => VK_F1.0  as u32, "F2"  => VK_F2.0  as u32, "F3"  => VK_F3.0  as u32,
        "F4"  => VK_F4.0  as u32, "F5"  => VK_F5.0  as u32, "F6"  => VK_F6.0  as u32,
        "F7"  => VK_F7.0  as u32, "F8"  => VK_F8.0  as u32, "F9"  => VK_F9.0  as u32,
        "F10" => VK_F10.0 as u32, "F11" => VK_F11.0 as u32, "F12" => VK_F12.0 as u32,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_super_shift_h() {
        let chord = parse_chord("SUPER+SHIFT+H").unwrap();
        assert_eq!(chord.mods, MOD_WIN | MOD_SHIFT);
        assert_eq!(chord.vk, b'H' as u32);
    }

    #[test]
    fn parses_win_alias() {
        // WIN, META, MOD4 all alias to MOD_WIN.
        let a = parse_chord("WIN+A").unwrap();
        let b = parse_chord("META+A").unwrap();
        let c = parse_chord("MOD4+A").unwrap();
        assert_eq!(a, b);
        assert_eq!(b, c);
        assert_eq!(a.mods, MOD_WIN);
    }

    #[test]
    fn parses_super_alt_ctrl_shift_digit() {
        let chord = parse_chord("SUPER+ALT+CTRL+SHIFT+1").unwrap();
        assert_eq!(chord.mods, MOD_WIN | MOD_ALT | MOD_CTRL | MOD_SHIFT);
        assert_eq!(chord.vk, b'1' as u32);
    }

    #[test]
    fn parses_named_keys() {
        assert_eq!(parse_chord("F11").unwrap().vk, VK_F11.0 as u32);
        assert_eq!(parse_chord("SUPER+SPACE").unwrap().vk, VK_SPACE.0 as u32);
        assert_eq!(parse_chord("ALT+ESCAPE").unwrap().vk, VK_ESCAPE.0 as u32);
    }

    #[test]
    fn rejects_unknown_token() {
        assert!(parse_chord("SUPER+FROBNICATE").is_err());
    }

    #[test]
    fn rejects_no_main_key() {
        assert!(parse_chord("SUPER+ALT").is_err());
    }

    #[test]
    fn case_insensitive() {
        let upper = parse_chord("SUPER+SHIFT+H").unwrap();
        let lower = parse_chord("super+shift+h").unwrap();
        let mixed = parse_chord("Super+Shift+H").unwrap();
        assert_eq!(upper, lower);
        assert_eq!(lower, mixed);
    }
}
