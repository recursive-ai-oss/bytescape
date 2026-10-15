//! The memory the reader may use, charged as the reader goes.
//!
//! The limit a caller sets is on everything the read builds that grows with
//! the volume: the file table, every name and its text, the attribute lists,
//! the directory index, and the tree the scan hands on. The size of the
//! volume is not a measure of that, because a hostile volume can name far
//! more than it holds (a record may carry hundreds of names), so each part is
//! charged when it is made and the read stops with
//! [`NtfsError::TooLarge`](crate::NtfsError::TooLarge) when the total
//! passes the limit.
//!
//! The charge is an estimate of the heap the read holds at its peak. It
//! overshoots by at most what the parsing threads build between two checks (a
//! record's worth each) and by the transient copies a growing vector makes,
//! which are a few chunks of the table at most.

use std::sync::atomic::{AtomicU64, Ordering};

/// What the scan's own tree costs per entry, besides the name's bytes: the
/// node's columns (about 48 bytes at a million nodes) and the applier's
/// tables while it works, rounded up. The memory tests check it from the
/// other side: no read that the limit lets through peaks above the limit.
pub(crate) const TREE_BYTES_PER_ENTRY: u64 = 96;

/// A running total of charged bytes against a limit, shared by the threads
/// that parse the table.
pub(crate) struct Charge {
    limit: u64,
    used: AtomicU64,
}

impl Charge {
    pub fn new(limit: u64, already: u64) -> Charge {
        Charge {
            limit,
            used: AtomicU64::new(already),
        }
    }

    /// Adds `bytes`. `false` when the total is now over the limit (the bytes
    /// stay counted).
    pub fn add(&self, bytes: u64) -> bool {
        // Saturating, so a hostile size can never wrap the total back under
        // the limit.
        let after = self
            .used
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |u| {
                Some(u.saturating_add(bytes))
            })
            .map_or(u64::MAX, |before| before.saturating_add(bytes));
        after <= self.limit
    }

    pub fn exceeded(&self) -> bool {
        self.used.load(Ordering::Relaxed) > self.limit
    }

    pub fn limit(&self) -> u64 {
        self.limit
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn charges_add_up_and_the_limit_sticks() {
        let c = Charge::new(100, 10);
        assert!(c.add(90));
        assert!(!c.exceeded());
        assert!(!c.add(1));
        assert!(c.exceeded());
        assert_eq!(c.limit(), 100);
    }

    #[test]
    fn a_huge_charge_saturates() {
        let c = Charge::new(u64::MAX - 1, 0);
        assert!(c.add(u64::MAX - 2));
        assert!(!c.add(u64::MAX));
        assert!(c.exceeded());
    }
}
