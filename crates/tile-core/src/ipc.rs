//! IPC schema between `tilectl` and `tile-daemon`.
//!
//! Wire format: newline-delimited JSON over a Windows named pipe at
//! `\\.\pipe\tilemanager.sock` (one request → one response). JSON keeps
//! debugging trivial; performance is fine since IPC traffic is one packet
//! per keystroke at most.

use serde::{Deserialize, Serialize};

use crate::direction::Direction;
use crate::window::WindowId;
use crate::workspace::WorkspaceId;

pub const PIPE_NAME: &str = r"\\.\pipe\tilemanager.sock";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    /// Daemon health probe.
    Ping,
    /// Re-read the config file.
    ReloadConfig,
    /// Dump current state as JSON for debugging / status bars.
    Dump,

    FocusDirection  { dir: Direction },
    SwapDirection   { dir: Direction },
    ResizeDirection { dir: Direction, delta: f32 },
    ToggleFloat,
    SwitchWorkspace { id: WorkspaceId },
    MoveToWorkspace { id: WorkspaceId },
    /// Pull the focused window out of its tab group (no-op if not tabbed).
    UntabWindow,
    /// Cycle tabs in the focused window's tab group.
    CycleTab        { forward: bool },
    /// Make a specific window the active tab in its group.
    ActivateTab     { window: WindowId },
    Quit,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "ok", rename_all = "snake_case")]
pub enum Response {
    Ok,
    Pong { version: String },
    State { json: String },
    Error { message: String },
}
