//! Top-level world state.
//!
//! `tile-win` translates Win32 events into [`Event`]s and pushes them in.
//! [`State::apply`] mutates the world and returns the set of monitors whose
//! layouts changed; the daemon then asks for [`LayoutPlan`]s for each one
//! and hands them back to the applier.
//!
//! Keeping this single function the only mutation entry point means the
//! daemon can serialize state to disk on Ctrl+C, replay events for tests,
//! and reason about animations as state-snapshot diffs.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::direction::Direction;
use crate::geom::Rect;
use crate::layout::{Edge, LayoutPlan, SplitAxis};
use crate::window::{WindowId, WindowInfo};
use crate::workspace::{Workspace, WorkspaceId};

#[derive(Debug, Error)]
pub enum StateError {
    #[error("unknown monitor: {0}")]
    UnknownMonitor(MonitorId),
    #[error("unknown window: {0}")]
    UnknownWindow(WindowId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct MonitorId(pub u32);

impl std::fmt::Display for MonitorId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "mon#{}", self.0)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Monitor {
    pub id: MonitorId,
    /// Full monitor area in virtual-screen pixels.
    pub bounds: Rect,
    /// Work area: monitor minus taskbar/AppBars. Layout uses this.
    pub work_area: Rect,
    /// Effective DPI scale (96 = 100%). For HiDPI rounding decisions later.
    pub dpi: u32,
    pub workspaces: Vec<Workspace>,
    pub active_workspace: WorkspaceId,
}

impl Monitor {
    pub fn active(&self) -> &Workspace {
        self.workspaces.iter().find(|w| w.id == self.active_workspace)
            .expect("active_workspace always points at an existing workspace")
    }
    pub fn active_mut(&mut self) -> &mut Workspace {
        let id = self.active_workspace;
        self.workspaces.iter_mut().find(|w| w.id == id)
            .expect("active_workspace always points at an existing workspace")
    }

    /// Get a workspace by id, creating it (with the monitor's gap config) if it
    /// doesn't exist yet. Used to back the dynamic-workspace model: each
    /// Windows virtual desktop maps to a unique `WorkspaceId`, and we don't
    /// know how many VDs the user will use — they're created on demand as
    /// the daemon discovers them.
    pub fn ensure_workspace(&mut self, id: WorkspaceId, outer_gap: i32, inner_gap: i32) -> &mut Workspace {
        if !self.workspaces.iter().any(|w| w.id == id) {
            self.workspaces.push(Workspace::new(id, outer_gap, inner_gap));
        }
        self.workspaces.iter_mut().find(|w| w.id == id).unwrap()
    }
}

/// Inputs into the state machine. These cover everything `tile-win`
/// observes plus everything `tilectl` can request — there is intentionally
/// only one event type, so the daemon's main loop is a single `match`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Event {
    // ---- environment changes (from tile-win) ----
    MonitorAttached  { monitor: Monitor },
    MonitorDetached  { id: MonitorId },
    MonitorReshaped  { id: MonitorId, bounds: Rect, work_area: Rect, dpi: u32 },

    // ---- window lifecycle (from tile-win) ----
    /// `workspace` identifies the Windows virtual desktop the window lives
    /// on, mapped by the daemon to a stable `WorkspaceId`. The state machine
    /// will create that workspace on the named monitor if it doesn't exist.
    WindowOpened    { info: WindowInfo, monitor: MonitorId, workspace: WorkspaceId },
    WindowClosed    { id: WindowId },
    WindowFocused   { id: WindowId },
    /// User dragged the window outside its tile cell — fall back to floating.
    WindowFloated   { id: WindowId },

    // ---- user commands (from hotkeys or tilectl) ----
    FocusDirection  { dir: Direction },
    SwapDirection   { dir: Direction },
    ResizeDirection { dir: Direction, delta: f32 },
    ToggleFloat,
    SwitchWorkspace { id: WorkspaceId },
    MoveToWorkspace { id: WorkspaceId },
    /// Drag-merge: drop `src` into `target`'s cell, forming (or extending)
    /// a tab group. Both windows must already live on the same workspace —
    /// the daemon enforces that before emitting the event.
    MergeWindows    { src: WindowId, target: WindowId },
    /// Pull the focused window out of its tab group into a sibling cell.
    /// No-op if the focused window isn't tabbed.
    UntabWindow,
    /// Cycle the active tab in the focused window's tab group. The newly
    /// active window also takes focus so subsequent commands operate on it.
    CycleTab        { forward: bool },
    /// Make `window` the active tab in whatever tab group it lives in.
    /// Emitted when the user clicks a tab on the strip overlay. No-op if
    /// `window` isn't part of a tab group.
    ActivateTab     { window: WindowId },
    /// Drop-at-edge: remove `src` from wherever it lives and insert it
    /// beside `target`, splitting along the edge direction. Center drops
    /// go to `MergeWindows`; this event handles the four cardinal edges.
    DropAtEdge      { src: WindowId, target: WindowId, edge: Edge },
    Quit,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct State {
    pub monitors: Vec<Monitor>,
    pub windows: HashMap<WindowId, WindowInfo>,
    /// Window → (monitor, workspace) lookup. Mirrors the trees but is O(1).
    pub locations: HashMap<WindowId, (MonitorId, WorkspaceId)>,
    pub focused_monitor: Option<MonitorId>,
    pub config: Config,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub outer_gap: i32,
    pub inner_gap: i32,
    pub workspaces_per_monitor: u16,
    pub resize_step: f32,
}

impl Default for Config {
    fn default() -> Self {
        // Zero gaps default — edge-to-edge tiling. Users who want
        // visible gaps between tiles can set non-zero values in
        // `%APPDATA%\TileManager\config.toml`.
        Self { outer_gap: 0, inner_gap: 0, workspaces_per_monitor: 9, resize_step: 0.05 }
    }
}

/// Result of [`State::apply`]: which monitors need re-rendered, plus a
/// signal asking the daemon to shut down (returned for `Event::Quit`).
#[derive(Debug, Default, Clone)]
pub struct ApplyOutcome {
    pub dirty_monitors: Vec<MonitorId>,
    pub quit: bool,
}

impl State {
    pub fn new(config: Config) -> Self {
        Self { config, ..Default::default() }
    }

    pub fn monitor(&self, id: MonitorId) -> Option<&Monitor> {
        self.monitors.iter().find(|m| m.id == id)
    }
    fn monitor_mut(&mut self, id: MonitorId) -> Option<&mut Monitor> {
        self.monitors.iter_mut().find(|m| m.id == id)
    }

    /// Lookup by `WindowId`: is the window currently part of a `Tabbed`
    /// group? Used by the daemon to make drag-release on blank space
    /// snap a tab-group member *back* into its group instead of yanking
    /// it out — popping out should be an explicit user action.
    pub fn is_in_tab_group(&self, id: WindowId) -> bool {
        let Some((mon_id, ws_id)) = self.locations.get(&id).copied() else { return false };
        let Some(mon) = self.monitor(mon_id) else { return false };
        mon.workspaces.iter()
            .find(|w| w.id == ws_id)
            .map(|w| w.layout.is_in_tab_group(id))
            .unwrap_or(false)
    }

    pub fn plan_for(&self, id: MonitorId) -> Option<LayoutPlan> {
        self.monitor(id).map(|m| m.active().compute(m.work_area))
    }

    pub fn apply(&mut self, ev: Event) -> Result<ApplyOutcome, StateError> {
        let mut out = ApplyOutcome::default();
        match ev {
            Event::MonitorAttached { mut monitor } => {
                // Workspaces are created on demand as the daemon discovers
                // virtual desktops, but we need at least one so `active()`
                // and `plan_for()` can succeed before any window opens.
                if monitor.workspaces.is_empty() {
                    let ws = Workspace::new(monitor.active_workspace, self.config.outer_gap, self.config.inner_gap);
                    monitor.workspaces.push(ws);
                } else if !monitor.workspaces.iter().any(|w| w.id == monitor.active_workspace) {
                    monitor.active_workspace = monitor.workspaces[0].id;
                }
                let id = monitor.id;
                self.monitors.push(monitor);
                if self.focused_monitor.is_none() { self.focused_monitor = Some(id); }
                out.dirty_monitors.push(id);
            }
            Event::MonitorDetached { id } => {
                // Migrate windows from the detached monitor to the focused one.
                let migrated: Vec<WindowId> = self
                    .monitor(id)
                    .map(|m| m.workspaces.iter().flat_map(|w| w.windows()).collect())
                    .unwrap_or_default();
                self.monitors.retain(|m| m.id != id);
                if self.focused_monitor == Some(id) {
                    self.focused_monitor = self.monitors.first().map(|m| m.id);
                }
                if let Some(home) = self.focused_monitor {
                    let landed_ws = if let Some(target) = self.monitor_mut(home) {
                        let ws = target.active_workspace;
                        let work_area = target.work_area;
                        let active = target.active_mut();
                        for w in migrated.iter().copied() {
                            active.layout.insert(w, active.focused, work_area);
                            active.focused = Some(w);
                        }
                        out.dirty_monitors.push(home);
                        Some(ws)
                    } else { None };
                    if let Some(ws) = landed_ws {
                        for w in migrated {
                            self.locations.insert(w, (home, ws));
                        }
                    }
                }
            }
            Event::MonitorReshaped { id, bounds, work_area, dpi } => {
                let mon = self.monitor_mut(id).ok_or(StateError::UnknownMonitor(id))?;
                mon.bounds = bounds;
                mon.work_area = work_area;
                mon.dpi = dpi;
                out.dirty_monitors.push(id);
            }

            Event::WindowOpened { info, monitor, workspace } => {
                let id = info.id;
                // Idempotency guard. Same WindowOpened can fire twice for
                // the same id when the periodic discover-tick races the
                // hook's EVENT_OBJECT_SHOW, or when a window briefly
                // cloaks/uncloaks during VD transitions. Re-inserting
                // into the BSP tree creates duplicate leaves that crash
                // the tree walk on the next operation. So: if we already
                // know this window, refresh its info but don't touch the
                // layout.
                if self.windows.contains_key(&id) {
                    self.windows.insert(id, info);
                    return Ok(out);
                }
                let outer_gap = self.config.outer_gap;
                let inner_gap = self.config.inner_gap;
                let mon = self.monitor_mut(monitor).ok_or(StateError::UnknownMonitor(monitor))?;
                let work_area = mon.work_area;
                let target_ws = mon.ensure_workspace(workspace, outer_gap, inner_gap);
                if !info.floating {
                    target_ws.layout.insert(id, target_ws.focused, work_area);
                }
                target_ws.focused = Some(id);
                self.windows.insert(id, info);
                self.locations.insert(id, (monitor, workspace));
                // Only repaint if the new window landed on the currently-
                // visible workspace; windows on other VDs get tiled but stay
                // off-screen until the user switches to them.
                if self.monitor(monitor).map(|m| m.active_workspace) == Some(workspace) {
                    out.dirty_monitors.push(monitor);
                }
            }
            Event::WindowClosed { id } => {
                if let Some((mon_id, ws_id)) = self.locations.remove(&id) {
                    if let Some(mon) = self.monitor_mut(mon_id) {
                        if let Some(ws) = mon.workspaces.iter_mut().find(|w| w.id == ws_id) {
                            ws.layout.remove(id);
                            if ws.focused == Some(id) {
                                ws.focused = ws.layout.windows().last().copied();
                            }
                        }
                        out.dirty_monitors.push(mon_id);
                    }
                }
                self.windows.remove(&id);
            }
            Event::WindowFocused { id } => {
                if let Some((mon_id, ws_id)) = self.locations.get(&id).copied() {
                    self.focused_monitor = Some(mon_id);
                    if let Some(mon) = self.monitor_mut(mon_id) {
                        mon.active_workspace = ws_id;
                        if let Some(ws) = mon.workspaces.iter_mut().find(|w| w.id == ws_id) {
                            ws.focused = Some(id);
                        }
                    }
                }
            }
            Event::WindowFloated { id } => {
                if let Some(info) = self.windows.get_mut(&id) {
                    info.floating = true;
                }
                if let Some((mon_id, ws_id)) = self.locations.get(&id).copied() {
                    if let Some(mon) = self.monitor_mut(mon_id) {
                        if let Some(ws) = mon.workspaces.iter_mut().find(|w| w.id == ws_id) {
                            ws.layout.remove(id);
                        }
                        out.dirty_monitors.push(mon_id);
                    }
                }
            }

            Event::FocusDirection { dir } => {
                if let Some((mon, ws, focused)) = self.focus_context() {
                    if let Some(target) = self.monitor(mon)
                        .and_then(|m| m.workspaces.iter().find(|w| w.id == ws))
                        .and_then(|w| w.layout.neighbour(focused, dir))
                    {
                        // Defer to the daemon: re-emit as WindowFocused after
                        // it raises the actual HWND. Returning dirty=[] keeps
                        // layouts as-is; only focus changes.
                        if let Some(mon) = self.monitor_mut(mon) {
                            if let Some(ws) = mon.workspaces.iter_mut().find(|w| w.id == ws) {
                                ws.focused = Some(target);
                            }
                        }
                    }
                }
            }
            Event::SwapDirection { dir } => {
                if let Some((mon, ws, focused)) = self.focus_context() {
                    let target = self.monitor(mon)
                        .and_then(|m| m.workspaces.iter().find(|w| w.id == ws))
                        .and_then(|w| w.layout.neighbour(focused, dir));
                    if let Some(target) = target {
                        if let Some(mon_ref) = self.monitor_mut(mon) {
                            if let Some(ws_ref) = mon_ref.workspaces.iter_mut().find(|w| w.id == ws) {
                                ws_ref.layout.swap(focused, target);
                            }
                        }
                        out.dirty_monitors.push(mon);
                    }
                }
            }
            Event::ResizeDirection { dir, delta } => {
                if let Some((mon, ws, focused)) = self.focus_context() {
                    let axis = if dir.is_horizontal() { SplitAxis::Horizontal } else { SplitAxis::Vertical };
                    let signed = match dir {
                        Direction::Right | Direction::Down => delta,
                        Direction::Left  | Direction::Up   => -delta,
                    };
                    if let Some(mon_ref) = self.monitor_mut(mon) {
                        if let Some(ws_ref) = mon_ref.workspaces.iter_mut().find(|w| w.id == ws) {
                            ws_ref.layout.resize(focused, axis, signed);
                        }
                    }
                    out.dirty_monitors.push(mon);
                }
            }
            Event::ToggleFloat => {
                if let Some((_, _, focused)) = self.focus_context() {
                    let was_floating = self.windows.get(&focused).map(|w| w.floating).unwrap_or(false);
                    if was_floating {
                        if let Some(info) = self.windows.get_mut(&focused) {
                            info.floating = false;
                        }
                        if let Some((mon_id, ws_id)) = self.locations.get(&focused).copied() {
                            if let Some(mon) = self.monitor_mut(mon_id) {
                                let work_area = mon.work_area;
                                if let Some(ws) = mon.workspaces.iter_mut().find(|w| w.id == ws_id) {
                                    let near = ws.focused;
                                    ws.layout.insert(focused, near, work_area);
                                }
                                out.dirty_monitors.push(mon_id);
                            }
                        }
                    } else {
                        // delegate to the WindowFloated handler so semantics stay one-place
                        return self.apply(Event::WindowFloated { id: focused });
                    }
                }
            }
            Event::SwitchWorkspace { id: target } => {
                let outer_gap = self.config.outer_gap;
                let inner_gap = self.config.inner_gap;
                if let Some(mon_id) = self.focused_monitor {
                    if let Some(mon) = self.monitor_mut(mon_id) {
                        // Ensure the workspace exists — when the daemon
                        // observes a virtual-desktop switch it tells us to
                        // switch to a workspace we may not have seen yet.
                        let _ = mon.ensure_workspace(target, outer_gap, inner_gap);
                        mon.active_workspace = target;
                        out.dirty_monitors.push(mon_id);
                    }
                }
            }
            Event::MoveToWorkspace { id: target } => {
                let outer_gap = self.config.outer_gap;
                let inner_gap = self.config.inner_gap;
                if let Some((mon_id, src_ws, focused)) = self.focus_context() {
                    if src_ws == target { return Ok(out); }
                    if let Some(mon) = self.monitor_mut(mon_id) {
                        let work_area = mon.work_area;
                        if let Some(src) = mon.workspaces.iter_mut().find(|w| w.id == src_ws) {
                            src.layout.remove(focused);
                            if src.focused == Some(focused) {
                                src.focused = src.layout.windows().last().copied();
                            }
                        }
                        let dst = mon.ensure_workspace(target, outer_gap, inner_gap);
                        let near = dst.focused;
                        dst.layout.insert(focused, near, work_area);
                        dst.focused = Some(focused);
                    }
                    self.locations.insert(focused, (mon_id, target));
                    out.dirty_monitors.push(mon_id);
                }
            }
            Event::MergeWindows { src, target } => {
                // Both windows must share a (monitor, workspace); we look
                // them up rather than trusting the caller, since hotkey UIs
                // can race against close events.
                let src_loc = self.locations.get(&src).copied();
                let tgt_loc = self.locations.get(&target).copied();
                if let (Some(sl), Some(tl)) = (src_loc, tgt_loc) {
                    if sl == tl && src != target {
                        let (mon_id, ws_id) = sl;
                        if let Some(mon) = self.monitor_mut(mon_id) {
                            if let Some(ws) = mon.workspaces.iter_mut().find(|w| w.id == ws_id) {
                                if ws.layout.merge(target, src) {
                                    // Active tab is the freshly-merged one,
                                    // so focus follows.
                                    ws.focused = Some(src);
                                }
                            }
                            out.dirty_monitors.push(mon_id);
                        }
                    }
                }
            }
            Event::UntabWindow => {
                if let Some((mon_id, ws_id, focused)) = self.focus_context() {
                    if let Some(mon) = self.monitor_mut(mon_id) {
                        let work_area = mon.work_area;
                        if let Some(ws) = mon.workspaces.iter_mut().find(|w| w.id == ws_id) {
                            if ws.layout.untab(focused, work_area) {
                                out.dirty_monitors.push(mon_id);
                            }
                        }
                    }
                }
            }
            Event::CycleTab { forward } => {
                if let Some((mon_id, ws_id, focused)) = self.focus_context() {
                    if let Some(mon) = self.monitor_mut(mon_id) {
                        if let Some(ws) = mon.workspaces.iter_mut().find(|w| w.id == ws_id) {
                            if let Some(next) = ws.layout.cycle_tab(focused, forward) {
                                ws.focused = Some(next);
                                out.dirty_monitors.push(mon_id);
                            }
                        }
                    }
                }
            }
            Event::DropAtEdge { src, target, edge } => {
                if src == target { return Ok(out); }
                let s_loc = self.locations.get(&src).copied();
                let t_loc = self.locations.get(&target).copied();
                if let (Some((s_mon, s_ws)), Some((t_mon, t_ws))) = (s_loc, t_loc) {
                    // Drop-at-edge currently only supports same-workspace
                    // moves. Cross-workspace drops would require us to
                    // translate the dragged window to the target's
                    // workspace, which collides with our per-VD model.
                    if s_mon != t_mon || s_ws != t_ws { return Ok(out); }
                    if let Some(mon) = self.monitor_mut(t_mon) {
                        if let Some(ws) = mon.workspaces.iter_mut().find(|w| w.id == t_ws) {
                            // Order matters: pull `src` out of the tree first
                            // (it might be the focused leaf or a tab member),
                            // then re-insert next to `target`.
                            let _ = ws.layout.remove(src);
                            let inserted = ws.layout.insert_beside(src, target, edge.axis(), edge.before());
                            if inserted {
                                ws.focused = Some(src);
                            }
                        }
                        out.dirty_monitors.push(t_mon);
                    }
                }
            }
            Event::ActivateTab { window } => {
                if let Some((mon_id, ws_id)) = self.locations.get(&window).copied() {
                    if let Some(mon) = self.monitor_mut(mon_id) {
                        if let Some(ws) = mon.workspaces.iter_mut().find(|w| w.id == ws_id) {
                            if let Some(root) = ws.layout.root.as_deref_mut() {
                                if crate::layout::activate_tab_in(root, window) {
                                    ws.focused = Some(window);
                                    out.dirty_monitors.push(mon_id);
                                }
                            }
                        }
                    }
                }
            }
            Event::Quit => out.quit = true,
        }
        out.dirty_monitors.sort();
        out.dirty_monitors.dedup();
        Ok(out)
    }

    fn focus_context(&self) -> Option<(MonitorId, WorkspaceId, WindowId)> {
        let mon = self.focused_monitor?;
        let m = self.monitor(mon)?;
        let ws = m.active_workspace;
        let focused = m.active().focused?;
        Some((mon, ws, focused))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::Workspace;

    fn fresh() -> State {
        let mut s = State::new(Config::default());
        let m = Monitor {
            id: MonitorId(1),
            bounds: Rect::new(0, 0, 1920, 1080),
            work_area: Rect::new(0, 40, 1920, 1000),
            dpi: 96,
            workspaces: vec![
                Workspace::new(WorkspaceId(1), 0, 0),
                Workspace::new(WorkspaceId(2), 0, 0),
            ],
            active_workspace: WorkspaceId(1),
        };
        s.apply(Event::MonitorAttached { monitor: m }).unwrap();
        s
    }

    #[test]
    fn open_then_close_clears_layout() {
        let mut s = fresh();
        let info = WindowInfo::new(WindowId(10), "Notepad", "Notepad");
        s.apply(Event::WindowOpened { info, monitor: MonitorId(1), workspace: WorkspaceId(1) }).unwrap();
        assert_eq!(s.plan_for(MonitorId(1)).unwrap().placements.len(), 1);
        s.apply(Event::WindowClosed { id: WindowId(10) }).unwrap();
        assert_eq!(s.plan_for(MonitorId(1)).unwrap().placements.len(), 0);
    }

    #[test]
    fn move_to_workspace_round_trip() {
        let mut s = fresh();
        let id = WindowId(7);
        s.apply(Event::WindowOpened {
            info: WindowInfo::new(id, "x", "X"),
            monitor: MonitorId(1),
            workspace: WorkspaceId(1),
        }).unwrap();
        s.apply(Event::MoveToWorkspace { id: WorkspaceId(2) }).unwrap();
        assert_eq!(s.locations[&id], (MonitorId(1), WorkspaceId(2)));
        // Active workspace is still #1, so plan should be empty
        assert_eq!(s.plan_for(MonitorId(1)).unwrap().placements.len(), 0);
        s.apply(Event::SwitchWorkspace { id: WorkspaceId(2) }).unwrap();
        assert_eq!(s.plan_for(MonitorId(1)).unwrap().placements.len(), 1);
    }

    /// Each "workspace" maps to a Windows virtual desktop. The state machine
    /// must lazily create them as the daemon discovers new VDs at runtime.
    #[test]
    fn windows_open_into_dynamic_workspaces() {
        let mut s = fresh();
        // Open into a workspace that wasn't pre-allocated.
        s.apply(Event::WindowOpened {
            info: WindowInfo::new(WindowId(1), "a", "A"),
            monitor: MonitorId(1),
            workspace: WorkspaceId(7),
        }).unwrap();
        // Active workspace is still #1, so the new window isn't visible yet.
        assert_eq!(s.plan_for(MonitorId(1)).unwrap().placements.len(), 0);
        // Switch to the dynamic workspace; the window appears.
        s.apply(Event::SwitchWorkspace { id: WorkspaceId(7) }).unwrap();
        assert_eq!(s.plan_for(MonitorId(1)).unwrap().placements.len(), 1);
    }

    /// Per-VD layout state must survive a round-trip through other VDs.
    #[test]
    fn per_workspace_layouts_are_independent() {
        let mut s = fresh();
        s.apply(Event::WindowOpened {
            info: WindowInfo::new(WindowId(1), "a", "A"),
            monitor: MonitorId(1),
            workspace: WorkspaceId(1),
        }).unwrap();
        s.apply(Event::WindowOpened {
            info: WindowInfo::new(WindowId(2), "b", "B"),
            monitor: MonitorId(1),
            workspace: WorkspaceId(2),
        }).unwrap();
        // ws#1 has only window 1; ws#2 has only window 2.
        assert_eq!(s.plan_for(MonitorId(1)).unwrap().placements.len(), 1);
        assert_eq!(s.plan_for(MonitorId(1)).unwrap().placements[0].window, WindowId(1));
        s.apply(Event::SwitchWorkspace { id: WorkspaceId(2) }).unwrap();
        assert_eq!(s.plan_for(MonitorId(1)).unwrap().placements.len(), 1);
        assert_eq!(s.plan_for(MonitorId(1)).unwrap().placements[0].window, WindowId(2));
        s.apply(Event::SwitchWorkspace { id: WorkspaceId(1) }).unwrap();
        assert_eq!(s.plan_for(MonitorId(1)).unwrap().placements[0].window, WindowId(1));
    }

    /// Open three windows on the same workspace and merge two of them into
    /// a tab group. The plan should drop to two placements (the lone leaf +
    /// the active tab) and the merged-in window should take focus.
    #[test]
    fn merge_windows_creates_tab_group() {
        let mut s = fresh();
        for id in [1u64, 2, 3] {
            s.apply(Event::WindowOpened {
                info: WindowInfo::new(WindowId(id), "x", "X"),
                monitor: MonitorId(1),
                workspace: WorkspaceId(1),
            }).unwrap();
        }
        assert_eq!(s.plan_for(MonitorId(1)).unwrap().placements.len(), 3);
        s.apply(Event::MergeWindows { src: WindowId(2), target: WindowId(3) }).unwrap();
        // Plan now: leaf #1 + both tabs of group {3, 2} stacked = 3.
        let plan = s.plan_for(MonitorId(1)).unwrap();
        assert_eq!(plan.placements.len(), 3);
        // src (the dragged window) becomes the active/focused tab.
        let mon = s.monitor(MonitorId(1)).unwrap();
        assert_eq!(mon.active().focused, Some(WindowId(2)));
    }

    #[test]
    fn cycle_tab_advances_focus() {
        let mut s = fresh();
        for id in [1u64, 2, 3] {
            s.apply(Event::WindowOpened {
                info: WindowInfo::new(WindowId(id), "x", "X"),
                monitor: MonitorId(1),
                workspace: WorkspaceId(1),
            }).unwrap();
        }
        // After the inserts, focus is on #3. Tab #2 and #3 together.
        s.apply(Event::MergeWindows { src: WindowId(2), target: WindowId(3) }).unwrap();
        // tabs = [3, 2], active = 2 (the dragged one).
        s.apply(Event::CycleTab { forward: true }).unwrap();
        assert_eq!(s.monitor(MonitorId(1)).unwrap().active().focused, Some(WindowId(3)));
        s.apply(Event::CycleTab { forward: true }).unwrap();
        assert_eq!(s.monitor(MonitorId(1)).unwrap().active().focused, Some(WindowId(2)));
    }

    #[test]
    fn untab_window_extracts_focused_tab() {
        let mut s = fresh();
        for id in [1u64, 2] {
            s.apply(Event::WindowOpened {
                info: WindowInfo::new(WindowId(id), "x", "X"),
                monitor: MonitorId(1),
                workspace: WorkspaceId(1),
            }).unwrap();
        }
        s.apply(Event::MergeWindows { src: WindowId(1), target: WindowId(2) }).unwrap();
        // After merge, both tabs are placed at the same cell rect = 2.
        assert_eq!(s.plan_for(MonitorId(1)).unwrap().placements.len(), 2);
        // Focus is on #1 (the dragged one); pop it back out.
        s.apply(Event::UntabWindow).unwrap();
        // Group collapses to a Leaf, both windows visible in their own cells.
        assert_eq!(s.plan_for(MonitorId(1)).unwrap().placements.len(), 2);
    }

    /// MergeWindows across different workspaces is rejected — the daemon
    /// is supposed to filter these out, but we double-check so a stray
    /// IPC call can't desync `locations` from the tree.
    #[test]
    fn merge_across_workspaces_is_rejected() {
        let mut s = fresh();
        s.apply(Event::WindowOpened {
            info: WindowInfo::new(WindowId(1), "a", "A"),
            monitor: MonitorId(1),
            workspace: WorkspaceId(1),
        }).unwrap();
        s.apply(Event::WindowOpened {
            info: WindowInfo::new(WindowId(2), "b", "B"),
            monitor: MonitorId(1),
            workspace: WorkspaceId(2),
        }).unwrap();
        s.apply(Event::MergeWindows { src: WindowId(1), target: WindowId(2) }).unwrap();
        // Each workspace still has its own single window — nothing was tabbed.
        assert_eq!(s.plan_for(MonitorId(1)).unwrap().placements.len(), 1);
        s.apply(Event::SwitchWorkspace { id: WorkspaceId(2) }).unwrap();
        assert_eq!(s.plan_for(MonitorId(1)).unwrap().placements.len(), 1);
    }

    /// Clicking a tab strip emits `ActivateTab`; the targeted tab becomes
    /// active and gets focus. Verifies the path the tab-strip overlay
    /// uses when the user clicks an inactive tab.
    #[test]
    fn activate_tab_makes_window_visible_and_focused() {
        let mut s = fresh();
        for id in [1u64, 2, 3] {
            s.apply(Event::WindowOpened {
                info: WindowInfo::new(WindowId(id), "x", "X"),
                monitor: MonitorId(1),
                workspace: WorkspaceId(1),
            }).unwrap();
        }
        s.apply(Event::MergeWindows { src: WindowId(1), target: WindowId(2) }).unwrap();
        s.apply(Event::MergeWindows { src: WindowId(3), target: WindowId(2) }).unwrap();
        // Group is {2, 1, 3}; #3 is currently active (last merged). All 3
        // tabs are in the plan (stacked at cell), with active emitted last.
        let placements = s.plan_for(MonitorId(1)).unwrap().placements;
        assert_eq!(placements.len(), 3);
        assert_eq!(placements.last().unwrap().window, WindowId(3),
            "active tab is emitted last so it Z-stacks on top");
        // Click tab for #2.
        s.apply(Event::ActivateTab { window: WindowId(2) }).unwrap();
        let placements = s.plan_for(MonitorId(1)).unwrap().placements;
        assert_eq!(placements.last().unwrap().window, WindowId(2),
            "newly-active tab moves to the last position");
        assert_eq!(s.monitor(MonitorId(1)).unwrap().active().focused, Some(WindowId(2)));
    }

    /// `WindowOpened` for an already-known window must NOT create a
    /// duplicate leaf in the BSP tree. Discover-tick races with the hook's
    /// EVENT_OBJECT_SHOW, and apps that briefly cloak/uncloak across VD
    /// transitions, can both deliver back-to-back open events for the
    /// same id. Earlier behavior double-inserted, corrupting the tree.
    #[test]
    fn window_opened_is_idempotent() {
        let mut s = fresh();
        let id = WindowId(42);
        let info = WindowInfo::new(id, "x", "X");
        s.apply(Event::WindowOpened { info: info.clone(), monitor: MonitorId(1), workspace: WorkspaceId(1) }).unwrap();
        // Fire it again — same window, same workspace. Should be a no-op.
        s.apply(Event::WindowOpened { info, monitor: MonitorId(1), workspace: WorkspaceId(1) }).unwrap();
        assert_eq!(s.plan_for(MonitorId(1)).unwrap().placements.len(), 1);
        let layout_windows = s.monitor(MonitorId(1)).unwrap().active().windows();
        assert_eq!(layout_windows, vec![id], "tree must contain exactly one leaf");
    }
}
