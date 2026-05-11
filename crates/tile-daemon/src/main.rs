//! TileManager daemon.
//!
//! One tokio runtime, one main loop. Three input streams converge into the
//! daemon:
//!
//!   1. WinEventHook (window lifecycle, focus changes) — `HookEvent`s
//!   2. HotkeyManager (global Win+key combos) — `Action`s
//!   3. IPC server (tilectl commands) — `Request`s
//!
//! HookEvents are translated into `tile_core::Event`s here, with each
//! window's Windows-virtual-desktop GUID resolved into a stable
//! `WorkspaceId` via the `VdMap`. That's how per-VD tiling works:
//! each Windows VD ↔ one of our internal workspaces, allocated lazily.

#![cfg_attr(not(windows), allow(dead_code, unused_imports))]

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use tile_core::config::{Action, ConfigFile};
use tile_core::ipc::{Request, Response};
use tile_core::state::{Event as CoreEvent, State};
use tile_core::workspace::WorkspaceId;
use tile_core::Direction;
use tokio::sync::mpsc;
use tracing::{info, warn};

#[cfg(windows)]
use tile_win::{
    applier::Applier, drop_zones::{DropZone, DropZoneManager}, hook::{EventHook, HookEvent},
    hwnd_map::HwndMap, ipc_server::serve as serve_ipc, keyboard_hook::KeyboardHook,
    manageable, monitors, tab_strip::{StripDescriptor, TabAction, TabStripManager},
    tray::{TrayCommand, TrayManager}, vdesktop,
};


#[cfg(windows)]
use windows::Win32::Foundation::HWND;

fn main() -> Result<()> {
    init_logging();
    #[cfg(windows)]
    {
        tile_win::dpi::declare_per_monitor_aware();
        tile_win::manageable::set_own_pid(std::process::id());
    }
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    rt.block_on(run())
}

fn init_logging() {
    use tracing_subscriber::{fmt, EnvFilter};
    let _ = fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,tile_=debug")))
        .with_target(false)
        .try_init();
}

#[cfg(not(windows))]
async fn run() -> Result<()> {
    anyhow::bail!("tile-daemon only runs on Windows");
}

/// Stable mapping from Windows-virtual-desktop GUID → our internal
/// `WorkspaceId`. We allocate fresh u16 ids on first encounter; the
/// daemon's lifetime is the only thing that matters since these don't
/// persist across runs.
#[cfg(windows)]
struct VdMap {
    next_id: u16,
    by_guid: HashMap<vdesktop::VdKey, WorkspaceId>,
}

#[cfg(windows)]
impl VdMap {
    fn new() -> Self { Self { next_id: 1, by_guid: HashMap::new() } }

    /// Translate a VD GUID to the workspace id our state machine knows.
    fn intern(&mut self, key: vdesktop::VdKey) -> WorkspaceId {
        if let Some(id) = self.by_guid.get(&key) { return *id; }
        let id = WorkspaceId(self.next_id);
        self.next_id = self.next_id.checked_add(1).expect("u16 of workspaces — way more than any user has VDs");
        self.by_guid.insert(key, id);
        info!(workspace = self.next_id - 1, "allocated workspace for new virtual desktop");
        id
    }

    /// Look up the workspace id for the given window's VD. Falls back to
    /// the workspace we last saw the foreground on (the user's "current"
    /// workspace) if the COM call fails.
    fn for_window(&mut self, hwnd: HWND, fallback: WorkspaceId) -> WorkspaceId {
        match vdesktop::window_desktop_key(hwnd) {
            Some(k) => self.intern(k),
            None    => fallback,
        }
    }
}

#[cfg(windows)]
async fn run() -> Result<()> {
    let cfg = load_config()?;
    let runtime_cfg = cfg.into_runtime();

    let mut state = State::new(runtime_cfg.clone());
    let mut vd_map = VdMap::new();

    // The daemon's notion of "current workspace": starts as the workspace
    // for the desktop that's foreground at launch. Updated whenever
    // EVENT_SYSTEM_FOREGROUND comes in for a window on a different VD.
    let mut current_ws: WorkspaceId = vdesktop::current_desktop_key()
        .map(|k| vd_map.intern(k))
        .unwrap_or(WorkspaceId(1));

    // Seed monitors from EnumDisplayMonitors. Their `active_workspace`
    // gets seeded to the VD we're starting on.
    for mut m in monitors::enumerate(runtime_cfg.outer_gap, runtime_cfg.inner_gap, runtime_cfg.workspaces_per_monitor) {
        m.active_workspace = current_ws;
        m.workspaces.clear(); // let state.apply() create the active one fresh
        state.apply(CoreEvent::MonitorAttached { monitor: m })
            .map_err(|e| anyhow::anyhow!("seed monitor: {e}"))?;
    }

    // Channels.
    let (hook_tx, mut hook_rx) = mpsc::unbounded_channel::<HookEvent>();
    let (action_tx, mut action_rx) = mpsc::unbounded_channel::<Action>();
    let (ipc_tx, mut ipc_rx) = mpsc::unbounded_channel::<(Request, tokio::sync::oneshot::Sender<Response>)>();

    // HWND map shared across hook + applier.
    let map = Arc::new(HwndMap::new());

    // Seed initial windows. We synthesise HookEvent::Opened so the same
    // VD-resolution logic runs as for events delivered live by the hook.
    {
        let map = map.clone();
        let hook_tx = hook_tx.clone();
        manageable::enumerate_manageable(move |hwnd| {
            let id = map.intern(hwnd);
            let title = manageable::title_of(hwnd);
            let class = manageable::class_of(hwnd);
            let mon = monitors::monitor_id_for_window(hwnd);
            info!(id = %id, monitor = %mon, title = %title, class = %class, "seed window");
            let info = tile_core::WindowInfo::new(id, title, class);
            let _ = hook_tx.send(HookEvent::Opened { raw_hwnd: hwnd.0 as isize, id, info });
        });
    }

    // Background services.
    let hook = EventHook::start(map.clone(), hook_tx.clone());
    // Switched from RegisterHotKey-based HotkeyManager to a low-level
    // keyboard hook. RegisterHotKey can't claim WIN+L, WIN+1..9, etc.
    // because Windows reserves them; the LL hook intercepts ahead of
    // Explorer and swallows matched keystrokes. Same Action stream.
    let hotkeys = KeyboardHook::start(action_tx);

    // Best-effort hotkey registration. Failures are warnings, not fatal —
    // a malformed keys string in user config shouldn't kill the daemon.
    for kb in &cfg.keybinds {
        if let Err(e) = hotkeys.register(&kb.keys, kb.action.clone()) {
            warn!(keys=%kb.keys, "hotkey registration failed: {e}");
        }
    }

    let applier = Applier::new(map.clone());

    // Tab-strip overlays. The manager owns one borderless window per
    // tab group on the active workspace and dispatches clicks back here
    // as `WindowId`s — translated into `Event::ActivateTab`.
    let (tab_click_tx, mut tab_click_rx) = mpsc::unbounded_channel::<TabAction>();
    let tab_strips = TabStripManager::start(tab_click_tx);

    // Drop-zone overlays: shown on every tile (except the dragged source)
    // while a drag is in flight, hidden as soon as it ends. Lets the user
    // *see* which cells will become tab targets — Aero Snap's built-in
    // preview gives no signal that we exist as a tiling WM.
    let drop_zones = DropZoneManager::start();

    // System tray icon. Right-click → Reload Config / About / Quit. The
    // tray is best-effort; if the icon fails to install we still want
    // the daemon to run, so we don't propagate errors from tray startup.
    let (tray_tx, mut tray_rx) = mpsc::unbounded_channel::<TrayCommand>();
    let tray = TrayManager::start(tray_tx);

    // Animator owns the *visible* layout state. Every layout-changing
    // event calls `animator.set_target(new_plan, now)`; a 60Hz tokio
    // interval pulls interpolated frames out via `animator.tick(now)`
    // and pushes them through the applier. Total animation time is
    // 140ms — long enough to feel smooth, short enough that the
    // user-perceived latency from "click → window arrives" is still
    // dominated by Win32 overhead, not us.
    let mut animator = tile_core::animator::Animator::new(140);

    // IPC server task.
    {
        let ipc_tx = ipc_tx.clone();
        tokio::spawn(async move { serve_ipc(ipc_tx).await; });
    }

    // Ctrl+C / SIGTERM-equivalent.
    let mut shutdown = Box::pin(tokio::signal::ctrl_c());

    // Initial paint of strips (in case seed enumeration produced any
    // tab groups via config restoration — currently it can't, but the
    // wiring is here so future config-from-disk works).
    refresh_tab_strips(&state, &tab_strips);

    // Periodic discovery sweep. Catches windows that:
    //   - Existed at startup but were cloaked on a different VD (the
    //     reason "tiling worked on startup but not on all desktops").
    //   - Opened after the hook was installed but `EVENT_OBJECT_CREATE`
    //     fired before their class/owner were set, so `is_manageable`
    //     rejected them and `EVENT_OBJECT_SHOW` never fired.
    // 2 seconds is fast enough to feel "automatic" without burning CPU.
    let mut discover_tick = tokio::time::interval(std::time::Duration::from_secs(2));
    discover_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // 60Hz frame tick to drive the animator. We use 16ms — close enough
    // to a refresh-rate cadence to feel smooth, and Tokio's interval
    // tolerates DWM jitter without piling up a backlog (Skip mode).
    let mut frame_tick = tokio::time::interval(std::time::Duration::from_millis(16));
    frame_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // 50ms cursor poll, only active during a drag. Lets us repaint the
    // drop-zone overlay so the region the cursor is *currently* hovering
    // gets the bright "hot" fill — users can see where their drop will
    // land before they release. Set to `Some(src)` on DragStarted, back
    // to `None` on DragEnded, so the `if` guard in the select skips the
    // poll the rest of the time.
    let mut cursor_tick = tokio::time::interval(std::time::Duration::from_millis(50));
    cursor_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // 250ms reconcile tick. Runs when nothing is animating AND no drag
    // is in flight, to catch windows that drifted out of their tile cell
    // without firing any event we'd normally hook. Two common cases:
    //   1. User exited fullscreen via Esc/F11 — the app restored its
    //      rect via internal SetWindowPos and we never saw an event.
    //   2. Some apps (Slack-in-tray, OBS, others) re-assert their own
    //      position after we tile them.
    // The reconcile pulls them back into the layout within ~250ms.
    // Cheap (Win32 SetWindowPos to current rect is essentially free).
    let mut reconcile_tick = tokio::time::interval(std::time::Duration::from_millis(250));
    reconcile_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut dragging_src: Option<tile_core::WindowId> = None;

    loop {
        tokio::select! {
            ev = hook_rx.recv() => {
                let Some(ev) = ev else { break };
                // Track drag-state for the cursor-poll tick. We do this in
                // the loop rather than inside handle_hook_event to keep the
                // mutable `dragging_src` local to this scope.
                match &ev {
                    HookEvent::DragStarted { src } => dragging_src = Some(*src),
                    HookEvent::DragEnded { .. } => dragging_src = None,
                    _ => {}
                }
                let vd_changed = handle_hook_event(
                    &mut state, &applier, &mut vd_map, &mut current_ws, &drop_zones,
                    &mut animator, &map, ev,
                );
                if vd_changed {
                    // After a VD switch, the windows on the new desktop just
                    // uncloaked. Sweep them in immediately rather than waiting
                    // for the periodic tick — the user expects instant tiling.
                    discover_windows(&mut state, &applier, &mut vd_map, current_ws, &map, &mut animator);
                }
                refresh_tab_strips(&state, &tab_strips);
            }
            act = action_rx.recv() => {
                let Some(act) = act else { break };
                if let Some(ev) = action_to_event(act) {
                    handle_event(&mut state, &applier, &mut animator, &map, ev);
                    refresh_tab_strips(&state, &tab_strips);
                }
            }
            req = ipc_rx.recv() => {
                let Some((req, reply)) = req else { break };
                let resp = handle_ipc(&mut state, &applier, &mut animator, &map, req);
                let _ = reply.send(resp);
                refresh_tab_strips(&state, &tab_strips);
            }
            click = tab_click_rx.recv() => {
                let Some(action) = click else { break };
                match action {
                    TabAction::Activate(window) => {
                        handle_event(&mut state, &applier, &mut animator, &map, CoreEvent::ActivateTab { window });
                        // Bring the now-active tab to the foreground so keyboard input
                        // routes to it.
                        applier.focus(window);
                    }
                    TabAction::Close(window) => {
                        applier.close(window);
                    }
                }
                refresh_tab_strips(&state, &tab_strips);
            }
            _ = discover_tick.tick() => {
                // Two diffs on the same cadence: catches added/removed
                // monitors AND windows we missed via the WinEventHook.
                discover_monitors(&mut state, &applier, &mut animator, &map);
                discover_windows(&mut state, &applier, &mut vd_map, current_ws, &map, &mut animator);
                refresh_tab_strips(&state, &tab_strips);
            }
            cmd = tray_rx.recv() => {
                let Some(cmd) = cmd else { break };
                match cmd {
                    TrayCommand::ReloadConfig => {
                        info!("tray: reload config");
                        // Stub: a real reload would re-parse config + rebuild
                        // hotkeys. We at least log intent so the user knows
                        // their click registered.
                    }
                    TrayCommand::About => {
                        info!("tray: about (W11 Tiles v{})", env!("CARGO_PKG_VERSION"));
                    }
                    TrayCommand::Shortcuts => {
                        info!("tray: show keyboard shortcuts");
                        // MessageBoxW blocks the calling thread until the
                        // user dismisses it — spawn a regular OS thread
                        // so the daemon's tokio runtime keeps pumping.
                        std::thread::spawn(|| {
                            tile_win::shortcuts_dialog::show();
                        });
                    }
                    TrayCommand::Quit => {
                        info!("tray: quit requested");
                        break;
                    }
                }
            }
            _ = cursor_tick.tick(), if dragging_src.is_some() => {
                let src = dragging_src.unwrap();
                let (cx, cy) = unsafe {
                    let mut pt = windows::Win32::Foundation::POINT::default();
                    let _ = windows::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut pt);
                    (pt.x, pt.y)
                };
                let zones = build_hot_drop_zones(&state, &map, cx, cy, src);
                drop_zones.show(zones);
            }
            _ = frame_tick.tick(), if animator.is_animating() => {
                // The `if` guard skips the tick entirely when there's no
                // animation in flight — Tokio's interval still ticks, but
                // we don't enter the branch, so no syscall, no allocation,
                // no applier call. Idle CPU goes from "60Hz no-op poll"
                // to "wake up only when there's a layout change".
                if let Some(frame) = animator.tick(std::time::Instant::now()) {
                    let _ = applier.apply(&frame);
                }
            }
            _ = reconcile_tick.tick(), if !animator.is_animating() && dragging_src.is_none() => {
                // Snap-back any window that drifted. The drag-guard is
                // critical: without it, the reconcile would fight a
                // user's active drag by repeatedly snapping the window
                // back to its tile cell mid-drag. We only reconcile when
                // both (a) no tween is in flight and (b) the user isn't
                // dragging anything.
                let _ = applier.apply(&combined_plan(&state, &map));
            }
            _ = &mut shutdown => {
                info!("shutdown signal received");
                break;
            }
        }
    }

    info!("daemon exiting");
    hook.stop();
    hotkeys.stop();
    tab_strips.stop();
    drop_zones.stop();
    tray.stop();
    Ok(())
}

/// Build the full visible plan across every monitor's active workspace.
/// The animator wants a single combined plan (not one per monitor) so it
/// can correctly identify "closed" windows by absence — if monitor 2's
/// plan is omitted, the animator would think every monitor-2 window
/// closed and snap them away.
#[cfg(windows)]
fn combined_plan(state: &State, map: &Arc<HwndMap>) -> tile_core::LayoutPlan {
    let mut plan = tile_core::LayoutPlan::default();
    for mon in &state.monitors {
        let Some(p) = state.plan_for(mon.id) else { continue };
        // Solo-fullscreen gate. If this monitor has exactly one placement
        // AND the underlying HWND is currently fullscreen (game, video,
        // slideshow), leave it alone — applying our cell rect would
        // shrink it to work_area-minus-gaps and break the user's
        // intentional fullscreen state. As soon as a second window joins
        // the monitor, both ends up in the plan and the fullscreen
        // window gets SetWindowPos'd into its tile cell, exiting
        // fullscreen automatically.
        if p.placements.len() == 1 {
            if let Some(hwnd) = map.lookup_hwnd(p.placements[0].window) {
                if manageable::is_fullscreen(hwnd) {
                    continue;
                }
            }
        }
        plan.placements.extend(p.placements);
    }
    plan
}

/// Compute the tab-group descriptors for the *currently active* workspace
/// on every monitor, then push them to the overlay manager. We only paint
/// strips for visible windows; off-workspace tab groups stay dormant.
#[cfg(windows)]
fn refresh_tab_strips(state: &State, mgr: &TabStripManager) {
    let mut strips: Vec<StripDescriptor> = Vec::new();
    for mon in &state.monitors {
        let active = mon.active();
        for view in active.layout.tab_groups(mon.work_area) {
            let s = StripDescriptor::from_view(&view, |id| {
                // Title fallback chain: real title → class name → "win#N".
                // Modal-style apps (settings panes, dialogs) sometimes have
                // empty titles; their class name is at least informative.
                if let Some(info) = state.windows.get(&id) {
                    if !info.title.trim().is_empty() {
                        return info.title.clone();
                    }
                    if !info.class.trim().is_empty() {
                        return info.class.clone();
                    }
                }
                format!("win#{}", id.0)
            });
            strips.push(s);
        }
    }
    mgr.update(strips);
}

/// Walk all currently-visible top-level windows and open ones we haven't
/// seen yet into the right workspace based on their VD GUID. Cheap (Win32
/// `EnumWindows` over a few hundred HWNDs); idempotent because the
/// `windows`-map check skips already-known ids.
///
/// Called on VD switch (immediate fix for "windows on the new desktop
/// don't tile") and on a 2-second tick (catches any window whose
/// `CREATE`/`SHOW` event we missed).
#[cfg(windows)]
fn discover_windows(
    state: &mut State,
    applier: &Applier,
    vd_map: &mut VdMap,
    current_ws: WorkspaceId,
    map: &Arc<HwndMap>,
    animator: &mut tile_core::animator::Animator,
) {
    let mut hwnds: Vec<isize> = Vec::new();
    manageable::enumerate_manageable(|hwnd| {
        hwnds.push(hwnd.0 as isize);
    });
    for raw in hwnds {
        let hwnd = HWND(raw as *mut _);
        let id = map.intern(hwnd);
        if state.windows.contains_key(&id) { continue; }
        let title = manageable::title_of(hwnd);
        let class = manageable::class_of(hwnd);
        let monitor = monitors::monitor_id_for_window(hwnd);
        let workspace = vd_map.for_window(hwnd, current_ws);
        info!(
            id = %id, monitor = %monitor, workspace = ?workspace,
            title = %title, "discovered window"
        );
        let info = tile_core::WindowInfo::new(id, title, class);
        handle_event(state, applier, animator, map, CoreEvent::WindowOpened { info, monitor, workspace });
    }
}

/// Re-enumerate monitors and reconcile against the daemon's view of the
/// world. Catches:
///   - Plugged-in monitor                       → `MonitorAttached`
///   - Unplugged monitor                        → `MonitorDetached`
///   - Resolution change / DPI change / rotation → `MonitorReshaped`
///
/// We don't react to `WM_DISPLAYCHANGE` directly (would need a hidden
/// message-only window for that). Polling on the same 2s tick is robust
/// and cheap — `EnumDisplayMonitors` is microseconds for typical desktops.
#[cfg(windows)]
fn discover_monitors(
    state: &mut State,
    applier: &Applier,
    animator: &mut tile_core::animator::Animator,
    map: &Arc<HwndMap>,
) {
    let cfg = state.config.clone();
    let fresh = monitors::enumerate(cfg.outer_gap, cfg.inner_gap, cfg.workspaces_per_monitor);

    // Diff: fresh vs. state.monitors.
    let mut existing_ids: std::collections::HashSet<tile_core::state::MonitorId> =
        state.monitors.iter().map(|m| m.id).collect();

    for new_mon in &fresh {
        existing_ids.remove(&new_mon.id);
        if let Some(cur) = state.monitor(new_mon.id) {
            // Already tracked: only fire reshape if anything changed.
            if cur.bounds != new_mon.bounds
                || cur.work_area != new_mon.work_area
                || cur.dpi != new_mon.dpi
            {
                info!(
                    id = %new_mon.id, bounds = ?new_mon.bounds, dpi = new_mon.dpi,
                    "monitor reshape detected"
                );
                handle_event(state, applier, animator, map, CoreEvent::MonitorReshaped {
                    id: new_mon.id,
                    bounds: new_mon.bounds,
                    work_area: new_mon.work_area,
                    dpi: new_mon.dpi,
                });
            }
        } else {
            info!(id = %new_mon.id, "new monitor attached");
            handle_event(state, applier, animator, map, CoreEvent::MonitorAttached {
                monitor: new_mon.clone(),
            });
        }
    }

    // Anything still in `existing_ids` was unplugged.
    for gone in existing_ids {
        info!(id = %gone, "monitor detached");
        handle_event(state, applier, animator, map, CoreEvent::MonitorDetached { id: gone });
    }
}

/// Returns `true` if the event triggered a virtual-desktop switch — the
/// caller uses that signal to run an immediate window-discovery sweep,
/// since the windows on the new desktop just uncloaked and may not have
/// fired their own `EVENT_OBJECT_SHOW` events yet.
#[cfg(windows)]
fn handle_hook_event(
    state: &mut State,
    applier: &Applier,
    vd_map: &mut VdMap,
    current_ws: &mut WorkspaceId,
    drop_zones: &DropZoneManager,
    animator: &mut tile_core::animator::Animator,
    map: &Arc<HwndMap>,
    ev: HookEvent,
) -> bool {
    let mut vd_changed = false;
    match ev {
        HookEvent::Opened { raw_hwnd, id: _, info } => {
            let hwnd = HWND(raw_hwnd as *mut _);
            let workspace = vd_map.for_window(hwnd, *current_ws);
            // Route to the monitor the window currently lives on instead
            // of the previous hardcoded MonitorId(1) — without this every
            // window crowded the primary on multi-monitor rigs.
            let monitor = monitors::monitor_id_for_window(hwnd);
            handle_event(state, applier, animator, map,CoreEvent::WindowOpened {
                info, monitor, workspace,
            });
        }
        HookEvent::Closed { id } => {
            handle_event(state, applier, animator, map,CoreEvent::WindowClosed { id });
        }
        HookEvent::Focused { raw_hwnd, id } => {
            // Detect a VD switch on focus change. If the foreground window's
            // VD differs from our `current_ws`, the user just switched
            // desktops via Ctrl+Win+Arrow (or their preferred shortcut).
            let hwnd = HWND(raw_hwnd as *mut _);
            let ws = vd_map.for_window(hwnd, *current_ws);
            if ws != *current_ws {
                info!(from=?*current_ws, to=?ws, "virtual desktop changed");
                *current_ws = ws;
                vd_changed = true;
                // Tell the state machine to flip to the new workspace.
                handle_event(state, applier, animator, map,CoreEvent::SwitchWorkspace { id: ws });
            }
            handle_event(state, applier, animator, map,CoreEvent::WindowFocused { id });
        }
        HookEvent::Floated { id } => {
            handle_event(state, applier, animator, map,CoreEvent::WindowFloated { id });
        }
        HookEvent::Restored { raw_hwnd: _, id } => {
            handle_event(state, applier, animator, map,CoreEvent::WindowFocused { id });
        }
        HookEvent::DragStarted { src } => {
            // Compute drop-zones for every visible tile except `src`.
            // Initial state: no zone is hot — the cursor-poll tick will
            // start updating that within 50ms once the drag is active.
            let mut zones: Vec<DropZone> = Vec::new();
            for mon in &state.monitors {
                let plan = mon.active().compute(mon.work_area);
                for p in &plan.placements {
                    if p.window != src {
                        zones.push(DropZone { rect: p.rect, hot: tile_win::drop_zones::HotZone::None });
                    }
                }
            }
            drop_zones.show(zones);
        }
        HookEvent::DragEnded { src, cursor_x, cursor_y } => {
            drop_zones.hide();
            // Cursor-based hit test against the active layout. The cursor
            // can fall into one of three buckets:
            //   1. Center 1/3-by-1/3 of a different tile  → MergeWindows (tab)
            //   2. Edge thirds of a different tile        → DropAtEdge (tile)
            //   3. Anywhere else (blank, src's own cell)  → Float
            // The drop_zones overlay shows all five regions so the user
            // can aim deliberately.
            let target = find_drop_target(state, map, cursor_x, cursor_y, src);
            match target {
                Some(DropTarget::Tile { window: target_id, kind: DropZoneKind::Center }) => {
                    info!(src=%src, target=%target_id, cursor=?(cursor_x, cursor_y), "drag-merge (tab)");
                    handle_event(state, applier, animator, map,CoreEvent::MergeWindows { src, target: target_id });
                }
                Some(DropTarget::Tile { window: target_id, kind: DropZoneKind::Edge(edge) }) => {
                    info!(src=%src, target=%target_id, ?edge, "drag-tile (split)");
                    handle_event(state, applier, animator, map,CoreEvent::DropAtEdge { src, target: target_id, edge });
                }
                Some(DropTarget::Monitor { id: target_mon }) => {
                    info!(src=%src, monitor=%target_mon, "drag to different monitor — moving");
                    handle_event(state, applier, animator, map, CoreEvent::MoveWindowToMonitor {
                        window: src, monitor: target_mon,
                    });
                }
                None => {
                    // Tiles are sticky. Drag-release on blank space — or
                    // on the window's own cell — never floats. The window
                    // snaps back to its existing tile slot. The only way
                    // to take a window OUT of the layout is the explicit
                    // SUPER+ALT+SPACE keybind (Action::ToggleFloat).
                    // Tab-group members get the same treatment for the
                    // same reason — popping a tab out is the SUPER+ALT+U
                    // keybind, not an accidental drag.
                    info!(src=%src, cursor=?(cursor_x, cursor_y), "drag ended on blank/self — snap back");
                    animator.set_target(combined_plan(state, map), std::time::Instant::now());
                }
            }
        }
    }
    vd_changed
}

/// Build the drop-zone descriptor list for the cursor-poll tick. For
/// each visible tile (other than the dragged source) we set the `hot`
/// field on the zone whose tile contains the cursor, with the kind
/// matching the cursor's sub-region. Other zones get `HotZone::None`.
/// Drop_zones diffs by `(rect, hot)` so painted overlays only repaint
/// when the cursor actually crosses a region boundary.
#[cfg(windows)]
fn build_hot_drop_zones(
    state: &State,
    map: &Arc<HwndMap>,
    cursor_x: i32,
    cursor_y: i32,
    src: tile_core::WindowId,
) -> Vec<DropZone> {
    let target = find_drop_target(state, map, cursor_x, cursor_y, src);
    let mut zones: Vec<DropZone> = Vec::new();
    for mon in &state.monitors {
        let plan = mon.active().compute(mon.work_area);
        for p in &plan.placements {
            if p.window == src { continue; }
            if !is_placement_visible(map, p.window) { continue; }
            let hot = match target {
                Some(DropTarget::Tile { window: id, kind }) if id == p.window => match kind {
                    DropZoneKind::Center                       => tile_win::drop_zones::HotZone::Center,
                    DropZoneKind::Edge(tile_core::Edge::Top)    => tile_win::drop_zones::HotZone::Top,
                    DropZoneKind::Edge(tile_core::Edge::Bottom) => tile_win::drop_zones::HotZone::Bottom,
                    DropZoneKind::Edge(tile_core::Edge::Left)   => tile_win::drop_zones::HotZone::Left,
                    DropZoneKind::Edge(tile_core::Edge::Right)  => tile_win::drop_zones::HotZone::Right,
                },
                _ => tile_win::drop_zones::HotZone::None,
            };
            zones.push(DropZone { rect: p.rect, hot });
        }
    }
    zones
}

/// Which sub-region of a tile the user dropped into. Mirrors the visual
/// layout of the drop-zone overlay: 3×3 grid where the middle cell is a
/// "merge as tab" target and the edge cells are split-and-tile targets.
#[cfg(windows)]
#[derive(Debug, Clone, Copy)]
enum DropZoneKind {
    Center,
    Edge(tile_core::Edge),
}

/// Cell hit-test: which tile + which sub-zone of that tile contains the
/// True if the underlying HWND for a placement is *currently visible*
/// to the user — i.e., not iconic (minimized) and not DWM-cloaked
/// (suspended UWP, mid-VD-transition, etc.). Used to filter the
/// drop-zone overlay and cursor hit-test so we don't render targets
/// for windows the user can't see on screen.
///
/// We keep the placement in the BSP tree either way — the window will
/// snap right back into its cell when it un-minimizes or un-cloaks.
/// We just don't *show* the drop affordance for it.
#[cfg(windows)]
fn is_placement_visible(map: &Arc<HwndMap>, window: tile_core::WindowId) -> bool {
    let Some(hwnd) = map.lookup_hwnd(window) else { return false };
    unsafe {
        if windows::Win32::UI::WindowsAndMessaging::IsIconic(hwnd).as_bool() {
            return false;
        }
    }
    if manageable::is_cloaked(hwnd) {
        return false;
    }
    true
}

/// Result of cursor hit-testing a drag-end against the current layout.
#[cfg(windows)]
#[derive(Debug, Clone, Copy)]
enum DropTarget {
    /// Cursor landed on a specific tile's sub-zone — merge or split.
    Tile { window: tile_core::WindowId, kind: DropZoneKind },
    /// Cursor landed on a *different* monitor than the source — move
    /// the dragged window to that monitor's active workspace. Hits the
    /// case where the user drags across screens onto blank area, or
    /// onto a tile on a different monitor that we treat as "place this
    /// over there" rather than "merge with that specific window".
    Monitor { id: tile_core::state::MonitorId },
}

/// cursor? Returns `None` only when the cursor is on the source's own
/// monitor and not on a tile — that case snaps back. Cross-monitor
/// drags always produce *some* target (either a tile or the destination
/// monitor itself).
#[cfg(windows)]
fn find_drop_target(
    state: &State,
    map: &Arc<HwndMap>,
    x: i32,
    y: i32,
    exclude: tile_core::WindowId,
) -> Option<DropTarget> {
    let src_mon = state.locations.get(&exclude).map(|(m, _)| *m);
    for mon in &state.monitors {
        if !mon.bounds.contains_point(x, y) { continue; }
        let plan = mon.active().compute(mon.work_area);
        for p in &plan.placements {
            if p.window == exclude { continue; }
            // Skip placements whose underlying window is currently
            // invisible to the user (minimized to taskbar, or cloaked
            // by DWM e.g. suspended UWP). The window still has a slot
            // in the BSP tree — we'll restore it cleanly when it
            // un-minimizes — but rendering a hit-target for a tile
            // the user can't actually see makes drop zones lie.
            if !is_placement_visible(map, p.window) { continue; }
            if !p.rect.contains_point(x, y) { continue; }
            // Same 3×3 division as drop_zones renders: left column =
            // LEFT, right column = RIGHT, top/bottom rows of the middle
            // column = TOP/BOTTOM, dead center = CENTER.
            let col = ((x - p.rect.x) * 3 / p.rect.width.max(1)).clamp(0, 2);
            let row = ((y - p.rect.y) * 3 / p.rect.height.max(1)).clamp(0, 2);
            let kind = match (col, row) {
                (1, 1) => DropZoneKind::Center,
                (0, _) => DropZoneKind::Edge(tile_core::Edge::Left),
                (2, _) => DropZoneKind::Edge(tile_core::Edge::Right),
                (1, 0) => DropZoneKind::Edge(tile_core::Edge::Top),
                (1, 2) => DropZoneKind::Edge(tile_core::Edge::Bottom),
                _      => DropZoneKind::Center, // unreachable, satisfies match
            };
            return Some(DropTarget::Tile { window: p.window, kind });
        }
        // Cursor is on this monitor but not on any tile. Two cases:
        //   1. Different monitor than source → user wants the window
        //      moved over here. Return Monitor.
        //   2. Same monitor as source → user wanted to drop on a tile
        //      but missed. Snap-back (return None).
        if src_mon != Some(mon.id) {
            return Some(DropTarget::Monitor { id: mon.id });
        }
        return None;
    }
    None
}

#[cfg(windows)]
fn handle_event(
    state: &mut State,
    applier: &Applier,
    animator: &mut tile_core::animator::Animator,
    map: &Arc<HwndMap>,
    ev: CoreEvent,
) {
    // Only PROACTIVE focus changes should re-call SetForegroundWindow.
    // `WindowFocused` is reactive — Windows already changed focus and is
    // notifying us — so re-asserting focus would steal it back if the
    // newly-focused HWND is one the daemon doesn't track.
    let proactive_focus = matches!(
        ev,
        CoreEvent::FocusDirection { .. } | CoreEvent::SwapDirection { .. }
    );
    match state.apply(ev) {
        Ok(out) => {
            // After any state change, hand the *full* combined plan
            // (every monitor's active workspace) to the animator. The
            // animator owns the in-flight tween; the 60Hz tokio tick
            // consumes its frames and pushes them through the applier.
            if !out.dirty_monitors.is_empty() {
                animator.set_target(combined_plan(state, map), std::time::Instant::now());
            }
            if proactive_focus {
                if let Some(focused) = state.focused_monitor
                    .and_then(|m| state.monitor(m))
                    .and_then(|m| m.active().focused)
                {
                    applier.focus(focused);
                }
            }
        }
        Err(e) => warn!("state.apply error: {e}"),
    }
}

#[cfg(windows)]
fn handle_ipc(state: &mut State, applier: &Applier, animator: &mut tile_core::animator::Animator, map: &Arc<HwndMap>, req: Request) -> Response {
    let ev = match req {
        Request::Ping => return Response::Pong { version: env!("CARGO_PKG_VERSION").into() },
        Request::ReloadConfig => {
            // Real reload would re-register hotkeys + rebuild rules; stub for now.
            return Response::Ok;
        }
        Request::Dump => {
            let json = serde_json::to_string_pretty(state).unwrap_or_else(|_| "{}".into());
            return Response::State { json };
        }
        Request::FocusDirection  { dir } => CoreEvent::FocusDirection  { dir },
        Request::SwapDirection   { dir } => CoreEvent::SwapDirection   { dir },
        Request::ResizeDirection { dir, delta } => CoreEvent::ResizeDirection { dir, delta },
        Request::ToggleFloat              => CoreEvent::ToggleFloat,
        Request::SwitchWorkspace { id }   => CoreEvent::SwitchWorkspace { id },
        Request::MoveToWorkspace { id }   => CoreEvent::MoveToWorkspace { id },
        Request::UntabWindow              => CoreEvent::UntabWindow,
        Request::CycleTab { forward }     => CoreEvent::CycleTab { forward },
        Request::ActivateTab { window }   => CoreEvent::ActivateTab { window },
        Request::Quit                     => CoreEvent::Quit,
    };
    handle_event(state, applier, animator, map,ev);
    Response::Ok
}

fn action_to_event(action: Action) -> Option<CoreEvent> {
    Some(match action {
        Action::FocusDirection  { dir }            => CoreEvent::FocusDirection  { dir },
        Action::SwapDirection   { dir }            => CoreEvent::SwapDirection   { dir },
        Action::ResizeDirection { dir, delta }     => CoreEvent::ResizeDirection { dir, delta },
        Action::ToggleFloat                        => CoreEvent::ToggleFloat,
        Action::SwitchWorkspace { id }             => CoreEvent::SwitchWorkspace { id },
        Action::MoveToWorkspace { id }             => CoreEvent::MoveToWorkspace { id },
        Action::UntabWindow                        => CoreEvent::UntabWindow,
        Action::CycleTab { forward }               => CoreEvent::CycleTab { forward },
        Action::Quit                               => CoreEvent::Quit,
        Action::Spawn { command }                  => {
            #[cfg(windows)]
            {
                let _ = std::process::Command::new("cmd").args(["/C", &command]).spawn();
            }
            return None;
        }
    })
}

fn load_config() -> Result<ConfigFile> {
    let dir = std::env::var("APPDATA").unwrap_or_default();
    let cfg_dir = std::path::Path::new(&dir).join("TileManager");
    let path = cfg_dir.join("config.toml");
    if !path.exists() {
        // First run: drop a starter config so the user has something to
        // edit instead of hunting for hidden defaults.
        if let Err(e) = std::fs::create_dir_all(&cfg_dir) {
            warn!(path=%cfg_dir.display(), "couldn't create config dir: {e}");
            return Ok(ConfigFile::default());
        }
        let cfg = ConfigFile::default();
        match toml::to_string_pretty(&cfg) {
            Ok(body) => {
                let header = "# TileManager config. Reload with `tilectl reload`.\n\
                              # See https://github.com/anthropics/tile-manager (forthcoming) for docs.\n\
                              # Default keybinds use SUPER+ALT (Windows-key + Alt) as the prefix —\n\
                              # the low-level keyboard hook can claim any chord, even ones\n\
                              # Windows reserves like SUPER+L. Edit `keybinds` below freely.\n\n";
                if let Err(e) = std::fs::write(&path, format!("{header}{body}")) {
                    warn!(path=%path.display(), "couldn't write starter config: {e}");
                } else {
                    info!(path=%path.display(), "wrote starter config");
                }
            }
            Err(e) => warn!("couldn't serialize default config: {e}"),
        }
        return Ok(cfg);
    }
    let raw = std::fs::read_to_string(&path).context("read config")?;
    toml::from_str(&raw).context("parse config")
}

#[allow(dead_code)]
fn _direction_used(_d: Direction) {}
