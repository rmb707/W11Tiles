//! Pure-logic core for TileManager.
//!
//! Nothing in this crate touches the Windows API. The daemon feeds events
//! into [`State`], asks for a [`LayoutPlan`], and pushes that plan to
//! `tile-win` to realize. Keeping the layout engine pure is the same trick
//! pop-shell uses on the GNOME side: it makes the hard parts unit-testable
//! and lets the shell of the program (events, hotkeys, animation) stay thin.

pub mod geom;
pub mod window;
pub mod layout;
pub mod workspace;
pub mod state;
pub mod direction;
pub mod config;
pub mod ipc;

pub use direction::Direction;
pub use geom::Rect;
pub use layout::{Edge, LayoutPlan, Placement, TabGroupView};
pub use state::{Event, State, StateError};
pub use window::{WindowId, WindowInfo};
pub use workspace::{Workspace, WorkspaceId};
pub mod animator;
