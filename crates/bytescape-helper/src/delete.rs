//! Deleting what the interface lists, and nothing else.
//!
//! The interface is not trusted to have filtered anything. Every path is
//! judged here, against the same compiled table the interface uses, and
//! judged twice: as the text it was sent, and as the place it really is, read
//! from the operating system's record of what was opened. The strictest
//! reading wins. Only then is anything touched, and just before each item is
//! touched it is looked up and judged again.
//!
//! The rules, in the order they apply to one item:
//!
//! 1. The request must be for a permanent delete.
//! 2. The path must be absolute and plain: no `.` or `..` part, no NUL, not
//!    too long, not a root.
//! 3. A blocked path (an operating-system folder, a drive root, or a folder
//!    that holds one) is refused without looking at the disk.
//! 4. The item is opened without following a link at its last name. The place
//!    it really is must be the path that was sent: a path that goes through a
//!    link is refused, not followed. That place is judged too.
//! 5. A protected path is refused unless the request says the user held the
//!    confirmation for that path.
//! 6. An item that is not what the interface saw, or is where another
//!    filesystem starts, is refused.
//!
//! Across the request the strictest rule applies: one blocked item, one
//! unconfirmed protected item or one malformed path refuses every other item
//! as well, so a selection is never half deleted because of its own mistake.
//! An item inside another item of the request is left to the outer one.

use bs_fsops::{DeleteError, OpenError, Target, validate_path};
use bs_ipc::{DeleteItem, DeleteRequest, ErrorCode, ItemOutcome, Refusal, path_from_wire};
use bs_knowledge::{Knowledge, Level};
use std::path::{Path, PathBuf};

/// Judges and deletes.
pub struct Judge {
    knowledge: Knowledge,
    /// Why this helper must not delete anything at all, when it must not.
    unavailable: Option<(ErrorCode, &'static str)>,
}

impl Judge {
    /// A judge that uses `knowledge`, which must know every drive root of this
    /// computer (see `Environment::with_drive_roots`).
    pub fn new(knowledge: Knowledge) -> Self {
        Self {
            knowledge,
            unavailable: None,
        }
    }

    /// A judge whose table is not complete enough to be trusted with a delete:
    /// the session answers a delete request with `code` and `why` and touches
    /// nothing.
    #[must_use]
    pub fn unavailable(mut self, code: ErrorCode, why: &'static str) -> Self {
        self.unavailable = Some((code, why));
        self
    }

    /// Why deleting is refused outright, if it is.
    pub fn refusal(&self) -> Option<(ErrorCode, &'static str)> {
        self.unavailable
    }

    /// How firmly the table guards a path, counting what a delete of the whole
    /// item would take with it: a folder that holds a blocked place is blocked
    /// too, so `/var` cannot be removed on the strength of a confirmation while
    /// the package databases under it are blocked.
    fn level_of(&self, path: &Path) -> Level {
        let own = self.knowledge.protection(path).level;
        match self.knowledge.blocked_below(path) {
            Some(_) if own < Level::Blocked => Level::Blocked,
            _ => own,
        }
    }

    fn look(&self, item: &DeleteItem) -> Result<Target, Refusal> {
        let path = path_from_wire(&item.path).map_err(|_| Refusal::BadPath)?;
        validate_path(&path).map_err(|_| Refusal::BadPath)?;
        // Judged as sent, before the disk is touched at all.
        let as_sent = self.level_of(&path);
        if as_sent >= Level::Blocked {
            return Err(Refusal::Blocked);
        }
        let target = Target::open(&path).map_err(|e| match e {
            OpenError::NotFound => Refusal::NotFound,
            _ => Refusal::CannotResolve,
        })?;
        // The real place may be stricter than the text: a link in the middle
        // of the path can lead into the system.
        let level = as_sent.max(self.level_of(target.real_path()));
        if level >= Level::Blocked {
            return Err(Refusal::Blocked);
        }
        if !same_place(target.real_path(), &path) {
            return Err(Refusal::NotRealPath);
        }
        if level >= Level::Protected && !item.confirmed_protected {
            return Err(Refusal::ProtectedNotConfirmed);
        }
        if let Some(seen) = item.seen {
            let now = target.identity();
            if now.volume != seen.volume || now.file != seen.file {
                return Err(Refusal::ChangedSinceScan);
            }
        }
        if target.on_mount_point() {
            return Err(Refusal::MountPoint);
        }
        Ok(target)
    }

    /// Judges every item and then deletes those that may go, calling `report`
    /// with each item's outcome as soon as it is known, in the order given.
    ///
    /// Returns how many items were reported: `request.items.len()` unless
    /// `proceed` stopped the run or `report` refused an outcome, and zero, with
    /// nothing reported and nothing touched, when the judge is
    /// [unavailable](Judge::unavailable).
    ///
    /// `proceed` is asked before each item is touched; when it says no, the
    /// rest of the items are left alone and not reported.
    pub fn run(
        &self,
        request: &DeleteRequest,
        mut proceed: impl FnMut() -> bool,
        mut report: impl FnMut(usize, ItemOutcome) -> bool,
    ) -> usize {
        let count = request.items.len();
        if self.unavailable.is_some() {
            // The session has already said why; nothing is touched here either.
            return 0;
        }
        if !request.permanent {
            // Nothing is checked or touched for a request that is not what the
            // helper does.
            for index in 0..count {
                if !report(index, ItemOutcome::Refused(Refusal::NotPermanent)) {
                    return index;
                }
            }
            return count;
        }
        let mut checked: Vec<Result<Target, Refusal>> =
            request.items.iter().map(|i| self.look(i)).collect();

        // An item inside another item (or listed twice) is left out: the outer
        // one takes it along, and deleting both would fail halfway.
        let reals: Vec<Option<PathBuf>> = checked
            .iter()
            .map(|c| c.as_ref().ok().map(|t| t.real_path().to_path_buf()))
            .collect();
        let mut overlaps: Vec<Option<usize>> = vec![None; count];
        for ((later, inner), slot) in reals.iter().enumerate().zip(overlaps.iter_mut()) {
            let Some(inner) = inner else { continue };
            for (earlier, outer) in reals.iter().enumerate() {
                let Some(outer) = outer else { continue };
                if earlier == later {
                    continue;
                }
                let same = same_place(inner, outer);
                // For equal paths only the later one is refused.
                if (contains(outer, inner) && !same) || (same && earlier < later) {
                    slot.get_or_insert(earlier);
                }
            }
        }
        for (entry, overlap) in checked.iter_mut().zip(overlaps) {
            if let Some(with) = overlap {
                *entry = Err(Refusal::Overlaps {
                    with: u32::try_from(with).unwrap_or(u32::MAX),
                });
            }
        }

        // Blocked items, protected ones without their confirmation, and paths
        // that make no sense refuse the whole request.
        let deciding = checked.iter().position(|c| {
            matches!(
                c,
                Err(Refusal::Blocked | Refusal::ProtectedNotConfirmed | Refusal::BadPath)
            )
        });
        if let Some(driver) = deciding {
            let driver = u32::try_from(driver).unwrap_or(u32::MAX);
            for entry in &mut checked {
                if entry.is_ok() {
                    *entry = Err(Refusal::SelectionRefused { driver });
                }
            }
        }

        for (index, (entry, item)) in checked.into_iter().zip(&request.items).enumerate() {
            if !proceed() {
                return index;
            }
            let outcome = match entry {
                Err(reason) => ItemOutcome::Refused(reason),
                Ok(planned) => self.execute(item, &planned),
            };
            if !report(index, outcome) {
                return index;
            }
        }
        count
    }

    /// Looks the item up again just before it is touched, and deletes it.
    fn execute(&self, item: &DeleteItem, planned: &Target) -> ItemOutcome {
        // Re-check here: the path may have been swapped for something else since
        // the plan was made, and the table must judge what is there now.
        let now = match self.look(item) {
            Ok(now) => now,
            Err(reason) => return ItemOutcome::Refused(reason),
        };
        if now.identity() != planned.identity() || !same_place(now.real_path(), planned.real_path())
        {
            return ItemOutcome::Refused(Refusal::ChangedSinceScan);
        }
        match now.delete() {
            Ok(removed) => ItemOutcome::Deleted {
                bytes: removed.allocated,
                entries: removed.entries,
                shared_files: removed.kept_by_other_names,
            },
            Err(DeleteError::NotFound) => ItemOutcome::Refused(Refusal::NotFound),
            Err(DeleteError::Replaced) => ItemOutcome::Refused(Refusal::ChangedSinceScan),
            Err(DeleteError::Look(error)) => ItemOutcome::Failed {
                step: 1,
                os_code: error.raw_os_error().unwrap_or(0),
                removed_bytes: 0,
                removed_entries: 0,
            },
            Err(DeleteError::Partial { removed, error }) => ItemOutcome::Failed {
                step: 2,
                os_code: error.raw_os_error().unwrap_or(0),
                removed_bytes: removed.allocated,
                removed_entries: removed.entries,
            },
            // The error type may grow; anything new is a failure, never a
            // success.
            Err(_) => ItemOutcome::Failed {
                step: 2,
                os_code: 0,
                removed_bytes: 0,
                removed_entries: 0,
            },
        }
    }
}

#[cfg(windows)]
fn units(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str().encode_wide().collect()
}

/// Whether two paths spell the same place, byte for byte on Linux and as
/// Windows compares names there.
fn same_place(a: &Path, b: &Path) -> bool {
    #[cfg(windows)]
    {
        // On the exact units: a name with an unpaired surrogate is not the
        // name with the replacement character in its place.
        bs_fsops::winpath::same_path_units(&units(a), &units(b))
    }
    #[cfg(not(windows))]
    {
        a.as_os_str() == b.as_os_str()
    }
}

/// Whether `inner` is `outer` or lies below it.
fn contains(outer: &Path, inner: &Path) -> bool {
    #[cfg(windows)]
    {
        bs_fsops::winpath::is_inside_units(&units(outer), &units(inner))
    }
    #[cfg(not(windows))]
    {
        inner.starts_with(outer)
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests;

#[cfg(all(test, windows))]
#[path = "delete/tests_windows.rs"]
mod tests_windows;
