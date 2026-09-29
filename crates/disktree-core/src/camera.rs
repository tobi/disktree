//! A level change, drawn as one camera move.
//!
//! Going into a folder or back out of it swaps one layout for another. Both
//! are drawn for the length of the move: the outer level seen through a
//! window flying between two regions of it, and the inner level laid out to
//! fill the view but placed on the region it stands for in the outer one.
//! Entering, the window closes onto that region, so the inner level ends up
//! filling the view; leaving, it opens back out and the level around it
//! closes in.
//!
//! Everything here is in whichever space the chart lays out in: viewport
//! pixels for a treemap, unit space for a partition.

use std::time::{Duration, Instant};

use crate::treemap::Rect;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LevelTransition {
    /// What a level fills when it is the one on screen.
    pub view: Rect,
    /// Where the inner level sits in the outer one: its folder's body.
    pub focus: Rect,
    /// The window onto the outer level at the start of the move.
    pub from: Rect,
    /// The window onto the outer level at its end.
    pub to: Rect,
    /// Going in, rather than back out.
    pub entering: bool,
    pub started: Instant,
    pub duration: Duration,
}

impl LevelTransition {
    pub fn new(
        view: Rect,
        focus: Rect,
        from: Rect,
        to: Rect,
        entering: bool,
        now: Instant,
    ) -> Self {
        let ratio = |a: f32, b: f32| a / b.max(1e-9);
        let zoom = ratio(from.w, to.w)
            .max(ratio(to.w, from.w))
            .max(ratio(from.h, to.h))
            .max(ratio(to.h, from.h));
        // Longer pulls take a little longer, never enough to feel slow.
        let seconds = zoom.max(1.0).log2().min(4.0).mul_add(0.035, 0.22);
        Self {
            view,
            focus,
            from,
            to,
            entering,
            started: now,
            duration: Duration::from_secs_f32(seconds),
        }
    }

    pub fn is_running(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.started) < self.duration
    }

    /// How far along the move is, eased in and out: `0.0` to `1.0`.
    pub fn progress(&self, now: Instant) -> f32 {
        let t = (now.saturating_duration_since(self.started).as_secs_f32()
            / self.duration.as_secs_f32().max(1e-6))
        .clamp(0.0, 1.0);
        if t < 0.5 {
            4.0 * t * t * t
        } else {
            1.0 - 2.0f32.mul_add(-t, 2.0).powi(3) / 2.0
        }
    }

    /// Where a rectangle of the outer level is drawn at `progress`.
    pub fn outer(&self, rect: Rect, progress: f32) -> Rect {
        let (left, width) =
            axis(self.from.x, self.from.w, self.to.x, self.to.w, progress);
        let (top, height) =
            axis(self.from.y, self.from.h, self.to.y, self.to.h, progress);
        let (sx, sy) = (self.view.w / width, self.view.h / height);
        Rect::new(
            (rect.x - left).mul_add(sx, self.view.x),
            (rect.y - top).mul_add(sy, self.view.y),
            rect.w * sx,
            rect.h * sy,
        )
    }

    /// Where a rectangle of the inner level is drawn at `progress`: placed
    /// on the focus, then seen through the same window as the outer level.
    pub fn inner(&self, rect: Rect, progress: f32) -> Rect {
        let fx = self.focus.w / self.view.w;
        let fy = self.focus.h / self.view.h;
        self.outer(
            Rect::new(
                (rect.x - self.view.x).mul_add(fx, self.focus.x),
                (rect.y - self.view.y).mul_add(fy, self.focus.y),
                rect.w * fx,
                rect.h * fy,
            ),
            progress,
        )
    }
}

/// One axis of the window at progress `k`, from `start`/`length` to
/// `end`/`target`.
///
/// Lengths change geometrically about the one point that sits at the same
/// place in both windows, which is what reads as a steady zoom; a linear
/// change of length would rush at the end.
pub fn axis(
    start: f32,
    length: f32,
    end: f32,
    target: f32,
    k: f32,
) -> (f32, f32) {
    let (start, length, end, target, k) = (
        f64::from(start),
        f64::from(length),
        f64::from(end),
        f64::from(target),
        f64::from(k),
    );
    if length <= 0.0
        || target <= 0.0
        || (length - target).abs() <= 1e-9 * length.max(target)
    {
        return (
            (end - start).mul_add(k, start) as f32,
            (target - length).mul_add(k, length) as f32,
        );
    }
    let current = length * (target / length).powf(k);
    let fixed = end.mul_add(length, -(start * target)) / (length - target);
    (
        ((start - fixed) * current / length + fixed) as f32,
        current as f32,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(left: Rect, right: Rect) -> bool {
        (left.x - right.x).abs() < 1e-3
            && (left.y - right.y).abs() < 1e-3
            && (left.w - right.w).abs() < 1e-3
            && (left.h - right.h).abs() < 1e-3
    }

    /// Entering, the window closes from the whole view onto the folder: the
    /// outer level starts where it was and the inner one ends filling the
    /// view.
    #[test]
    fn a_level_change_starts_on_the_outer_level_and_lands_on_the_inner_one() {
        let view = Rect::new(0.0, 0.0, 800.0, 600.0);
        let focus = Rect::new(400.0, 300.0, 200.0, 150.0);
        let now = Instant::now();
        let change = LevelTransition::new(view, focus, view, focus, true, now);
        let tile = Rect::new(100.0, 100.0, 50.0, 50.0);
        assert!(close(change.outer(tile, 0.0), tile));
        assert!(
            close(change.inner(view, 0.0), focus),
            "the inner level starts where its folder was"
        );
        assert!(close(change.outer(focus, 1.0), view));
        assert!(
            close(change.inner(tile, 1.0), tile),
            "and lands exactly on its layout"
        );
        assert!(change.is_running(now + Duration::from_millis(10)));
        assert!(!change.is_running(now + Duration::from_secs(1)));
        assert!(change.progress(now).abs() < f32::EPSILON);
        assert!(
            (change.progress(now + Duration::from_secs(1)) - 1.0).abs()
                < f32::EPSILON
        );
    }

    /// A steady zoom: halfway through, the window is the geometric mean of
    /// its ends, and the point both windows share stays put.
    #[test]
    fn a_level_change_zooms_geometrically() {
        let (x, w) = axis(0.0, 800.0, 400.0, 200.0, 0.5);
        assert!((w - 400.0).abs() < 1e-3, "{w}");
        // The point at the same place in both windows: 800 · 400 / 600.
        let fixed = 800.0 * 400.0 / 600.0;
        assert!(((fixed - x) / w - fixed / 800.0).abs() < 1e-4);
        let (pan_x, pan_w) = axis(0.0, 5.0, 2.0, 5.0, 0.5);
        assert!(
            (pan_x - 1.0).abs() < 1e-6 && (pan_w - 5.0).abs() < 1e-6,
            "equal lengths slide"
        );
    }
}
