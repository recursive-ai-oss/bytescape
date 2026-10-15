//! The capacity bar: used, Recycle Bin, unreadable, reserved and free bytes of
//! one drive, and the animation between two states.

use std::time::Duration;

use bs_platform::drives::{Capacity, ReservedKind};
use bs_render::SegmentKind;

/// Length of a change of the bar.
pub const CHANGE_DURATION: Duration = Duration::from_millis(250);

/// How the drive's bytes divide up.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Breakdown {
    pub used: u64,
    pub recycle_bin: u64,
    pub unreadable: u64,
    pub reserved: u64,
    pub free: u64,
    pub total: u64,
    /// The reserved bytes are a per-user quota, not a filesystem reserve.
    pub quota: bool,
}

impl Breakdown {
    /// Splits a drive's bytes.
    ///
    /// `counted` is what the scan has counted so far. The unreadable part is
    /// the gap between that and what the system reports as used, and it only
    /// exists once the walk has finished: until then the uncounted bytes are
    /// simply not counted yet. Bytes in the Recycle Bin sit inside the used
    /// bytes, so they are carved out of them.
    pub fn new(cap: &Capacity, counted: u64, scan_complete: bool, recycle_bin: u64) -> Self {
        let unreadable = if scan_complete {
            cap.uncounted_bytes(counted)
        } else {
            0
        };
        let bin = recycle_bin.min(cap.used.saturating_sub(unreadable));
        Self {
            used: cap.used.saturating_sub(unreadable).saturating_sub(bin),
            recycle_bin: bin,
            unreadable,
            reserved: cap.reserved,
            free: cap.free,
            total: cap.total,
            quota: matches!(cap.reserved_kind, ReservedKind::UserQuota),
        }
    }

    /// Fractions of the whole drive, in drawing order.
    pub fn parts(&self) -> Parts {
        let total = self.total.max(1) as f64;
        let f = |b: u64| (b as f64 / total) as f32;
        Parts([
            f(self.used),
            f(self.recycle_bin),
            f(self.unreadable),
            f(self.reserved),
            f(self.free),
        ])
    }

    /// The text for the bar's tooltip.
    pub fn describe(&self) -> String {
        self.describe_as(false)
    }

    /// The tooltip text. After a full scan the gap that remains is the
    /// filesystem's own overhead, not bytes that could not be read
    /// (`overhead`).
    pub fn describe_as(&self, overhead: bool) -> String {
        let size = bs_layout::format_size;
        let reserved = if self.quota {
            "Beyond your disk quota"
        } else {
            "Reserved by filesystem"
        };
        let mut parts = vec![format!("Used {}", size(self.used))];
        if self.recycle_bin > 0 {
            parts.push(format!("Recycle Bin {}", size(self.recycle_bin)));
        }
        if self.unreadable > 0 {
            let what = if overhead {
                "Filesystem overhead"
            } else {
                "Couldn't read"
            };
            parts.push(format!("{what} {}", size(self.unreadable)));
        }
        if self.reserved > 0 {
            parts.push(format!("{reserved} {}", size(self.reserved)));
        }
        parts.push(format!("Free {}", size(self.free)));
        parts.join(" · ")
    }
}

/// Fractions of the drive for used, Recycle Bin, unreadable, reserved and
/// free.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Parts(pub [f32; 5]);

impl Parts {
    const KINDS: [SegmentKind; 5] = [
        SegmentKind::Used,
        SegmentKind::RecycleBin,
        SegmentKind::Unreadable,
        SegmentKind::Reserved,
        SegmentKind::Free,
    ];

    /// The parts as the renderer draws them.
    pub fn segments(&self) -> Vec<(SegmentKind, f32)> {
        Self::KINDS.into_iter().zip(self.0).collect()
    }

    fn lerp(self, to: Parts, t: f32) -> Parts {
        let mut out = [0.0; 5];
        for (o, (a, b)) in out.iter_mut().zip(self.0.into_iter().zip(to.0)) {
            *o = a + (b - a) * t;
        }
        Parts(out)
    }
}

/// The bar's current shape and the change in progress.
#[derive(Clone, Debug, Default)]
pub struct CapacityModel {
    breakdown: Option<Breakdown>,
    from: Parts,
    to: Parts,
    started: Duration,
    changing: bool,
}

impl CapacityModel {
    /// The latest figures, if any have arrived.
    pub fn breakdown(&self) -> Option<&Breakdown> {
        self.breakdown.as_ref()
    }

    /// Moves the bar to new figures. The first figures appear at once; later
    /// ones glide over [`CHANGE_DURATION`], or jump when `reduced_motion`.
    pub fn set(&mut self, breakdown: Breakdown, now: Duration, reduced_motion: bool) {
        let target = breakdown.parts();
        let first = self.breakdown.is_none();
        let shown = self.parts_at(now);
        self.breakdown = Some(breakdown);
        if target == self.to {
            return;
        }
        if first || reduced_motion {
            self.from = target;
            self.to = target;
            self.changing = false;
        } else {
            self.from = shown;
            self.to = target;
            self.started = now;
            self.changing = true;
        }
    }

    /// The fractions to draw at `now`.
    pub fn parts_at(&self, now: Duration) -> Parts {
        if !self.changing {
            return self.to;
        }
        let t = (now.saturating_sub(self.started).as_secs_f32() / CHANGE_DURATION.as_secs_f32())
            .clamp(0.0, 1.0);
        let eased = 1.0 - (1.0 - t).powi(3);
        self.from.lerp(self.to, eased)
    }

    /// Whether the bar is still moving at `now`.
    pub fn is_changing(&self, now: Duration) -> bool {
        self.changing && now.saturating_sub(self.started) < CHANGE_DURATION
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cap() -> Capacity {
        // 1000 total: 600 used, 100 reserved, 300 free.
        Capacity::from_bytes(1000, 400, 300, ReservedKind::FilesystemReserve)
    }

    #[test]
    fn the_gap_is_unreadable_only_after_the_walk() {
        let during = Breakdown::new(&cap(), 450, false, 0);
        assert_eq!((during.used, during.unreadable), (600, 0));
        let after = Breakdown::new(&cap(), 450, true, 0);
        assert_eq!((after.used, after.unreadable), (450, 150));
        // Everything adds up to the drive.
        assert_eq!(
            after.used + after.unreadable + after.reserved + after.free,
            1000
        );
    }

    #[test]
    fn counting_more_than_the_system_reports_leaves_no_gap() {
        let b = Breakdown::new(&cap(), 900, true, 0);
        assert_eq!((b.unreadable, b.used), (0, 600));
    }

    #[test]
    fn the_recycle_bin_comes_out_of_the_used_bytes() {
        let b = Breakdown::new(&cap(), 450, true, 80);
        assert_eq!((b.used, b.recycle_bin, b.unreadable), (370, 80, 150));
        assert_eq!(
            b.used + b.recycle_bin + b.unreadable + b.reserved + b.free,
            1000
        );
        // A bin larger than anything it could hold is held to the used bytes.
        let huge = Breakdown::new(&cap(), 450, true, 10_000);
        assert_eq!(huge.recycle_bin, 450);
    }

    #[test]
    fn parts_sum_to_one_and_follow_the_drawing_order() {
        let p = Breakdown::new(&cap(), 450, true, 80).parts();
        let sum: f32 = p.0.iter().sum();
        assert!((sum - 1.0).abs() < 1e-5);
        assert_eq!(p.segments()[2].0, SegmentKind::Unreadable);
    }

    #[test]
    fn the_description_names_a_quota_truthfully() {
        let mut c = cap();
        c.reserved_kind = ReservedKind::UserQuota;
        let text = Breakdown::new(&c, 450, true, 0).describe();
        assert!(text.contains("Beyond your disk quota"), "{text}");
        assert!(!text.contains("Reserved by filesystem"));
        let text = Breakdown::new(&cap(), 450, true, 0).describe();
        assert!(text.contains("Reserved by filesystem"), "{text}");
    }

    #[test]
    fn changes_glide_over_250_ms_and_reduced_motion_jumps() {
        let ms = Duration::from_millis;
        let mut m = CapacityModel::default();
        m.set(Breakdown::new(&cap(), 450, true, 0), ms(0), false);
        // The first figures are shown at once.
        assert!(!m.is_changing(ms(0)));
        let before = m.parts_at(ms(0));
        m.set(Breakdown::new(&cap(), 450, true, 100), ms(1000), false);
        assert!(m.is_changing(ms(1100)));
        let mid = m.parts_at(ms(1125));
        let end = m.parts_at(ms(1250));
        assert!(mid.0[1] > before.0[1] && mid.0[1] < end.0[1], "{mid:?}");
        assert!(!m.is_changing(ms(1250)));
        assert!((end.0[1] - 0.1).abs() < 1e-5);
        let mut r = CapacityModel::default();
        r.set(Breakdown::new(&cap(), 450, true, 0), ms(0), true);
        r.set(Breakdown::new(&cap(), 450, true, 100), ms(1000), true);
        assert!(!r.is_changing(ms(1001)));
        assert!((r.parts_at(ms(1001)).0[1] - 0.1).abs() < 1e-5);
    }
}
