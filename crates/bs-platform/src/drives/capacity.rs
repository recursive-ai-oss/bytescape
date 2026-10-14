//! Size figures for one drive.

/// Why some free bytes are not available to the current user.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ReservedKind {
    /// The filesystem holds the space back for the administrator (ext4's root
    /// reserve, for example). Linux reports it as `f_bfree - f_bavail`.
    FilesystemReserve,
    /// A per-user disk quota withholds the space. The bytes are free on the
    /// volume but the current user may not write them.
    UserQuota,
}

/// How full a drive is, in bytes.
///
/// The four byte figures always add up: `total == used + reserved + free`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capacity {
    /// The size of the whole volume. A per-user quota does not shrink it.
    pub total: u64,
    /// Bytes in use, as the operating system counts them. This is what scan
    /// progress is measured against.
    ///
    /// It already leaves out `reserved`: on Linux it is `f_blocks - f_bfree`,
    /// on Windows the volume size minus the volume's whole free space. The
    /// bytes a scan could not count are therefore `used - counted`, with no
    /// further subtraction of `reserved`; [`Capacity::uncounted_bytes`] does
    /// that sum.
    pub used: u64,
    /// Bytes the current user can still write.
    pub free: u64,
    /// Free bytes the current user cannot use; `reserved_kind` says why.
    pub reserved: u64,
    /// The reason for `reserved`. It is set by the backend that produced the
    /// figures, so a label can say "Reserved by filesystem" or "Over your
    /// quota limit" without guessing from the operating system.
    pub reserved_kind: ReservedKind,
}

impl Capacity {
    /// Builds the figures from the whole-volume byte counts: the size of the
    /// volume, all its free bytes, and the free bytes available to the user.
    ///
    /// `reserved_kind` says why the difference between the last two is
    /// withheld. Inconsistent input is clamped rather than trusted: free cannot
    /// exceed the total, and the user's share cannot exceed all free bytes.
    pub fn from_bytes(
        total: u64,
        total_free: u64,
        available_to_user: u64,
        reserved_kind: ReservedKind,
    ) -> Self {
        let total_free = total_free.min(total);
        let free = available_to_user.min(total_free);
        Self {
            total,
            used: total - total_free,
            free,
            reserved: total_free - free,
            reserved_kind,
        }
    }

    /// Builds the figures from `statvfs` block counts: the block size, the
    /// number of blocks, free blocks (`f_bfree`) and blocks available to an
    /// unprivileged user (`f_bavail`). The difference is the filesystem's
    /// reserve.
    ///
    /// Returns `None` when the block size is zero or the size does not fit in
    /// 64 bits, since no honest figure exists then. Free counts that exceed
    /// the block count, or an available count above the free count, are
    /// clamped.
    pub fn from_blocks(
        unit: u64,
        blocks: u64,
        free_blocks: u64,
        avail_blocks: u64,
    ) -> Option<Self> {
        if unit == 0 {
            return None;
        }
        let total = blocks.checked_mul(unit)?;
        let free_blocks = free_blocks.min(blocks);
        // Both products are at most `total`, which fits.
        let total_free = free_blocks * unit;
        let available = avail_blocks.min(free_blocks) * unit;
        Some(Self::from_bytes(
            total,
            total_free,
            available,
            ReservedKind::FilesystemReserve,
        ))
    }

    /// Builds the figures from a Windows disk-space report in allocation
    /// units: the size of the whole volume and its free space, both ignoring
    /// quotas, and the free space left for the calling user, which a quota
    /// can reduce. Free space withheld by a quota is `reserved`.
    ///
    /// Returns `None` when the unit size is zero or a size does not fit in 64
    /// bits.
    #[cfg(any(windows, test))]
    pub(crate) fn from_allocation_units(
        unit: u64,
        actual_total: u64,
        actual_available: u64,
        caller_available: u64,
    ) -> Option<Self> {
        if unit == 0 {
            return None;
        }
        let total = actual_total.checked_mul(unit)?;
        let actual_available = actual_available.min(actual_total);
        let total_free = actual_available * unit;
        let available = caller_available.min(actual_available) * unit;
        Some(Self::from_bytes(
            total,
            total_free,
            available,
            ReservedKind::UserQuota,
        ))
    }

    /// Builds the figures from `GetDiskFreeSpaceExW`, for systems that lack
    /// the disk-space report above.
    ///
    /// That call's total is the size the calling user may use, which a quota
    /// makes smaller than the volume, and it gives no other source for the
    /// real size. So the figures are only trusted when no quota shows. A quota
    /// shows when the whole volume has more free space than the user's total
    /// (so the total cannot be the volume), or when the user can write less
    /// than the volume has free. In those cases this returns `None`, because
    /// the volume's used bytes cannot be told. A quota that is not yet
    /// limiting leaves no trace in these three numbers, so it is not detected.
    #[cfg(any(windows, test))]
    pub(crate) fn from_user_scoped_bytes(
        total_for_user: u64,
        total_free: u64,
        available_to_user: u64,
    ) -> Option<Self> {
        if total_free > total_for_user || available_to_user < total_free {
            return None;
        }
        Some(Self::from_bytes(
            total_for_user,
            total_free,
            available_to_user,
            ReservedKind::UserQuota,
        ))
    }

    /// The bytes the operating system counts as used that a scan did not
    /// reach, given the allocated bytes the scan counted. This is the
    /// Unreadable gap. `reserved` is not part of the sum, because `used`
    /// already leaves it out. Counting more than the system reports (hard
    /// links, shared extents) gives 0, not a negative number.
    pub fn uncounted_bytes(&self, counted_allocated: u64) -> u64 {
        self.used.saturating_sub(counted_allocated)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn sums(c: &Capacity) -> bool {
        c.used
            .checked_add(c.reserved)
            .and_then(|x| x.checked_add(c.free))
            == Some(c.total)
    }

    #[test]
    fn ext4_with_root_reserve() {
        // 1000 blocks of 4096, 300 free, 250 usable by a normal user.
        let c = Capacity::from_blocks(4096, 1000, 300, 250).unwrap();
        assert_eq!(c.total, 4_096_000);
        assert_eq!(c.used, 700 * 4096);
        assert_eq!(c.reserved, 50 * 4096);
        assert_eq!(c.free, 250 * 4096);
        assert_eq!(c.reserved_kind, ReservedKind::FilesystemReserve);
    }

    #[test]
    fn whole_volume_figures_carry_the_given_reason() {
        let plain = Capacity::from_bytes(1000, 400, 400, ReservedKind::UserQuota);
        assert_eq!((plain.used, plain.reserved, plain.free), (600, 0, 400));
        let held_back = Capacity::from_bytes(1000, 400, 100, ReservedKind::FilesystemReserve);
        assert_eq!(
            (held_back.used, held_back.reserved, held_back.free),
            (600, 300, 100)
        );
        assert_eq!(held_back.reserved_kind, ReservedKind::FilesystemReserve);
    }

    /// A 1000 GB disk with 400 GB free, where the user has a 50 GB quota with
    /// 40 GB of it left. The disk-space report gives the real volume total and
    /// free space whatever the quota is.
    #[test]
    fn windows_quota_keeps_the_real_volume_size() {
        const GB: u64 = 1_000_000_000;
        // Allocation units of 4 KiB would make the numbers awkward; use 1 GB units.
        let c = Capacity::from_allocation_units(GB, 1000, 400, 40).unwrap();
        assert_eq!(c.total, 1000 * GB);
        assert_eq!(c.used, 600 * GB, "used is the volume's, not the user's");
        assert_eq!(c.free, 40 * GB);
        assert_eq!(c.reserved, 360 * GB);
        assert_eq!(c.reserved_kind, ReservedKind::UserQuota);
        assert!(sums(&c));
    }

    #[test]
    fn windows_without_a_quota_has_nothing_reserved() {
        let c = Capacity::from_allocation_units(4096, 1000, 400, 400).unwrap();
        assert_eq!((c.used, c.reserved, c.free), (600 * 4096, 0, 400 * 4096));
        assert_eq!(c.reserved_kind, ReservedKind::UserQuota);
    }

    #[test]
    fn allocation_units_reject_what_has_no_honest_answer() {
        assert_eq!(Capacity::from_allocation_units(0, 10, 5, 5), None);
        assert_eq!(Capacity::from_allocation_units(u64::MAX, 2, 1, 1), None);
    }

    #[test]
    fn the_fallback_figures_are_trusted_only_when_no_quota_shows() {
        // No quota: the user's total is the volume's.
        let c = Capacity::from_user_scoped_bytes(1000, 400, 400).unwrap();
        assert_eq!((c.total, c.used, c.free, c.reserved), (1000, 600, 400, 0));
        // The documented quota shape: the user's total (50) is below the
        // volume's free space (400). The old code clamped this into a 50 GB
        // drive with nothing used.
        assert_eq!(Capacity::from_user_scoped_bytes(50, 400, 40), None);
        // A quota whose remainder is below the volume's free space.
        assert_eq!(Capacity::from_user_scoped_bytes(1000, 400, 100), None);
    }

    #[test]
    fn gap_is_used_minus_counted_without_subtracting_the_reserve_again() {
        // The build host's `/`: 10 GiB reserved, 131 GiB used.
        const GIB: u64 = 1 << 30;
        let c = Capacity {
            total: 196 * GIB,
            used: 131 * GIB,
            free: 55 * GIB,
            reserved: 10 * GIB,
            reserved_kind: ReservedKind::FilesystemReserve,
        };
        // The scan counted 100 GiB of allocated bytes.
        assert_eq!(c.uncounted_bytes(100 * GIB), 31 * GIB);
        assert_ne!(
            c.uncounted_bytes(100 * GIB),
            21 * GIB,
            "reserved is not subtracted again"
        );
        // Over-counting (hard links) bottoms out at zero.
        assert_eq!(c.uncounted_bytes(200 * GIB), 0);
    }

    #[test]
    fn nonsense_is_clamped() {
        // bavail above bfree, bfree above blocks.
        let c = Capacity::from_blocks(512, 10, 20, 30).unwrap();
        assert!(sums(&c));
        assert_eq!((c.used, c.reserved, c.free), (0, 0, 10 * 512));
    }

    #[test]
    fn zero_unit_and_overflow_have_no_answer() {
        assert_eq!(Capacity::from_blocks(0, 10, 5, 5), None);
        assert_eq!(Capacity::from_blocks(u64::MAX, 2, 1, 1), None);
    }

    #[test]
    fn empty_filesystem() {
        let c = Capacity::from_blocks(4096, 0, 0, 0).unwrap();
        assert_eq!(c.total, 0);
        assert!(sums(&c));
    }

    proptest! {
        #[test]
        fn blocks_never_panic_and_always_add_up(
            unit in any::<u64>(),
            blocks in any::<u64>(),
            free in any::<u64>(),
            avail in any::<u64>(),
        ) {
            if let Some(c) = Capacity::from_blocks(unit, blocks, free, avail) {
                prop_assert!(sums(&c));
                prop_assert!(c.free <= c.total && c.used <= c.total && c.reserved <= c.total);
            }
        }

        #[test]
        fn bytes_never_panic_and_always_add_up(
            total in any::<u64>(),
            total_free in any::<u64>(),
            avail in any::<u64>(),
        ) {
            let c = Capacity::from_bytes(total, total_free, avail, ReservedKind::UserQuota);
            prop_assert!(sums(&c));
        }

        #[test]
        fn allocation_units_never_panic_and_always_add_up(
            unit in any::<u64>(),
            total in any::<u64>(),
            actual in any::<u64>(),
            caller in any::<u64>(),
        ) {
            if let Some(c) = Capacity::from_allocation_units(unit, total, actual, caller) {
                prop_assert!(sums(&c));
            }
        }

        #[test]
        fn user_scoped_never_panics_and_adds_up(
            total in any::<u64>(),
            total_free in any::<u64>(),
            avail in any::<u64>(),
        ) {
            if let Some(c) = Capacity::from_user_scoped_bytes(total, total_free, avail) {
                prop_assert!(sums(&c));
            }
        }

        #[test]
        fn uncounted_never_exceeds_used(used in any::<u64>(), counted in any::<u64>()) {
            let c = Capacity { total: u64::MAX, used, free: 0, reserved: 0, reserved_kind: ReservedKind::FilesystemReserve };
            prop_assert!(c.uncounted_bytes(counted) <= used);
        }
    }
}
