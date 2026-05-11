use serde::{Deserialize, Serialize};

use crate::geom::Rect;
use crate::layout::{LayoutPlan, LayoutTree};
use crate::window::WindowId;

/// Workspaces are scoped per-monitor (Hyprland model), not global
/// (i3 model). Per-monitor matches how Windows users already think
/// about virtual desktops + multi-monitor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct WorkspaceId(pub u16);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workspace {
    pub id: WorkspaceId,
    pub layout: LayoutTree,
    pub focused: Option<WindowId>,
}

impl Workspace {
    pub fn new(id: WorkspaceId, outer_gap: i32, inner_gap: i32) -> Self {
        Self {
            id,
            layout: LayoutTree::new(outer_gap, inner_gap),
            focused: None,
        }
    }

    pub fn compute(&self, work_area: Rect) -> LayoutPlan {
        self.layout.compute(work_area)
    }

    pub fn windows(&self) -> Vec<WindowId> { self.layout.windows() }
}
