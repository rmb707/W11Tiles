use serde::{Deserialize, Serialize};

/// Pixel-space rectangle. Top-left origin, matching Win32 RECT semantics
/// (so we can hand `Rect` directly to the applier without translation).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl Rect {
    pub const ZERO: Rect = Rect { x: 0, y: 0, width: 0, height: 0 };

    pub const fn new(x: i32, y: i32, width: i32, height: i32) -> Self {
        Self { x, y, width, height }
    }

    pub const fn right(&self) -> i32 { self.x + self.width }
    pub const fn bottom(&self) -> i32 { self.y + self.height }

    pub const fn area(&self) -> i64 { self.width as i64 * self.height as i64 }

    pub fn shrunk(self, by: i32) -> Rect {
        Rect {
            x: self.x + by,
            y: self.y + by,
            width: (self.width - 2 * by).max(0),
            height: (self.height - 2 * by).max(0),
        }
    }

    /// Split horizontally — left/right halves with gap between them.
    /// `ratio` is the fraction (0.0..1.0) of the width given to the left side.
    pub fn split_h(&self, ratio: f32, gap: i32) -> (Rect, Rect) {
        let ratio = ratio.clamp(0.05, 0.95);
        let usable = (self.width - gap).max(0);
        let left_w = (usable as f32 * ratio).round() as i32;
        let right_w = usable - left_w;
        (
            Rect::new(self.x, self.y, left_w, self.height),
            Rect::new(self.x + left_w + gap, self.y, right_w, self.height),
        )
    }

    /// Split vertically — top/bottom halves with gap between.
    pub fn split_v(&self, ratio: f32, gap: i32) -> (Rect, Rect) {
        let ratio = ratio.clamp(0.05, 0.95);
        let usable = (self.height - gap).max(0);
        let top_h = (usable as f32 * ratio).round() as i32;
        let bot_h = usable - top_h;
        (
            Rect::new(self.x, self.y, self.width, top_h),
            Rect::new(self.x, self.y + top_h + gap, self.width, bot_h),
        )
    }

    pub fn contains_point(&self, x: i32, y: i32) -> bool {
        x >= self.x && x < self.right() && y >= self.y && y < self.bottom()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_h_with_gap_conserves_width() {
        let r = Rect::new(0, 0, 1000, 500);
        let (a, b) = r.split_h(0.5, 10);
        assert_eq!(a.width + b.width + 10, 1000);
        assert_eq!(b.x, a.right() + 10);
    }

    #[test]
    fn split_v_with_gap_conserves_height() {
        let r = Rect::new(0, 0, 800, 1000);
        let (a, b) = r.split_v(0.6, 8);
        assert_eq!(a.height + b.height + 8, 1000);
    }

    #[test]
    fn shrink_clamps_to_zero() {
        let r = Rect::new(0, 0, 10, 10).shrunk(20);
        assert_eq!(r.width, 0);
        assert_eq!(r.height, 0);
    }
}
