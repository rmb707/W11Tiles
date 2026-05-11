//! Windows integration layer.
//!
//! Everything here that touches Win32 is gated behind `cfg(windows)` so
//! `tile-core` tests on CI Linux runners stay green. Non-Windows builds
//! get `unimplemented!()` stubs so downstream crates still compile.

#![cfg_attr(not(windows), allow(unused))]

pub mod hwnd_map;
pub mod manageable;
pub mod monitors;
pub mod hook;
pub mod applier;
pub mod hotkey;
pub mod keyboard_hook;
pub mod ipc_server;
pub mod dpi;
pub mod vdesktop;
pub mod tab_strip;
pub mod drop_zones;
pub mod tray;

#[cfg(windows)]
pub use hook::EventHook;
#[cfg(windows)]
pub use applier::Applier;
#[cfg(windows)]
pub use hotkey::HotkeyManager;
