//! On-disk config schema.
//!
//! Parsed from `%APPDATA%\TileManager\config.toml` by the daemon at start.
//! Lives in core so `tilectl reload` and the daemon agree on shape.

use serde::{Deserialize, Serialize};

use crate::direction::Direction;
use crate::state::Config as RuntimeConfig;
use crate::workspace::WorkspaceId;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ConfigFile {
    pub gaps: Gaps,
    pub workspaces: WorkspaceConfig,
    pub keybinds: Vec<Keybind>,
    pub float_rules: Vec<FloatRule>,
    pub animation: Animation,
}

impl Default for ConfigFile {
    fn default() -> Self {
        Self {
            gaps: Gaps::default(),
            workspaces: WorkspaceConfig::default(),
            keybinds: default_keybinds(),
            float_rules: default_float_rules(),
            animation: Animation::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Gaps {
    pub outer: i32,
    pub inner: i32,
}
impl Default for Gaps { fn default() -> Self { Self { outer: 0, inner: 0 } } }

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkspaceConfig {
    pub per_monitor: u16,
    pub follow_focus: bool,
}
impl Default for WorkspaceConfig {
    fn default() -> Self { Self { per_monitor: 9, follow_focus: true } }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Keybind {
    /// Hyprland-ish format: "SUPER+SHIFT+H" — case insensitive.
    pub keys: String,
    pub action: Action,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Action {
    FocusDirection { dir: Direction },
    SwapDirection  { dir: Direction },
    ResizeDirection{ dir: Direction, delta: f32 },
    ToggleFloat,
    SwitchWorkspace { id: WorkspaceId },
    MoveToWorkspace { id: WorkspaceId },
    /// Pull the focused window out of its tab group (no-op if not tabbed).
    UntabWindow,
    /// Cycle tabs in the focused window's tab group.
    CycleTab { forward: bool },
    Quit,
    Spawn { command: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FloatRule {
    /// Win32 class match — `*` wildcard supported at start/end only for now.
    pub class: Option<String>,
    pub exe: Option<String>,
    pub title_contains: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Animation {
    pub enabled: bool,
    pub duration_ms: u32,
    /// "ease-out", "ease-in-out", "linear" — string for now, parsed at apply
    pub curve: String,
}
impl Default for Animation {
    fn default() -> Self { Self { enabled: true, duration_ms: 140, curve: "ease-out".into() } }
}

impl ConfigFile {
    pub fn into_runtime(&self) -> RuntimeConfig {
        RuntimeConfig {
            outer_gap: self.gaps.outer,
            inner_gap: self.gaps.inner,
            workspaces_per_monitor: self.workspaces.per_monitor,
            resize_step: 0.05,
        }
    }
}

fn default_keybinds() -> Vec<Keybind> {
    // NOTE: bare `WIN+<key>` is mostly unusable as default — Windows itself
    // owns Win+L (lock), Win+H (dictation), Win+SPACE (kbd layout), Win+1..9
    // (taskbar), Win+E, Win+R, Win+Tab, Win+D, etc. RegisterHotKey returns
    // 0x80070581 ("hot key already registered") for all of those.
    //
    // Production tilers (komorebi, GlazeWM) sidestep this by using a
    // low-level keyboard hook (WH_KEYBOARD_LL) that intercepts before the
    // shell — that's tracked as a known sharp edge in the README.
    //
    // Until then, we default to WIN+ALT+<key>, which is reliably free
    // across Windows 10/11 and matches Hyprland's "two-mod" feel.
    use Action::*;
    use Direction::*;
    fn k(keys: &str, a: Action) -> Keybind { Keybind { keys: keys.into(), action: a } }
    let mut binds = vec![
        k("SUPER+ALT+H", FocusDirection { dir: Left }),
        k("SUPER+ALT+L", FocusDirection { dir: Right }),
        k("SUPER+ALT+K", FocusDirection { dir: Up }),
        k("SUPER+ALT+J", FocusDirection { dir: Down }),
        k("SUPER+ALT+SHIFT+H", SwapDirection { dir: Left }),
        k("SUPER+ALT+SHIFT+L", SwapDirection { dir: Right }),
        k("SUPER+ALT+SHIFT+K", SwapDirection { dir: Up }),
        k("SUPER+ALT+SHIFT+J", SwapDirection { dir: Down }),
        k("SUPER+ALT+CTRL+H", ResizeDirection { dir: Left,  delta: 0.05 }),
        k("SUPER+ALT+CTRL+L", ResizeDirection { dir: Right, delta: 0.05 }),
        k("SUPER+ALT+CTRL+K", ResizeDirection { dir: Up,    delta: 0.05 }),
        k("SUPER+ALT+CTRL+J", ResizeDirection { dir: Down,  delta: 0.05 }),
        k("SUPER+ALT+SPACE", ToggleFloat),
        k("SUPER+ALT+TAB", CycleTab { forward: true }),
        k("SUPER+ALT+SHIFT+TAB", CycleTab { forward: false }),
        k("SUPER+ALT+U", UntabWindow),
        k("SUPER+ALT+Q", Quit),
    ];
    for n in 1u16..=9 {
        binds.push(k(&format!("SUPER+ALT+{n}"), SwitchWorkspace { id: WorkspaceId(n) }));
        binds.push(k(&format!("SUPER+ALT+SHIFT+{n}"), MoveToWorkspace { id: WorkspaceId(n) }));
    }
    binds
}

fn default_float_rules() -> Vec<FloatRule> {
    vec![
        // Modal dialogs and OS chrome we don't want to tile.
        FloatRule { class: Some("#32770".into()), exe: None, title_contains: None },
        FloatRule { class: Some("ApplicationFrameWindow".into()), exe: Some("PickerHost.exe".into()), title_contains: None },
        FloatRule { class: None, exe: Some("Taskmgr.exe".into()), title_contains: None },
        FloatRule { class: None, exe: None, title_contains: Some("Settings".into()) },
    ]
}
