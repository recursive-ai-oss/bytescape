//! Turns the stream of wheel, touchpad and pinch input into folder steps.
//!
//! One step is one level. A wheel notch is a deliberate step, but a touchpad
//! fling or a long pinch delivers dozens of small events, some of them
//! inertia that goes on after the fingers left. Unchecked, that would dive
//! through half the tree before the user could look. The rules, in the order
//! they apply:
//!
//! * Deltas add up until they reach one step; the remainder is dropped, so a
//!   slow drag never carries stale movement into the next step.
//! * After a step, input in the same direction is ignored for
//!   [`STEP_LOCKOUT`], as long as the zoom animation, so every level is seen
//!   before the next one starts.
//! * A burst of precise input (touchpad, pinch) moves at most
//!   [`MAX_LEVELS_PER_GESTURE`] levels. A gesture ends when no event arrives
//!   for [`GESTURE_IDLE`], which inertia never allows while it lasts. A wheel
//!   notch is not capped, so rolling a wheel notch by notch can go as deep as
//!   the user likes.
//! * Turning the other way starts a new gesture at once: going back needs no
//!   waiting.

use std::time::Duration;

/// Input in the same direction is ignored this long after a step.
pub const STEP_LOCKOUT: Duration = Duration::from_millis(150);
/// A gesture of precise input moves at most this many levels.
pub const MAX_LEVELS_PER_GESTURE: u32 = 3;
/// Silence of this length ends a gesture.
pub const GESTURE_IDLE: Duration = Duration::from_millis(250);
/// Pinch scale change, as winit reports it, that counts as one level.
pub const PINCH_PER_LEVEL: f32 = 0.25;

/// What kind of input produced a delta.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// A wheel notch: deliberate, one event per click.
    Notch,
    /// A touchpad or a pinch: many small deltas.
    Precise,
}

/// Coalesces and rate-limits scroll input.
#[derive(Clone, Debug, Default)]
pub struct WheelGate {
    sum: f32,
    direction: i8,
    last_event: Option<Duration>,
    last_step: Option<Duration>,
    levels: u32,
}

impl WheelGate {
    /// Feeds one delta (`steps` > 0 is deeper, < 0 is up) at time `now`.
    /// Returns `1` or `-1` when the view should step that way, else `0`.
    pub fn feed(&mut self, steps: f32, source: Source, now: Duration) -> i32 {
        if !steps.is_finite() || steps == 0.0 {
            return 0;
        }
        let direction: i8 = if steps > 0.0 { 1 } else { -1 };
        let quiet = self
            .last_event
            .is_none_or(|t| now.saturating_sub(t) >= GESTURE_IDLE);
        if quiet || direction != self.direction {
            *self = WheelGate::default();
        }
        self.direction = direction;
        self.last_event = Some(now);
        if source == Source::Precise && self.levels >= MAX_LEVELS_PER_GESTURE {
            return 0;
        }
        if self
            .last_step
            .is_some_and(|t| now.saturating_sub(t) < STEP_LOCKOUT)
        {
            return 0;
        }
        self.sum += steps.abs();
        if self.sum < 1.0 {
            return 0;
        }
        self.sum = 0.0;
        self.levels += 1;
        self.last_step = Some(now);
        i32::from(direction)
    }

    /// Forgets the gesture in progress (the view changed some other way).
    pub fn reset(&mut self) {
        *self = WheelGate::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn a_notch_is_one_step_and_the_next_waits_for_the_lockout() {
        let mut g = WheelGate::default();
        assert_eq!(g.feed(1.0, Source::Notch, ms(0)), 1);
        // Ten notches in 100 ms: only the first counts.
        for i in 1..10 {
            assert_eq!(g.feed(1.0, Source::Notch, ms(i * 10)), 0);
        }
        assert_eq!(g.feed(1.0, Source::Notch, ms(160)), 1);
    }

    #[test]
    fn notches_are_not_capped_per_gesture() {
        let mut g = WheelGate::default();
        let steps: i32 = (0..8)
            .map(|i| g.feed(1.0, Source::Notch, ms(i * 160)))
            .sum();
        assert_eq!(steps, 8);
    }

    #[test]
    fn small_touchpad_deltas_add_up_to_one_step() {
        let mut g = WheelGate::default();
        let mut got = Vec::new();
        for i in 0..10 {
            got.push(g.feed(0.3, Source::Precise, ms(i * 8)));
        }
        // 0.3 + 0.3 + 0.3 + 0.3 reaches 1.0 on the fourth event; the lockout
        // then swallows the rest of the burst.
        assert_eq!(got.iter().filter(|&&s| s == 1).count(), 1);
        assert_eq!(got[3], 1);
    }

    #[test]
    fn a_fling_moves_a_bounded_number_of_levels() {
        let mut g = WheelGate::default();
        // 120 events 8 ms apart: a second of inertia, each worth 0.4 step.
        let steps: i32 = (0..120)
            .map(|i| g.feed(0.4, Source::Precise, ms(i * 8)))
            .sum();
        assert_eq!(steps, MAX_LEVELS_PER_GESTURE as i32);
    }

    #[test]
    fn a_new_gesture_after_silence_moves_again() {
        let mut g = WheelGate::default();
        let first: i32 = (0..120)
            .map(|i| g.feed(0.4, Source::Precise, ms(i * 8)))
            .sum();
        assert_eq!(first, 3);
        let start = 120 * 8 + 300;
        let second: i32 = (0..120)
            .map(|i| g.feed(0.4, Source::Precise, ms(start + i * 8)))
            .sum();
        assert_eq!(second, 3);
    }

    #[test]
    fn turning_around_starts_at_once() {
        let mut g = WheelGate::default();
        assert_eq!(g.feed(1.0, Source::Notch, ms(0)), 1);
        assert_eq!(g.feed(-1.0, Source::Notch, ms(20)), -1);
    }

    #[test]
    fn a_slow_drag_does_not_leave_stale_movement_behind() {
        let mut g = WheelGate::default();
        assert_eq!(g.feed(0.6, Source::Precise, ms(0)), 0);
        // Silence ends the gesture; the 0.6 is forgotten.
        assert_eq!(g.feed(0.6, Source::Precise, ms(400)), 0);
    }

    #[test]
    fn nonsense_deltas_are_ignored() {
        let mut g = WheelGate::default();
        assert_eq!(g.feed(f32::NAN, Source::Notch, ms(0)), 0);
        assert_eq!(g.feed(f32::INFINITY, Source::Notch, ms(1)), 0);
        assert_eq!(g.feed(0.0, Source::Notch, ms(2)), 0);
        assert_eq!(g.feed(1.0, Source::Notch, ms(3)), 1);
    }

    #[test]
    fn a_large_precise_delta_is_still_one_level() {
        let mut g = WheelGate::default();
        assert_eq!(g.feed(7.5, Source::Precise, ms(0)), 1);
    }
}
