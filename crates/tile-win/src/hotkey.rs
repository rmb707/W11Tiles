//! Global hotkeys via `RegisterHotKey`.
//!
//! Hotkeys deliver via `WM_HOTKEY` to the registering thread, so this
//! module owns its own message-pump thread (separate from the WinEvent
//! one — we don't want a noisy WinEvent stream to delay hotkey delivery).
//!
//! Key parsing: "SUPER+SHIFT+H" → MOD_WIN | MOD_SHIFT, VK_H.
//! Aliases: SUPER/META/WIN → WIN, ALT/META/MOD1 → ALT, CTRL/CONTROL → CTRL.

#![cfg(windows)]

use std::collections::HashMap;
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use parking_lot::Mutex;
use tile_core::config::Action;
use tokio::sync::mpsc::UnboundedSender;

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT,
    MOD_SHIFT, MOD_WIN,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, PostThreadMessageW, TranslateMessage, MSG, WM_HOTKEY, WM_QUIT,
};

pub struct HotkeyManager {
    thread: Option<JoinHandle<()>>,
    thread_id: u32,
    bindings: Arc<Mutex<HashMap<i32, Action>>>,
}

impl HotkeyManager {
    pub fn start(tx: UnboundedSender<Action>) -> Self {
        let bindings: Arc<Mutex<HashMap<i32, Action>>> = Arc::new(Mutex::new(HashMap::new()));
        let bindings_thread = bindings.clone();
        let (tid_tx, tid_rx) = std::sync::mpsc::channel();

        let thread = thread::spawn(move || {
            let tid = unsafe { windows::Win32::System::Threading::GetCurrentThreadId() };
            let _ = tid_tx.send(tid);
            let mut msg = MSG::default();
            unsafe {
                while GetMessageW(&mut msg, HWND::default(), 0, 0).into() {
                    if msg.message == WM_HOTKEY {
                        let id = msg.wParam.0 as i32;
                        if let Some(action) = bindings_thread.lock().get(&id).cloned() {
                            let _ = tx.send(action);
                        }
                    }
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
        });

        let thread_id = tid_rx.recv().unwrap_or(0);
        Self { thread: Some(thread), thread_id, bindings }
    }

    /// Bind a single keybind. The `RegisterHotKey` call must run on the
    /// owning thread, so we PostThreadMessageW a custom message and have
    /// the pump call register. For now we just call `RegisterHotKey` from
    /// here; Windows lets any thread register if `hwnd` is NULL — the
    /// hotkey fires on the registering thread regardless. The caveat is
    /// the *firing* thread must be the one that pumps messages.
    ///
    /// Workaround: register from the pump thread itself. We hand the call
    /// across via a one-shot oneshot. Until that plumbing's in, this is
    /// best-effort: it works if HotkeyManager::register is called from
    /// the same thread that started it (which the daemon doesn't), so
    /// TODO before shipping: route through a thread-local channel.
    pub fn register(&self, id: i32, keys: &str, action: Action) -> Result<(), String> {
        let (mods, vk) = parse_keybind(keys)?;
        unsafe {
            RegisterHotKey(HWND::default(), id, mods | MOD_NOREPEAT, vk)
                .map_err(|e| format!("RegisterHotKey failed for {keys}: {e}"))?;
        }
        self.bindings.lock().insert(id, action);
        Ok(())
    }

    pub fn unregister_all(&self) {
        let ids: Vec<i32> = self.bindings.lock().keys().copied().collect();
        for id in ids {
            unsafe { let _ = UnregisterHotKey(HWND::default(), id); }
        }
        self.bindings.lock().clear();
    }

    pub fn stop(mut self) {
        self.unregister_all();
        if self.thread_id != 0 {
            unsafe { let _ = PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0)); }
        }
        if let Some(t) = self.thread.take() { let _ = t.join(); }
    }
}

fn parse_keybind(s: &str) -> Result<(HOT_KEY_MODIFIERS, u32), String> {
    let mut mods = HOT_KEY_MODIFIERS(0);
    let mut vk: Option<u32> = None;
    for tok in s.split('+').map(|t| t.trim().to_ascii_uppercase()) {
        match tok.as_str() {
            "SUPER" | "WIN" | "META" | "MOD4" => mods |= MOD_WIN,
            "CTRL"  | "CONTROL"               => mods |= MOD_CONTROL,
            "ALT"   | "MOD1"                  => mods |= MOD_ALT,
            "SHIFT"                           => mods |= MOD_SHIFT,
            other => {
                if let Some(c) = vk_for(other) { vk = Some(c); }
                else { return Err(format!("unknown key token '{other}'")); }
            }
        }
    }
    let vk = vk.ok_or_else(|| format!("no main key in '{s}'"))?;
    Ok((mods, vk))
}

fn vk_for(s: &str) -> Option<u32> {
    use windows::Win32::UI::Input::KeyboardAndMouse::*;
    let bytes = s.as_bytes();
    if bytes.len() == 1 {
        let c = bytes[0];
        if c.is_ascii_alphabetic() { return Some(c.to_ascii_uppercase() as u32); }
        if c.is_ascii_digit()      { return Some(c as u32); }
    }
    Some(match s {
        "SPACE"     => VK_SPACE.0 as u32,
        "TAB"       => VK_TAB.0 as u32,
        "ESC" | "ESCAPE" => VK_ESCAPE.0 as u32,
        "ENTER" | "RETURN" => VK_RETURN.0 as u32,
        "LEFT"  => VK_LEFT.0 as u32,
        "RIGHT" => VK_RIGHT.0 as u32,
        "UP"    => VK_UP.0 as u32,
        "DOWN"  => VK_DOWN.0 as u32,
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
        let (mods, vk) = parse_keybind("SUPER+SHIFT+H").unwrap();
        assert_eq!(mods, MOD_WIN | MOD_SHIFT);
        assert_eq!(vk, b'H' as u32);
    }
    #[test]
    fn parses_digits() {
        let (_, vk) = parse_keybind("SUPER+1").unwrap();
        assert_eq!(vk, b'1' as u32);
    }
}
