//! BSP (binary space partition) layout tree.
//!
//! Every interior node is a `Split` (horizontal or vertical, with a ratio).
//! Every leaf holds a single [`WindowId`]. Inserting a window splits the
//! current focus leaf; removing a window collapses its parent split.
//!
//! Why BSP and not master/stack: pop-shell users coming from i3/Hyprland
//! expect "split where I'm pointing." Master/stack imposes a layout the
//! user has to fight; BSP follows the user's focus naturally. We can
//! always add an alternate stack engine later behind the same trait.
//!
//! `Tabbed` nodes layer an i3/Hyprland-style tab strip on top of BSP:
//! several windows share a single cell, with only the active tab made
//! visible. We deliberately keep tab groups as a *node kind* rather than
//! a separate sidecar so navigation, resize, and swap walk one tree.

use serde::{Deserialize, Serialize};

use crate::direction::Direction;
use crate::geom::Rect;
use crate::window::WindowId;

/// Reserved vertical pixels at the top of a `Tabbed` cell for the tab
/// strip the renderer paints. Layout shrinks the active tab's rect by
/// this much so client area never sits under the strip.
pub const TAB_STRIP_HEIGHT: i32 = 28;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SplitAxis { Horizontal, Vertical }

/// Which side of an existing tile a dragged window should land on. Drives
/// `LayoutTree::insert_beside` and `Event::DropAtEdge`. The drop-zone
/// overlay surfaces five regions per tile (one per `Edge`, plus a center
/// for "merge as tab").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Edge { Top, Bottom, Left, Right }

impl Edge {
    pub fn axis(self) -> SplitAxis {
        match self {
            Edge::Left | Edge::Right  => SplitAxis::Horizontal,
            Edge::Top  | Edge::Bottom => SplitAxis::Vertical,
        }
    }
    /// Whether the dragged window goes on the *first* (left/top) side of
    /// the resulting split.
    pub fn before(self) -> bool { matches!(self, Edge::Left | Edge::Top) }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Node {
    Leaf(WindowId),
    Split { axis: SplitAxis, ratio: f32, left: Box<Node>, right: Box<Node> },
    /// A stack of windows sharing one cell. Only `tabs[active]` is laid
    /// out; the rest are left at their previous on-screen position by the
    /// applier (cheaper than re-issuing `SetWindowPos` every frame).
    Tabbed { tabs: Vec<WindowId>, active: usize },
}

impl Node {
    fn leaf(id: WindowId) -> Box<Node> { Box::new(Node::Leaf(id)) }

    /// Walks the tree and pushes every leaf in left→right traversal order.
    pub fn collect_leaves(&self, out: &mut Vec<WindowId>) {
        match self {
            Node::Leaf(id) => out.push(*id),
            Node::Split { left, right, .. } => {
                left.collect_leaves(out);
                right.collect_leaves(out);
            }
            Node::Tabbed { tabs, .. } => out.extend(tabs.iter().copied()),
        }
    }

    pub fn contains(&self, target: WindowId) -> bool {
        match self {
            Node::Leaf(id) => *id == target,
            Node::Split { left, right, .. } => left.contains(target) || right.contains(target),
            Node::Tabbed { tabs, .. } => tabs.contains(&target),
        }
    }
}

/// Concrete placement of a single window after the layout has run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Placement {
    pub window: WindowId,
    pub rect: Rect,
}

/// Computed plan for an entire workspace — the daemon hands this to the
/// applier which translates it into `SetWindowPos` calls.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LayoutPlan {
    pub placements: Vec<Placement>,
}

/// Snapshot of one `Tabbed` node ready to feed the renderer. `cell` is
/// the *full* tabbed-cell rect — the renderer paints the strip across
/// the top `TAB_STRIP_HEIGHT` band of it. The corresponding active tab's
/// `Placement` (from `compute()`) sits below the strip in the same
/// coordinates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabGroupView {
    pub cell: Rect,
    pub tabs: Vec<WindowId>,
    pub active: usize,
}

/// A single workspace's tiling tree, plus its outer bounds and gap config.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LayoutTree {
    pub root: Option<Box<Node>>,
    pub outer_gap: i32,
    pub inner_gap: i32,
}

impl LayoutTree {
    pub fn new(outer_gap: i32, inner_gap: i32) -> Self {
        Self { root: None, outer_gap, inner_gap }
    }

    pub fn is_empty(&self) -> bool { self.root.is_none() }

    /// Whether `id` is currently a member of a `Tabbed` group anywhere in
    /// the tree. Drives the "tab groups are sticky" UX: drag-release on
    /// blank space shouldn't accidentally pop a window out of its group;
    /// only explicit untab via button or keybind should.
    pub fn is_in_tab_group(&self, id: WindowId) -> bool {
        fn walk(node: &Node, id: WindowId) -> bool {
            match node {
                Node::Leaf(_) => false,
                Node::Split { left, right, .. } => walk(left, id) || walk(right, id),
                Node::Tabbed { tabs, .. } => tabs.contains(&id),
            }
        }
        self.root.as_deref().map(|r| walk(r, id)).unwrap_or(false)
    }

    pub fn windows(&self) -> Vec<WindowId> {
        let mut out = Vec::new();
        if let Some(r) = &self.root { r.collect_leaves(&mut out); }
        out
    }

    /// Insert a new window. If the tree is empty, the window becomes the
    /// root leaf. Otherwise it splits `near` (the currently focused window)
    /// along an axis chosen by the longer dimension of its current cell —
    /// same heuristic Hyprland uses by default.
    pub fn insert(&mut self, new: WindowId, near: Option<WindowId>, monitor: Rect) {
        let Some(root) = self.root.take() else {
            self.root = Some(Node::leaf(new));
            return;
        };
        let target = near.unwrap_or_else(|| {
            let mut leaves = Vec::new();
            root.collect_leaves(&mut leaves);
            *leaves.last().expect("non-empty root has at least one leaf")
        });
        let usable = monitor.shrunk(self.outer_gap);
        let axis = pick_split_axis(&root, target, usable, self.inner_gap);
        self.root = Some(insert_at(root, target, new, axis));
    }

    /// Remove a window. Returns true if it existed.
    pub fn remove(&mut self, target: WindowId) -> bool {
        let Some(root) = self.root.take() else { return false };
        match remove_from(*root, target) {
            RemoveResult::NotFound(node) => {
                self.root = Some(Box::new(node));
                false
            }
            RemoveResult::Removed(remainder) => {
                self.root = remainder;
                true
            }
        }
    }

    /// Compute the final placement for every window, given the monitor's
    /// usable area (work area, with taskbar already excluded by tile-win).
    pub fn compute(&self, monitor: Rect) -> LayoutPlan {
        let mut plan = LayoutPlan::default();
        let Some(root) = &self.root else { return plan };
        let usable = monitor.shrunk(self.outer_gap);
        place(root, usable, self.inner_gap, &mut plan);
        plan
    }

    /// Walk the tree and emit one `TabGroupView` per `Tabbed` node, with
    /// the *full* cell rect (including the strip area) so the renderer
    /// knows where to position its overlay. The order of returned groups
    /// follows the tree's left→right traversal — stable across calls
    /// when the tree shape doesn't change, which lets the overlay
    /// manager diff cheaply.
    pub fn tab_groups(&self, monitor: Rect) -> Vec<TabGroupView> {
        let mut out = Vec::new();
        let Some(root) = &self.root else { return out };
        let usable = monitor.shrunk(self.outer_gap);
        collect_tab_groups(root, usable, self.inner_gap, &mut out);
        out
    }

    /// Find the neighbour of `from` in the given direction. Returns the
    /// leaf window-id that should receive focus (or be swapped with).
    /// Algorithm: walk up the tree until we find a split whose axis matches
    /// the direction and whose subtree-of-origin is on the correct side;
    /// then descend the *other* subtree picking the geometrically nearest
    /// leaf. This is the same approach i3 calls "tree navigation."
    pub fn neighbour(&self, from: WindowId, dir: Direction) -> Option<WindowId> {
        let root = self.root.as_deref()?;
        let plan = self.compute_for_navigation();
        let from_rect = plan.iter().find(|p| p.window == from)?.rect;
        // Geometric fallback: pick the candidate whose center lies in the
        // requested direction with the smallest perpendicular offset.
        // Beats tree-walk in the case of unbalanced trees and matches what
        // users expect visually.
        let (cx, cy) = (from_rect.x + from_rect.width / 2, from_rect.y + from_rect.height / 2);
        plan.iter()
            .filter(|p| p.window != from)
            .filter(|p| {
                let (px, py) = (p.rect.x + p.rect.width / 2, p.rect.y + p.rect.height / 2);
                match dir {
                    Direction::Left  => px < cx,
                    Direction::Right => px > cx,
                    Direction::Up    => py < cy,
                    Direction::Down  => py > cy,
                }
            })
            .min_by_key(|p| {
                let (px, py) = (p.rect.x + p.rect.width / 2, p.rect.y + p.rect.height / 2);
                let (dx, dy) = ((px - cx).abs(), (py - cy).abs());
                if dir.is_horizontal() { dy * 4 + dx } else { dx * 4 + dy }
            })
            .map(|p| p.window)
            .or_else(|| {
                let _ = root; // keep `root` live for the type-check above
                None
            })
    }

    fn compute_for_navigation(&self) -> Vec<Placement> {
        // Use a synthetic 10000x10000 box — neighbour math only cares about
        // *relative* positions, not absolute pixels. This avoids needing
        // the daemon to round-trip the monitor rect for navigation.
        self.compute(Rect::new(0, 0, 10_000, 10_000)).placements
    }

    /// Swap two leaves in place. Returns false if either id is missing.
    pub fn swap(&mut self, a: WindowId, b: WindowId) -> bool {
        let Some(root) = self.root.as_deref_mut() else { return false };
        if !root.contains(a) || !root.contains(b) || a == b { return false }
        swap_leaves(root, a, b);
        true
    }

    /// Adjust the split ratio of the parent of `pivot` along the given axis.
    /// `delta` is in fractional units (e.g. 0.05 to grow the left side by 5%).
    pub fn resize(&mut self, pivot: WindowId, axis: SplitAxis, delta: f32) -> bool {
        let Some(root) = self.root.as_deref_mut() else { return false };
        adjust_ratio(root, pivot, axis, delta).is_some()
    }

    /// Merge `dragged` into the same tile as `target`, creating a `Tabbed`
    /// node when `target` is a plain `Leaf`, or appending when `target` is
    /// already inside a `Tabbed`. New tabs are appended to the end and
    /// become the active one — mirrors how i3/Hyprland surface a freshly
    /// dropped window so the user sees what they just dragged.
    /// Returns false if either id is missing or both are the same window.
    /// Insert `src` next to `target`, splitting `target`'s cell along the
    /// chosen `axis`. If `before` is true, `src` ends up on the left/top
    /// side of `target`; otherwise the right/bottom side. `src` must
    /// already be absent from the tree — caller is expected to remove
    /// it first if it was already in the layout. Returns true if `target`
    /// was found and the split was created.
    ///
    /// Used by drag-to-edge: dropping on a tile's bottom edge calls this
    /// with `axis=Vertical, before=false` so the dragged window becomes
    /// the lower sibling of the target. Same primitive supports drag-to-
    /// retile of a previously-floating window.
    pub fn insert_beside(&mut self, src: WindowId, target: WindowId, axis: SplitAxis, before: bool) -> bool {
        let Some(root) = self.root.take() else {
            // Tree was empty — just plant `src` as the root.
            self.root = Some(Node::leaf(src));
            return true;
        };
        if !root.contains(target) {
            self.root = Some(root);
            return false;
        }
        self.root = Some(insert_beside_at(root, target, src, axis, before));
        true
    }

    pub fn merge(&mut self, target: WindowId, dragged: WindowId) -> bool {
        if target == dragged { return false; }
        let Some(root) = self.root.take() else { return false };
        if !root.contains(target) || !root.contains(dragged) {
            self.root = Some(root);
            return false;
        }
        // Pull `dragged` out first so the tree is in a consistent shape
        // before we tab it into `target`.
        let after_remove = match remove_from(*root, dragged) {
            RemoveResult::Removed(remainder) => remainder,
            RemoveResult::NotFound(node) => {
                self.root = Some(Box::new(node));
                return false;
            }
        };
        let Some(remainder) = after_remove else {
            // The tree only had `dragged` — nothing to merge into. Restore.
            self.root = Some(Node::leaf(dragged));
            return false;
        };
        self.root = Some(merge_into(remainder, target, dragged));
        true
    }

    /// Extract `target` from its tab group into a sibling cell. The
    /// remaining tabs stay tabbed (or collapse to a Leaf if only one is
    /// left); `target` becomes a Leaf split off along the longer dimension
    /// of the original cell, matching the BSP heuristic used by `insert`.
    /// No-op if `target` isn't currently in a `Tabbed`.
    pub fn untab(&mut self, target: WindowId, monitor: Rect) -> bool {
        let Some(root) = self.root.as_deref() else { return false };
        if !in_tabbed(root, target) { return false; }
        let usable = monitor.shrunk(self.outer_gap);
        let axis = pick_split_axis(root, target, usable, self.inner_gap);
        let Some(root) = self.root.take() else { return false };
        self.root = Some(untab_at(root, target, axis));
        true
    }

    /// Cycle to the next/previous tab in the `Tabbed` group containing
    /// `current`. Returns the newly-active `WindowId`, or `None` when
    /// `current` isn't part of a tab group (callers fall back to regular
    /// focus navigation in that case).
    pub fn cycle_tab(&mut self, current: WindowId, forward: bool) -> Option<WindowId> {
        let root = self.root.as_deref_mut()?;
        cycle_tab_in(root, current, forward)
    }
}

// ---- helpers ---------------------------------------------------------------

fn pick_split_axis(root: &Node, target: WindowId, usable: Rect, gap: i32) -> SplitAxis {
    // Compute the cell occupied by `target` under the current tree, then
    // split along its longer dimension so windows stay roughly square.
    let mut tmp = LayoutPlan::default();
    place(root, usable, gap, &mut tmp);
    let cell = tmp.placements.iter().find(|p| p.window == target).map(|p| p.rect);
    match cell {
        Some(r) if r.width >= r.height => SplitAxis::Horizontal,
        Some(_) => SplitAxis::Vertical,
        None => SplitAxis::Horizontal,
    }
}

/// Variant of `insert_at` that lets the caller force the side: if `before`
/// is true, the new window lands on the left/top of the split; otherwise
/// the right/bottom. When `target` is part of a `Tabbed`, the entire
/// tab group becomes the *target* sibling — splitting *next to* the group
/// rather than appending into it (that's what `insert_at` does and is
/// what the user *doesn't* want when they drop on an edge).
fn insert_beside_at(node: Box<Node>, target: WindowId, src: WindowId, axis: SplitAxis, before: bool) -> Box<Node> {
    match *node {
        Node::Leaf(id) if id == target => {
            let target_node = Node::leaf(id);
            let src_node = Node::leaf(src);
            let (left, right) = if before { (src_node, target_node) } else { (target_node, src_node) };
            Box::new(Node::Split { axis, ratio: 0.5, left, right })
        }
        Node::Leaf(_) => node,
        Node::Tabbed { ref tabs, .. } if tabs.contains(&target) => {
            let tabbed = node;
            let src_node = Node::leaf(src);
            let (left, right) = if before { (src_node, tabbed) } else { (tabbed, src_node) };
            Box::new(Node::Split { axis, ratio: 0.5, left, right })
        }
        Node::Tabbed { .. } => node,
        Node::Split { axis: a, ratio, left, right } => {
            if left.contains(target) {
                Box::new(Node::Split { axis: a, ratio, left: insert_beside_at(left, target, src, axis, before), right })
            } else {
                Box::new(Node::Split { axis: a, ratio, left, right: insert_beside_at(right, target, src, axis, before) })
            }
        }
    }
}

fn insert_at(node: Box<Node>, target: WindowId, new: WindowId, axis: SplitAxis) -> Box<Node> {
    match *node {
        Node::Leaf(id) if id == target => Box::new(Node::Split {
            axis,
            ratio: 0.5,
            left: Node::leaf(id),
            right: Node::leaf(new),
        }),
        Node::Leaf(_) => node,
        Node::Split { axis: a, ratio, left, right } => {
            if left.contains(target) {
                Box::new(Node::Split { axis: a, ratio, left: insert_at(left, target, new, axis), right })
            } else {
                Box::new(Node::Split { axis: a, ratio, left, right: insert_at(right, target, new, axis) })
            }
        }
        Node::Tabbed { mut tabs, active } => {
            // Insertions land *into* the existing tab group rather than
            // splitting it — that's the whole point of a Tabbed: keep
            // adjacent windows stacked.
            if tabs.contains(&target) {
                tabs.push(new);
                let active = tabs.len() - 1;
                Box::new(Node::Tabbed { tabs, active })
            } else {
                Box::new(Node::Tabbed { tabs, active })
            }
        }
    }
}

enum RemoveResult {
    NotFound(Node),
    /// The remainder after collapsing the split — `None` means the whole
    /// subtree disappeared (target was the only leaf).
    Removed(Option<Box<Node>>),
}

fn remove_from(node: Node, target: WindowId) -> RemoveResult {
    match node {
        Node::Leaf(id) if id == target => RemoveResult::Removed(None),
        Node::Leaf(_) => RemoveResult::NotFound(node),
        Node::Tabbed { mut tabs, active } => {
            if let Some(pos) = tabs.iter().position(|id| *id == target) {
                tabs.remove(pos);
                match tabs.len() {
                    0 => RemoveResult::Removed(None),
                    // Two-tab group losing one window collapses back to a
                    // plain Leaf — there's nothing left to "stack."
                    1 => RemoveResult::Removed(Some(Node::leaf(tabs[0]))),
                    _ => {
                        let active = active.min(tabs.len() - 1);
                        RemoveResult::Removed(Some(Box::new(Node::Tabbed { tabs, active })))
                    }
                }
            } else {
                RemoveResult::NotFound(Node::Tabbed { tabs, active })
            }
        }
        Node::Split { axis, ratio, left, right } => {
            match remove_from(*left, target) {
                RemoveResult::Removed(None) => RemoveResult::Removed(Some(right)),
                RemoveResult::Removed(Some(remainder)) => RemoveResult::Removed(Some(Box::new(
                    Node::Split { axis, ratio, left: remainder, right }
                ))),
                RemoveResult::NotFound(left) => match remove_from(*right, target) {
                    RemoveResult::Removed(None) => RemoveResult::Removed(Some(Box::new(left))),
                    RemoveResult::Removed(Some(remainder)) => RemoveResult::Removed(Some(Box::new(
                        Node::Split { axis, ratio, left: Box::new(left), right: remainder }
                    ))),
                    RemoveResult::NotFound(right) => RemoveResult::NotFound(
                        Node::Split { axis, ratio, left: Box::new(left), right: Box::new(right) }
                    ),
                },
            }
        }
    }
}

fn place(node: &Node, area: Rect, gap: i32, out: &mut LayoutPlan) {
    match node {
        Node::Leaf(id) => out.placements.push(Placement { window: *id, rect: area }),
        Node::Split { axis, ratio, left, right } => match axis {
            SplitAxis::Horizontal => {
                let (l, r) = area.split_h(*ratio, gap);
                place(left, l, gap, out);
                place(right, r, gap, out);
            }
            SplitAxis::Vertical => {
                let (t, b) = area.split_v(*ratio, gap);
                place(left, t, gap, out);
                place(right, b, gap, out);
            }
        },
        Node::Tabbed { tabs, active } => {
            // Stack ALL tabs at the cell rect. Earlier we only emitted the
            // active tab and left inactive ones at their pre-merge rects —
            // but that meant inactive windows could sit anywhere on screen
            // and pop to foreground when clicked, since the OS treated them
            // as independent top-level windows. By co-locating them at
            // the same rect, the only visible one is whichever is highest
            // in Z-order. The applier emits in this order (inactive first,
            // active last) so the last `SetWindowPos` raises the active
            // tab on top of the inactive siblings.
            let strip = TAB_STRIP_HEIGHT.min(area.height);
            let rect = Rect::new(area.x, area.y + strip, area.width, area.height - strip);
            for (i, &id) in tabs.iter().enumerate() {
                if i != *active {
                    out.placements.push(Placement { window: id, rect });
                }
            }
            if let Some(active_id) = tabs.get(*active).copied() {
                out.placements.push(Placement { window: active_id, rect });
            }
        }
    }
}

fn collect_tab_groups(node: &Node, area: Rect, gap: i32, out: &mut Vec<TabGroupView>) {
    match node {
        Node::Leaf(_) => {}
        Node::Split { axis, ratio, left, right } => match axis {
            SplitAxis::Horizontal => {
                let (l, r) = area.split_h(*ratio, gap);
                collect_tab_groups(left, l, gap, out);
                collect_tab_groups(right, r, gap, out);
            }
            SplitAxis::Vertical => {
                let (t, b) = area.split_v(*ratio, gap);
                collect_tab_groups(left, t, gap, out);
                collect_tab_groups(right, b, gap, out);
            }
        },
        Node::Tabbed { tabs, active } => {
            out.push(TabGroupView { cell: area, tabs: tabs.clone(), active: *active });
        }
    }
}

/// Walk the tree, find the `Tabbed` group containing `window`, and set
/// its `active` to that window's index. Returns `true` if found.
pub(crate) fn activate_tab_in(node: &mut Node, window: WindowId) -> bool {
    match node {
        Node::Leaf(_) => false,
        Node::Split { left, right, .. } => activate_tab_in(left, window) || activate_tab_in(right, window),
        Node::Tabbed { tabs, active } => {
            if let Some(idx) = tabs.iter().position(|id| *id == window) {
                *active = idx;
                true
            } else {
                false
            }
        }
    }
}

fn swap_leaves(node: &mut Node, a: WindowId, b: WindowId) {
    match node {
        Node::Leaf(id) => {
            if *id == a { *id = b; }
            else if *id == b { *id = a; }
        }
        Node::Split { left, right, .. } => {
            swap_leaves(left, a, b);
            swap_leaves(right, a, b);
        }
        Node::Tabbed { tabs, .. } => {
            for id in tabs.iter_mut() {
                if *id == a { *id = b; }
                else if *id == b { *id = a; }
            }
        }
    }
}

/// True when `target` lives inside a `Tabbed` group rather than as a
/// stand-alone leaf. Used by `untab` to short-circuit the no-op case
/// before we go rebuild the tree.
fn in_tabbed(node: &Node, target: WindowId) -> bool {
    match node {
        Node::Leaf(_) => false,
        Node::Tabbed { tabs, .. } => tabs.contains(&target),
        Node::Split { left, right, .. } => in_tabbed(left, target) || in_tabbed(right, target),
    }
}

/// Replace the subtree containing `target`'s tab group with a Split that
/// has the (possibly collapsed) remaining group on one side and a fresh
/// leaf for `target` on the other.
fn untab_at(node: Box<Node>, target: WindowId, axis: SplitAxis) -> Box<Node> {
    match *node {
        Node::Leaf(id) => Box::new(Node::Leaf(id)),
        Node::Tabbed { mut tabs, active } => {
            if !tabs.contains(&target) {
                return Box::new(Node::Tabbed { tabs, active });
            }
            tabs.retain(|id| *id != target);
            let remainder: Box<Node> = match tabs.len() {
                0 => Node::leaf(target), // shouldn't happen: target was one of >=2 tabs
                1 => Node::leaf(tabs[0]),
                _ => {
                    let active = active.min(tabs.len() - 1);
                    Box::new(Node::Tabbed { tabs, active })
                }
            };
            Box::new(Node::Split {
                axis,
                ratio: 0.5,
                left: remainder,
                right: Node::leaf(target),
            })
        }
        Node::Split { axis: a, ratio, left, right } => {
            if in_tabbed(&left, target) {
                Box::new(Node::Split { axis: a, ratio, left: untab_at(left, target, axis), right })
            } else {
                Box::new(Node::Split { axis: a, ratio, left, right: untab_at(right, target, axis) })
            }
        }
    }
}

/// Drop `dragged` into the same cell as `target`. If `target` is a Leaf,
/// promote both into a fresh Tabbed; if `target` already belongs to a
/// Tabbed, append. The new tab becomes active.
fn merge_into(node: Box<Node>, target: WindowId, dragged: WindowId) -> Box<Node> {
    match *node {
        Node::Leaf(id) if id == target => Box::new(Node::Tabbed {
            tabs: vec![id, dragged],
            active: 1,
        }),
        Node::Leaf(_) => node,
        Node::Tabbed { mut tabs, active } => {
            if tabs.contains(&target) {
                tabs.push(dragged);
                let active = tabs.len() - 1;
                Box::new(Node::Tabbed { tabs, active })
            } else {
                Box::new(Node::Tabbed { tabs, active })
            }
        }
        Node::Split { axis, ratio, left, right } => {
            if left.contains(target) {
                Box::new(Node::Split { axis, ratio, left: merge_into(left, target, dragged), right })
            } else {
                Box::new(Node::Split { axis, ratio, left, right: merge_into(right, target, dragged) })
            }
        }
    }
}

/// Walk the tree, find the Tabbed containing `current`, advance/retreat
/// `active`, and return the newly-active id. Returns `None` if `current`
/// isn't tabbed.
fn cycle_tab_in(node: &mut Node, current: WindowId, forward: bool) -> Option<WindowId> {
    match node {
        Node::Leaf(_) => None,
        Node::Tabbed { tabs, active } => {
            let pos = tabs.iter().position(|id| *id == current)?;
            let n = tabs.len();
            let next = if forward { (pos + 1) % n } else { (pos + n - 1) % n };
            *active = next;
            Some(tabs[next])
        }
        Node::Split { left, right, .. } => {
            cycle_tab_in(left, current, forward).or_else(|| cycle_tab_in(right, current, forward))
        }
    }
}

/// Walk upward from the leaf, returning the first ancestor whose split axis
/// matches the requested one and adjusting its ratio. Returns `Some(())`
/// on success.
fn adjust_ratio(node: &mut Node, pivot: WindowId, axis: SplitAxis, delta: f32) -> Option<()> {
    fn walk(node: &mut Node, pivot: WindowId, axis: SplitAxis, delta: f32) -> Walk {
        match node {
            Node::Leaf(id) if *id == pivot => Walk::FoundLeaf,
            Node::Leaf(_) => Walk::NotFound,
            Node::Tabbed { tabs, .. } => {
                if tabs.contains(&pivot) { Walk::FoundLeaf } else { Walk::NotFound }
            }
            Node::Split { axis: a, ratio, left, right } => {
                match walk(left, pivot, axis, delta) {
                    Walk::FoundLeaf | Walk::NeedAncestor if *a == axis => {
                        *ratio = (*ratio + delta).clamp(0.05, 0.95);
                        Walk::Adjusted
                    }
                    Walk::FoundLeaf | Walk::NeedAncestor => Walk::NeedAncestor,
                    Walk::Adjusted => Walk::Adjusted,
                    Walk::NotFound => match walk(right, pivot, axis, delta) {
                        Walk::FoundLeaf | Walk::NeedAncestor if *a == axis => {
                            *ratio = (*ratio - delta).clamp(0.05, 0.95);
                            Walk::Adjusted
                        }
                        other => other,
                    },
                }
            }
        }
    }
    enum Walk { NotFound, FoundLeaf, NeedAncestor, Adjusted }
    match walk(node, pivot, axis, delta) {
        Walk::Adjusted => Some(()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(n: u64) -> WindowId { WindowId(n) }

    #[test]
    fn single_window_fills_monitor() {
        let mut tree = LayoutTree::new(0, 0);
        tree.insert(w(1), None, Rect::new(0, 0, 1920, 1080));
        let plan = tree.compute(Rect::new(0, 0, 1920, 1080));
        assert_eq!(plan.placements.len(), 1);
        assert_eq!(plan.placements[0].rect, Rect::new(0, 0, 1920, 1080));
    }

    #[test]
    fn two_windows_split_horizontally() {
        let mon = Rect::new(0, 0, 1000, 500);
        let mut tree = LayoutTree::new(0, 0);
        tree.insert(w(1), None, mon);
        tree.insert(w(2), Some(w(1)), mon);
        let plan = tree.compute(mon);
        assert_eq!(plan.placements.len(), 2);
        // wider-than-tall monitor → first split is horizontal
        assert_eq!(plan.placements[0].rect.height, 500);
        assert_eq!(plan.placements[1].rect.height, 500);
        assert_eq!(plan.placements[0].rect.width + plan.placements[1].rect.width, 1000);
    }

    #[test]
    fn three_windows_split_along_longest_dim() {
        // After two inserts, the right cell is 500x500 (square). On a tie our
        // pick_split_axis prefers Horizontal — so #3 ends up beside #2, not below.
        // This test locks that policy in.
        let mon = Rect::new(0, 0, 1000, 500);
        let mut tree = LayoutTree::new(0, 0);
        tree.insert(w(1), None, mon);
        tree.insert(w(2), Some(w(1)), mon);
        tree.insert(w(3), Some(w(2)), mon);
        let plan = tree.compute(mon);
        assert_eq!(plan.placements.len(), 3);
        let by_id: std::collections::HashMap<_,_> = plan.placements.iter().map(|p| (p.window, p.rect)).collect();
        assert_eq!(by_id[&w(1)], Rect::new(0, 0, 500, 500));
        // #2 and #3 share the right half, side-by-side on the horizontal axis.
        assert_eq!(by_id[&w(2)].y, 0);
        assert_eq!(by_id[&w(3)].y, 0);
        assert_eq!(by_id[&w(2)].height, 500);
        assert_eq!(by_id[&w(3)].height, 500);
        assert_eq!(by_id[&w(2)].width + by_id[&w(3)].width, 500);
    }

    #[test]
    fn remove_collapses_split() {
        let mon = Rect::new(0, 0, 1000, 500);
        let mut tree = LayoutTree::new(0, 0);
        tree.insert(w(1), None, mon);
        tree.insert(w(2), Some(w(1)), mon);
        assert!(tree.remove(w(1)));
        let plan = tree.compute(mon);
        assert_eq!(plan.placements.len(), 1);
        assert_eq!(plan.placements[0].window, w(2));
        assert_eq!(plan.placements[0].rect, mon);
    }

    #[test]
    fn remove_last_window_empties_tree() {
        let mon = Rect::new(0, 0, 1000, 500);
        let mut tree = LayoutTree::new(0, 0);
        tree.insert(w(1), None, mon);
        assert!(tree.remove(w(1)));
        assert!(tree.is_empty());
        assert_eq!(tree.compute(mon).placements.len(), 0);
    }

    #[test]
    fn swap_exchanges_leaves() {
        let mon = Rect::new(0, 0, 1000, 500);
        let mut tree = LayoutTree::new(0, 0);
        tree.insert(w(1), None, mon);
        tree.insert(w(2), Some(w(1)), mon);
        let before = tree.compute(mon);
        assert!(tree.swap(w(1), w(2)));
        let after = tree.compute(mon);
        // rects unchanged, ids exchanged
        let r1_before = before.placements.iter().find(|p| p.window == w(1)).unwrap().rect;
        let r1_after  = after.placements.iter().find(|p| p.window == w(1)).unwrap().rect;
        assert_ne!(r1_before, r1_after);
    }

    #[test]
    fn neighbour_right_picks_horizontally_adjacent() {
        let mon = Rect::new(0, 0, 1000, 500);
        let mut tree = LayoutTree::new(0, 0);
        tree.insert(w(1), None, mon);
        tree.insert(w(2), Some(w(1)), mon);
        assert_eq!(tree.neighbour(w(1), Direction::Right), Some(w(2)));
        assert_eq!(tree.neighbour(w(2), Direction::Left),  Some(w(1)));
        assert_eq!(tree.neighbour(w(1), Direction::Up),    None);
    }

    #[test]
    fn outer_and_inner_gaps_apply() {
        let mon = Rect::new(0, 0, 1000, 500);
        let mut tree = LayoutTree::new(20, 10);
        tree.insert(w(1), None, mon);
        tree.insert(w(2), Some(w(1)), mon);
        let plan = tree.compute(mon);
        // both windows respect outer_gap on top
        for p in &plan.placements {
            assert_eq!(p.rect.y, 20);
            assert_eq!(p.rect.height, 460);
        }
    }

    // ---- Tabbed-node tests -------------------------------------------------

    #[test]
    fn merge_two_leaves_creates_tabbed() {
        let mon = Rect::new(0, 0, 1000, 500);
        let mut tree = LayoutTree::new(0, 0);
        tree.insert(w(1), None, mon);
        tree.insert(w(2), Some(w(1)), mon);
        assert!(tree.merge(w(1), w(2)));
        // The whole tree collapses to a single Tabbed node holding both.
        match tree.root.as_deref().unwrap() {
            Node::Tabbed { tabs, active } => {
                assert_eq!(tabs, &vec![w(1), w(2)]);
                // Newly-merged tab is the active one.
                assert_eq!(*active, 1);
            }
            other => panic!("expected Tabbed root, got {:?}", other),
        }
    }

    #[test]
    fn merge_into_existing_tabbed_appends() {
        let mon = Rect::new(0, 0, 1000, 500);
        let mut tree = LayoutTree::new(0, 0);
        tree.insert(w(1), None, mon);
        tree.insert(w(2), Some(w(1)), mon);
        tree.insert(w(3), Some(w(2)), mon);
        // Tab #2 with #3 first, then drag #1 onto the existing tab group.
        assert!(tree.merge(w(2), w(3)));
        assert!(tree.merge(w(2), w(1)));
        match tree.root.as_deref().unwrap() {
            Node::Tabbed { tabs, active } => {
                assert_eq!(tabs.len(), 3);
                assert!(tabs.contains(&w(1)));
                assert!(tabs.contains(&w(2)));
                assert!(tabs.contains(&w(3)));
                // last appended is active
                assert_eq!(tabs[*active], w(1));
            }
            other => panic!("expected Tabbed root, got {:?}", other),
        }
    }

    #[test]
    fn removing_one_of_two_tabs_collapses_to_leaf() {
        let mon = Rect::new(0, 0, 1000, 500);
        let mut tree = LayoutTree::new(0, 0);
        tree.insert(w(1), None, mon);
        tree.insert(w(2), Some(w(1)), mon);
        assert!(tree.merge(w(1), w(2)));
        assert!(tree.remove(w(2)));
        match tree.root.as_deref().unwrap() {
            Node::Leaf(id) => assert_eq!(*id, w(1)),
            other => panic!("expected Leaf, got {:?}", other),
        }
    }

    #[test]
    fn cycle_tab_returns_next_window() {
        let mon = Rect::new(0, 0, 1000, 500);
        let mut tree = LayoutTree::new(0, 0);
        tree.insert(w(1), None, mon);
        tree.insert(w(2), Some(w(1)), mon);
        tree.insert(w(3), Some(w(2)), mon);
        assert!(tree.merge(w(1), w(2)));
        assert!(tree.merge(w(1), w(3)));
        // Now tabs = [1, 2, 3] and the active is 3 (latest merged).
        // Forward from 3 wraps to 1.
        assert_eq!(tree.cycle_tab(w(3), true), Some(w(1)));
        assert_eq!(tree.cycle_tab(w(1), true), Some(w(2)));
        assert_eq!(tree.cycle_tab(w(2), false), Some(w(1)));
    }

    #[test]
    fn cycle_tab_on_leaf_returns_none() {
        let mon = Rect::new(0, 0, 1000, 500);
        let mut tree = LayoutTree::new(0, 0);
        tree.insert(w(1), None, mon);
        assert_eq!(tree.cycle_tab(w(1), true), None);
    }

    #[test]
    fn tabbed_group_stacks_all_tabs_at_cell_with_active_last() {
        // Behavior: every tab in the group gets a placement at the same
        // cell rect (minus the tab strip at the top). The active tab is
        // emitted *last* so the applier's iteration order Z-stacks it on
        // top of the inactive siblings. Inactive tabs at the same rect
        // means they can't be clicked into foreground unexpectedly.
        let mon = Rect::new(0, 0, 1000, 500);
        let mut tree = LayoutTree::new(0, 0);
        tree.insert(w(1), None, mon);
        tree.insert(w(2), Some(w(1)), mon);
        tree.insert(w(3), Some(w(2)), mon);
        assert!(tree.merge(w(1), w(2)));
        assert!(tree.merge(w(1), w(3)));
        let plan = tree.compute(mon);
        assert_eq!(plan.placements.len(), 3, "all tabs get placements, not just active");
        let expected = Rect::new(0, TAB_STRIP_HEIGHT, 1000, 500 - TAB_STRIP_HEIGHT);
        for p in &plan.placements {
            assert_eq!(p.rect, expected, "every tab is placed at the cell rect");
        }
        // The most recently merged window (w(3)) is active and emitted last.
        assert_eq!(plan.placements.last().unwrap().window, w(3));
    }

    #[test]
    fn untab_extracts_target_into_split_sibling() {
        let mon = Rect::new(0, 0, 1000, 500);
        let mut tree = LayoutTree::new(0, 0);
        tree.insert(w(1), None, mon);
        tree.insert(w(2), Some(w(1)), mon);
        tree.insert(w(3), Some(w(2)), mon);
        assert!(tree.merge(w(1), w(2)));
        assert!(tree.merge(w(1), w(3)));
        // tabs = [1, 2, 3]; pull #2 back out.
        assert!(tree.untab(w(2), mon));
        // Root should now be a Split whose two sides are the remaining
        // Tabbed of [1, 3] and a Leaf for #2.
        match tree.root.as_deref().unwrap() {
            Node::Split { left, right, .. } => {
                let (tab_side, leaf_side) = match (left.as_ref(), right.as_ref()) {
                    (Node::Tabbed { .. }, Node::Leaf(_)) => (left.as_ref(), right.as_ref()),
                    (Node::Leaf(_), Node::Tabbed { .. }) => (right.as_ref(), left.as_ref()),
                    _ => panic!("expected Tabbed + Leaf children, got {:?} / {:?}", left, right),
                };
                match tab_side {
                    Node::Tabbed { tabs, .. } => {
                        assert_eq!(tabs.len(), 2);
                        assert!(tabs.contains(&w(1)));
                        assert!(tabs.contains(&w(3)));
                        assert!(!tabs.contains(&w(2)));
                    }
                    _ => unreachable!(),
                }
                match leaf_side {
                    Node::Leaf(id) => assert_eq!(*id, w(2)),
                    _ => unreachable!(),
                }
            }
            other => panic!("expected Split root after untab, got {:?}", other),
        }
        // Layout produces: one placement for #2 (extracted leaf) plus
        // every tab in the remaining group {1, 3} stacked at the tabbed
        // sibling's cell. Total = 1 leaf + 2 tabs = 3 placements.
        let plan = tree.compute(mon);
        assert_eq!(plan.placements.len(), 3);
    }

    #[test]
    fn untab_of_two_tab_group_collapses_remaining_to_leaf() {
        let mon = Rect::new(0, 0, 1000, 500);
        let mut tree = LayoutTree::new(0, 0);
        tree.insert(w(1), None, mon);
        tree.insert(w(2), Some(w(1)), mon);
        assert!(tree.merge(w(1), w(2)));
        assert!(tree.untab(w(2), mon));
        // After untab, the remaining single tab collapses; the Split has
        // two Leaf children.
        match tree.root.as_deref().unwrap() {
            Node::Split { left, right, .. } => match (left.as_ref(), right.as_ref()) {
                (Node::Leaf(_), Node::Leaf(_)) => {}
                other => panic!("expected Leaf+Leaf, got {:?}", other),
            },
            other => panic!("expected Split root, got {:?}", other),
        }
    }

    #[test]
    fn untab_no_op_for_non_tabbed_window() {
        let mon = Rect::new(0, 0, 1000, 500);
        let mut tree = LayoutTree::new(0, 0);
        tree.insert(w(1), None, mon);
        tree.insert(w(2), Some(w(1)), mon);
        assert!(!tree.untab(w(1), mon));
    }

    #[test]
    fn swap_exchanges_inside_tabbed() {
        let mon = Rect::new(0, 0, 1000, 500);
        let mut tree = LayoutTree::new(0, 0);
        tree.insert(w(1), None, mon);
        tree.insert(w(2), Some(w(1)), mon);
        tree.insert(w(3), Some(w(2)), mon);
        assert!(tree.merge(w(2), w(3)));
        // tabs on the right cell are [2, 3]; swap #1 (left leaf) with #3 (in tabbed).
        assert!(tree.swap(w(1), w(3)));
        let mut all = Vec::new();
        tree.root.as_deref().unwrap().collect_leaves(&mut all);
        assert!(all.contains(&w(1)));
        assert!(all.contains(&w(2)));
        assert!(all.contains(&w(3)));
    }

    /// Drop-on-bottom: target stays where it was, dragged window appears
    /// below as a new sibling. Drives drag-to-edge "tile here" UX.
    #[test]
    fn insert_beside_bottom_creates_vertical_sibling() {
        let mon = Rect::new(0, 0, 1000, 1000);
        let mut tree = LayoutTree::new(0, 0);
        tree.insert(w(1), None, mon);
        // Drop window 2 onto bottom edge of window 1.
        assert!(tree.insert_beside(w(2), w(1), SplitAxis::Vertical, /*before=*/false));
        let plan = tree.compute(mon);
        assert_eq!(plan.placements.len(), 2);
        let by_id: std::collections::HashMap<_, _> =
            plan.placements.iter().map(|p| (p.window, p.rect)).collect();
        // Both share full width, stacked top/bottom.
        assert_eq!(by_id[&w(1)].width, 1000);
        assert_eq!(by_id[&w(2)].width, 1000);
        assert_eq!(by_id[&w(1)].y, 0);
        assert_eq!(by_id[&w(2)].y, 500);
    }

    #[test]
    fn insert_beside_left_creates_horizontal_sibling() {
        let mon = Rect::new(0, 0, 1000, 500);
        let mut tree = LayoutTree::new(0, 0);
        tree.insert(w(1), None, mon);
        assert!(tree.insert_beside(w(2), w(1), SplitAxis::Horizontal, /*before=*/true));
        let plan = tree.compute(mon);
        let by_id: std::collections::HashMap<_, _> =
            plan.placements.iter().map(|p| (p.window, p.rect)).collect();
        // 2 is on the left, 1 is on the right.
        assert_eq!(by_id[&w(2)].x, 0);
        assert_eq!(by_id[&w(1)].x, 500);
    }

    /// Dropping on a tab-group's edge splits the entire group as one
    /// sibling — does NOT pull a tab out of it.
    #[test]
    fn insert_beside_tabbed_target_keeps_group_intact() {
        let mon = Rect::new(0, 0, 1000, 1000);
        let mut tree = LayoutTree::new(0, 0);
        tree.insert(w(1), None, mon);
        tree.insert(w(2), Some(w(1)), mon);
        assert!(tree.merge(w(2), w(1))); // tabbed group {1, 2}
        // Drop #3 below the tab group.
        assert!(tree.insert_beside(w(3), w(2), SplitAxis::Vertical, false));
        let mut all = Vec::new();
        tree.root.as_deref().unwrap().collect_leaves(&mut all);
        assert_eq!(all.len(), 3);
        // Group is preserved: top half is the tab group, bottom half is #3.
        let plan = tree.compute(mon);
        // Tab group emits both members stacked at its cell + leaf #3 = 3.
        assert_eq!(plan.placements.len(), 3);
    }
}
