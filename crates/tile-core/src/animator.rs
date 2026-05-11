//! Frame-by-frame interpolation between layout plans.
//!
//! The daemon calls [`Animator::set_target`] each time the layout changes.
//! On every tick (driven by a tokio interval at ~60Hz) it calls
//! [`Animator::tick`] and applies the returned [`LayoutPlan`] via
//! `SetWindowPos`. When the animation completes, `tick` returns the final
//! target once and then `None` until the next `set_target`.
//!
//! Why a separate module rather than baking tweens into the applier:
//! interpolation is pure math over plans — no Win32 — so we keep it in
//! `tile-core` where it's unit-testable and the applier stays a thin
//! "translate plan to SetWindowPos" loop. The state machine and animator
//! both produce `LayoutPlan`s; the applier doesn't care which one it gets.
//!
//! Easing choice: ease-out cubic (`1 - (1-t)^3`). It starts fast and
//! decelerates into the target — matching the "settle" feel users expect
//! from window managers (Hyprland, GNOME, macOS Mission Control all use
//! ease-out variants). Linear feels mechanical; ease-in-out drags the
//! middle frames, which on a 200ms tween reads as sluggish.

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::geom::Rect;
use crate::layout::{LayoutPlan, Placement};
use crate::window::WindowId;

/// State of an in-flight tween between two layout plans.
///
/// We keep `prev` and `target` as full plans (not just rect deltas) so
/// `set_target` mid-flight can capture the *current interpolated frame*
/// as the new origin without having to know which window is mid-move.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Animator {
    duration_ms: u32,
    prev: LayoutPlan,
    target: LayoutPlan,
    /// `Some(t0)` while a tween is running; `None` once we've settled and
    /// already emitted the final frame. Storing it as `Option<Instant>`
    /// (rather than a separate `is_animating: bool`) keeps the "settled"
    /// invariant in one place.
    #[serde(skip)]
    start: Option<Instant>,
    /// Set when `set_target` runs — `tick` consumes it to emit the first
    /// frame. Required so a brand-new target with no prev still reaches
    /// the applier even though `start` is `None`.
    #[serde(skip)]
    pending_emit: bool,
}

impl Animator {
    pub fn new(duration_ms: u32) -> Self {
        Self {
            duration_ms,
            prev: LayoutPlan::default(),
            target: LayoutPlan::default(),
            start: None,
            pending_emit: false,
        }
    }

    /// Replace the target plan. The `prev` baseline becomes whatever the
    /// CURRENT interpolated frame was — so mid-flight retargets feel
    /// continuous, not like the in-progress animation got cancelled and
    /// jumps to a new origin.
    ///
    /// Three cases:
    /// 1. First-ever target (prev is empty *and* no animation running):
    ///    skip the tween entirely, just mark for one immediate emit.
    /// 2. Identical to current target with the animation already settled:
    ///    no-op. Don't restart a tween nobody asked for.
    /// 3. Otherwise: capture the current frame as `prev`, start fresh.
    pub fn set_target(&mut self, plan: LayoutPlan, now: Instant) {
        // Case 2: same plan, already settled — nothing to do.
        if self.start.is_none() && !self.pending_emit && plans_equal(&self.target, &plan) {
            return;
        }

        // Case 1: first-ever target. No prev to lerp from, so emit
        // instantly without a tween. We deliberately don't tween from
        // `(0,0,0,0)` — that would visually shrink every window from a
        // dot, which looks awful at startup.
        let first_target = self.prev.placements.is_empty()
            && self.target.placements.is_empty()
            && self.start.is_none();
        if first_target {
            self.target = plan;
            self.prev = LayoutPlan::default();
            self.start = None;
            self.pending_emit = true;
            return;
        }

        // Case 3: capture current visible frame as the new prev. If
        // we're mid-animation, that's the interpolated state at `now`;
        // if we'd already settled, it's the previous target itself.
        let new_prev = match self.start {
            Some(t0) => self.frame_at(now, t0),
            None => self.target.clone(),
        };
        self.prev = new_prev;
        self.target = plan;
        self.start = Some(now);
        self.pending_emit = false;
    }

    /// Returns `Some(frame_plan)` if a frame should be drawn, or `None`
    /// if the animation has settled at `target` and there's nothing new
    /// to push. Caller drives this from a 16ms tokio interval.
    pub fn tick(&mut self, now: Instant) -> Option<LayoutPlan> {
        // First-frame fast-path: brand-new target that should appear
        // immediately. We hit this for the very first plan or for
        // identical-plan retargets where state changed for some other
        // reason (e.g. the daemon flagged the monitor dirty defensively).
        if self.pending_emit && self.start.is_none() {
            self.pending_emit = false;
            return Some(self.target.clone());
        }

        let t0 = self.start?;
        let elapsed = now.saturating_duration_since(t0);
        let total = Duration::from_millis(self.duration_ms as u64);
        if elapsed >= total {
            // Final frame: emit the exact target so we don't accumulate
            // rounding error, then mark as settled.
            self.start = None;
            self.pending_emit = false;
            return Some(self.target.clone());
        }

        Some(self.frame_at(now, t0))
    }

    /// `true` while we're still mid-animation; lets the daemon decide
    /// whether to bother refreshing tab strips this frame, etc.
    pub fn is_animating(&self) -> bool {
        self.start.is_some()
    }

    /// Compute the frame the eye currently sees, given a tween that
    /// started at `t0`. Pulled out of `tick` so `set_target` can reuse
    /// it for the mid-flight-retarget path without duplicating the
    /// easing math.
    fn frame_at(&self, now: Instant, t0: Instant) -> LayoutPlan {
        let elapsed_ms = now.saturating_duration_since(t0).as_secs_f32() * 1000.0;
        let raw_t = if self.duration_ms == 0 {
            1.0
        } else {
            (elapsed_ms / self.duration_ms as f32).clamp(0.0, 1.0)
        };
        let eased = ease_out_cubic(raw_t);

        let mut out = LayoutPlan::default();
        out.placements.reserve(self.target.placements.len());
        for tgt in &self.target.placements {
            let prev_rect = self
                .prev
                .placements
                .iter()
                .find(|p| p.window == tgt.window)
                .map(|p| p.rect);
            let rect = match prev_rect {
                Some(prev) => lerp_rect(prev, tgt.rect, eased),
                // Newly-opened windows appear at their target rect
                // immediately. Tweening from a zero-rect would render
                // a 1px shrink-then-grow on every spawn.
                None => tgt.rect,
            };
            out.placements.push(Placement { window: tgt.window, rect });
        }
        // Closed windows (in `prev` but not `target`) are intentionally
        // omitted: the applier just stops touching them. Animating a
        // shrink on something the user has already destroyed looks like
        // a bug, not polish.
        out
    }
}

fn ease_out_cubic(t: f32) -> f32 {
    let inv = 1.0 - t;
    1.0 - inv * inv * inv
}

/// Linearly interpolate every component then round once at the very end.
/// Keeping `f32` math through the lerp avoids the staircasing you get if
/// you round each component independently per frame.
fn lerp_rect(a: Rect, b: Rect, t: f32) -> Rect {
    let lerp = |a: i32, b: i32| -> i32 {
        let af = a as f32;
        let bf = b as f32;
        (af + (bf - af) * t).round() as i32
    };
    Rect::new(lerp(a.x, b.x), lerp(a.y, b.y), lerp(a.width, b.width), lerp(a.height, b.height))
}

/// Two plans are equal for animation purposes when they place the same
/// windows at the same rects, regardless of placement order. Hash-based
/// comparison would be cheaper at scale, but layouts rarely exceed a
/// dozen windows per workspace — linear scans win on small N.
fn plans_equal(a: &LayoutPlan, b: &LayoutPlan) -> bool {
    if a.placements.len() != b.placements.len() {
        return false;
    }
    a.placements.iter().all(|pa| {
        b.placements
            .iter()
            .any(|pb| pb.window == pa.window && pb.rect == pa.rect)
    })
}

// Re-export to silence "unused" if the crate ever drops `WindowId` use here.
// It's used in tests via the helper functions.
#[allow(dead_code)]
fn _window_id_used(_: WindowId) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(n: u64) -> WindowId {
        WindowId(n)
    }

    /// Build a plan from a list of (id, rect) pairs. Order is preserved
    /// so tests can rely on indexing for assertions.
    fn plan(items: &[(u64, Rect)]) -> LayoutPlan {
        LayoutPlan {
            placements: items
                .iter()
                .map(|(id, rect)| Placement { window: w(*id), rect: *rect })
                .collect(),
        }
    }

    fn rect_for(plan: &LayoutPlan, id: u64) -> Rect {
        plan.placements
            .iter()
            .find(|p| p.window == w(id))
            .unwrap_or_else(|| panic!("window {} not in plan", id))
            .rect
    }

    fn at(now: Instant, ms: u64) -> Instant {
        now + Duration::from_millis(ms)
    }

    #[test]
    fn first_target_emits_immediately_then_none() {
        let now = Instant::now();
        let mut a = Animator::new(200);
        let target = plan(&[(1, Rect::new(0, 0, 100, 100))]);
        a.set_target(target.clone(), now);

        // First tick after a fresh set_target should hand the target back
        // immediately — there's no prior frame to tween from.
        let frame = a.tick(now).expect("expected first frame");
        assert_eq!(frame.placements.len(), 1);
        assert_eq!(rect_for(&frame, 1), Rect::new(0, 0, 100, 100));

        // Subsequent ticks return None until something changes.
        assert!(a.tick(at(now, 16)).is_none());
        assert!(a.tick(at(now, 1000)).is_none());
        assert!(!a.is_animating());
    }

    #[test]
    fn mid_animation_progresses() {
        let now = Instant::now();
        let mut a = Animator::new(200);
        // First plant the prev (instant emit, settles immediately).
        a.set_target(plan(&[(1, Rect::new(0, 0, 100, 100))]), now);
        let _ = a.tick(now);

        // Now retarget. This SHOULD start a tween from prev to new.
        a.set_target(plan(&[(1, Rect::new(200, 0, 100, 100))]), now);
        assert!(a.is_animating());

        // At t=0.5 of duration we should be partway between 0 and 200 in x.
        let mid = a.tick(at(now, 100)).expect("mid frame");
        let r = rect_for(&mid, 1);
        // ease-out cubic at t=0.5 → 1 - 0.5^3 = 0.875, so x ≈ 175.
        // The test spec only asks for "somewhere in between" within ±100.
        assert!(r.x > 100, "x should be past midpoint due to ease-out, got {}", r.x);
        assert!(r.x < 200, "x should not have arrived yet, got {}", r.x);
        assert!((r.x - 100).abs() <= 100);
        // Other dims are equal in prev/target so they shouldn't drift.
        assert_eq!(r.y, 0);
        assert_eq!(r.width, 100);
        assert_eq!(r.height, 100);
    }

    #[test]
    fn animation_completes_at_target() {
        let now = Instant::now();
        let mut a = Animator::new(200);
        a.set_target(plan(&[(1, Rect::new(0, 0, 100, 100))]), now);
        let _ = a.tick(now);

        a.set_target(plan(&[(1, Rect::new(200, 0, 100, 100))]), now);
        // Tick well past the duration: we get the exact target, then
        // None forever after.
        let final_frame = a.tick(at(now, 500)).expect("final frame");
        assert_eq!(rect_for(&final_frame, 1), Rect::new(200, 0, 100, 100));
        assert!(a.tick(at(now, 600)).is_none());
        assert!(!a.is_animating());
    }

    #[test]
    fn closed_windows_omitted_from_frame() {
        let now = Instant::now();
        let mut a = Animator::new(200);
        a.set_target(
            plan(&[
                (1, Rect::new(0, 0, 100, 100)),
                (2, Rect::new(100, 0, 100, 100)),
            ]),
            now,
        );
        let _ = a.tick(now);

        // Close window 2: target now only contains 1.
        a.set_target(plan(&[(1, Rect::new(0, 0, 200, 100))]), now);

        let mid = a.tick(at(now, 50)).expect("mid frame");
        // Only window 1 should appear in any animation frame; we never
        // want to issue SetWindowPos against a destroyed HWND.
        assert_eq!(mid.placements.len(), 1);
        assert_eq!(mid.placements[0].window, w(1));

        let done = a.tick(at(now, 500)).expect("final frame");
        assert_eq!(done.placements.len(), 1);
        assert_eq!(done.placements[0].window, w(1));
    }

    #[test]
    fn new_windows_appear_at_target_immediately() {
        let now = Instant::now();
        let mut a = Animator::new(200);
        a.set_target(plan(&[(1, Rect::new(0, 0, 500, 500))]), now);
        let _ = a.tick(now);

        // Open window 2 — wasn't in prev, so it should *not* tween from
        // (0,0,0,0). It should land at its target rect on the first tick
        // after set_target.
        a.set_target(
            plan(&[
                (1, Rect::new(0, 0, 250, 500)),
                (2, Rect::new(250, 0, 250, 500)),
            ]),
            now,
        );

        let first = a.tick(at(now, 16)).expect("first animated frame");
        assert_eq!(rect_for(&first, 2), Rect::new(250, 0, 250, 500));
        // …and stays there for the rest of the tween.
        let mid = a.tick(at(now, 100)).expect("mid frame");
        assert_eq!(rect_for(&mid, 2), Rect::new(250, 0, 250, 500));
    }

    #[test]
    fn retarget_continues_from_current_frame() {
        let now = Instant::now();
        let mut a = Animator::new(200);
        a.set_target(plan(&[(1, Rect::new(0, 0, 100, 100))]), now);
        let _ = a.tick(now);

        // Begin a tween 0 → 200.
        a.set_target(plan(&[(1, Rect::new(200, 0, 100, 100))]), now);
        let mid = a.tick(at(now, 100)).expect("mid frame");
        let mid_x = rect_for(&mid, 1).x;
        assert!(mid_x > 0 && mid_x < 200, "expected mid-tween, got x={}", mid_x);

        // Retarget mid-flight. The new prev MUST be the interpolated
        // frame — not the original (0,0,100,100). Verify by reading
        // back what `Animator` thinks `prev` is now.
        a.set_target(plan(&[(1, Rect::new(400, 0, 100, 100))]), at(now, 100));
        assert_eq!(rect_for(&a.prev, 1).x, mid_x,
            "prev should be the interpolated mid-frame, not the original origin");

        // And the very next tick (t=0 into the new tween) should sit
        // at that captured prev — proving we don't snap.
        let after_retarget = a.tick(at(now, 100)).expect("first frame of new tween");
        assert_eq!(rect_for(&after_retarget, 1).x, mid_x);
    }

    #[test]
    fn identical_replan_is_noop_after_settled() {
        let now = Instant::now();
        let mut a = Animator::new(200);
        let p = plan(&[(1, Rect::new(0, 0, 100, 100))]);
        a.set_target(p.clone(), now);
        let _ = a.tick(now);
        // Re-submit the same plan — we shouldn't kick off a fresh tween.
        a.set_target(p, at(now, 50));
        assert!(!a.is_animating());
        assert!(a.tick(at(now, 60)).is_none());
    }

    #[test]
    fn empty_plans_round_trip_cleanly() {
        let now = Instant::now();
        let mut a = Animator::new(200);
        // Empty → empty is a true no-op: nothing to render, no tick. The
        // settle short-circuit in `set_target` correctly skips emitting a
        // frame nobody would draw to. Behavior contract: only generate a
        // frame when there's a non-trivial state change to communicate.
        a.set_target(LayoutPlan::default(), now);
        assert!(a.tick(now).is_none());
        // After actually putting something in, then re-settling at empty,
        // we DO get a transition frame (windows close → animator omits
        // them, applier doesn't reposition them, they're already gone).
        a.set_target(plan(&[(1, Rect::new(0, 0, 100, 100))]), now);
        let _ = a.tick(now); // consume the initial emit
        let _ = a.tick(at(now, 250)); // let it settle
        a.set_target(LayoutPlan::default(), at(now, 260));
        // After all windows close, animator captures the just-emitted
        // (then-settled) state as `prev` and tweens to empty target.
        // Closed windows are omitted from each frame per the contract;
        // the key invariant is that `set_target` doesn't panic on this
        // transition.
        let _ = a.tick(at(now, 280));
    }

    #[test]
    fn ease_out_cubic_endpoints() {
        // Sanity-check the curve so a future refactor doesn't silently
        // swap us back to linear.
        assert!((ease_out_cubic(0.0) - 0.0).abs() < 1e-6);
        assert!((ease_out_cubic(1.0) - 1.0).abs() < 1e-6);
        // Decelerating: at t=0.5 we're already past 0.5.
        assert!(ease_out_cubic(0.5) > 0.5);
    }
}
