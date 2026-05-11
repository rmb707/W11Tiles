use serde::{Deserialize, Serialize};

/// Cardinal direction — used by focus-move, swap, and resize commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

impl Direction {
    pub fn is_horizontal(self) -> bool {
        matches!(self, Direction::Left | Direction::Right)
    }

    pub fn opposite(self) -> Direction {
        match self {
            Direction::Left  => Direction::Right,
            Direction::Right => Direction::Left,
            Direction::Up    => Direction::Down,
            Direction::Down  => Direction::Up,
        }
    }
}
