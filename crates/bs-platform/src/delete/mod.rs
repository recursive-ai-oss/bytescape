//! Moving items to the Recycle Bin or Trash, putting them back, emptying a
//! drive's bin, and deleting for good.
//!
//! # What is promised
//!
//! - **Nothing moves before every item has been checked.** [`Disposal::precheck`]
//!   judges each item against the protected-path table (both as written and
//!   as the place the path really leads to), against the bin's rules, and
//!   against what the caller saw earlier. [`Disposal::trash`] and
//!   [`Disposal::delete_permanently`] run the same check and then re-check
//!   each item immediately before touching it.
//! - **The strictest rule wins.** One blocked item refuses the whole
//!   selection; one protected item without its confirmation does too. The
//!   [`Plan`] names the items that decided it.
//! - **The answer is the truth.** An item that the system would delete for good
//!   instead of recycling is never moved: it is reported as
//!   [`Outcome::WouldBePermanent`] so the caller can ask. If Windows deletes an
//!   item for good anyway, the outcome is [`Outcome::Deleted`], never
//!   [`Outcome::MovedToBin`].
//! - **Links are things, not roads.** A symbolic link, junction or other
//!   reparse point is moved or removed as itself. Nothing below it is
//!   touched, and a folder on another filesystem is never entered.
//! - **A changed item is left alone.** When the caller supplies what it saw
//!   ([`Identity`]) and the path now names something else, the item is
//!   refused.
//!
//! # Using it
//!
//! 1. Build one [`Disposal`] ([`Disposal::system`]) and keep it.
//! 2. Call [`Disposal::precheck`] when the user picks Delete. A blocked item
//!    refuses the selection; a protected one needs the hold-to-confirm
//!    gesture, whose result is a [`ProtectedConfirmation`] naming those paths.
//! 3. Call [`Disposal::trash`]. For each [`Outcome::WouldBePermanent`], tell
//!    the user why and, if they agree, pass those items to
//!    [`Disposal::delete_permanently`].
//! 4. Say what happened from the outcome alone: a moved item is "moved", never
//!    "freed"; only [`Outcome::Deleted`] gives bytes back. [`Disposal::restore`]
//!    takes the token from [`Outcome::MovedToBin`].
//!
//! Items should carry what the scan saw ([`identity_of`] gives it, for example
//! when the item is selected), so that an item replaced in the meantime is
//! refused instead of deleted.
//!
//! # Linux
//!
//! The Trash follows the freedesktop.org specification: the home Trash for
//! items on the same filesystem as the data folder, otherwise
//! `$topdir/.Trash/$uid` (only when `.Trash` is a real, sticky folder) or
//! `$topdir/.Trash-$uid`. The `.trashinfo` record is created exclusively
//! before the item is renamed into `files/`; nothing is ever copied between
//! filesystems. When no Trash can be used for an item the answer is
//! [`PermanentReason::NoTrashOnVolume`].
//!
//! # Windows
//!
//! Items go through `IFileOperation` with the recycle flag and no user
//! interface. Before that, the item's size is compared with the volume's
//! Recycle Bin limit and the "don't move files to the Recycle Bin" setting;
//! afterwards the shell's own report says whether a recycled copy exists.

use crate::drives::{Drive, DriveError};
use bs_knowledge::{Environment, Knowledge};
use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};

pub use bs_knowledge::{Level, Reason};

#[cfg(target_os = "linux")]
pub(crate) mod trashinfo;

#[cfg(target_os = "linux")]
mod fsys;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(windows)]
mod windows;
#[cfg(any(windows, test))]
mod winpure;

#[cfg(all(test, target_os = "linux"))]
mod tests;

pub(crate) use bs_fsops::validate_path;
pub use bs_fsops::{BadPath, Identity, MAX_PATH_BYTES};

/// The identity of whatever `path` names right now. A link is described, not
/// followed.
pub fn identity_of(path: &Path) -> io::Result<Identity> {
    #[cfg(target_os = "linux")]
    {
        linux::identity_of(path)
    }
    #[cfg(windows)]
    {
        windows::identity_of(path)
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = path;
        Err(io::Error::from(io::ErrorKind::Unsupported))
    }
}

/// One thing to delete.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    /// The absolute path, as the scan reported it.
    pub path: PathBuf,
    /// What the scan (or the selection) saw there. `None` skips the
    /// replaced-since-scan check; the other checks still run.
    pub seen: Option<Identity>,
}

impl Item {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            seen: None,
        }
    }

    /// Records what was seen at the path.
    #[must_use]
    pub fn seen(mut self, identity: Identity) -> Self {
        self.seen = Some(identity);
        self
    }
}

/// Proof that the user held the confirmation for these protected paths. The
/// interface creates it when the hold-to-confirm gesture completes, naming the
/// paths the dialog listed; it covers exactly those paths and nothing else.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProtectedConfirmation {
    paths: BTreeSet<PathBuf>,
}

impl ProtectedConfirmation {
    pub fn for_paths<P: Into<PathBuf>>(paths: impl IntoIterator<Item = P>) -> Self {
        Self {
            paths: paths.into_iter().map(Into::into).collect(),
        }
    }

    fn covers(&self, path: &Path) -> bool {
        self.paths.contains(path)
    }
}

/// The note for an item that is blocked only because a blocked place lies
/// inside it ([`Reason::HoldsBlocked`]).
pub const HOLDS_BLOCKED_NOTE: &str = "Holds a part of the operating system.";

/// Why an item was not touched.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum RefuseReason {
    /// Part of the operating system: never offered for deletion.
    Blocked {
        reason: Reason,
        note: Option<&'static str>,
    },
    /// Protected, and the confirmation does not cover this path.
    ProtectedNotConfirmed {
        note: Option<&'static str>,
    },
    /// Another item in the selection is blocked or needs a confirmation it
    /// does not have, so the whole selection is refused. `driver` is the index
    /// of that item.
    SelectionRefused {
        driver: usize,
    },
    /// The path names something other than what the caller saw.
    ChangedSinceScan,
    /// Nothing exists at the path.
    NotFound,
    BadPath(BadPath),
    /// The path is outside the area this [`Disposal`] is limited to.
    OutsideRestriction,
    /// The path cannot be turned into the place it really leads to.
    CannotResolve,
    /// The item is the root of a mounted filesystem.
    MountPoint,
    /// The item is the same as, or inside, another item of the selection
    /// (`with` is its index).
    Overlaps {
        with: usize,
    },
    /// Restoring: something already exists at the original path.
    OriginalExists,
    /// Restoring: the folder the item came from is gone.
    OriginalFolderMissing,
    /// The drive has no Recycle Bin or Trash to empty (a network share or a
    /// removable drive).
    NoBinOnDrive,
    /// Restoring: the item is no longer in the bin, or is not the one the
    /// token names.
    NotInBin,
}

/// Why an item cannot go to the bin.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum PermanentReason {
    /// The item is larger than the bin's size limit.
    TooLargeForBin { item_bytes: u64, limit_bytes: u64 },
    /// The volume is set to delete without using the bin.
    BinDisabled,
    /// The volume has no bin that can be used: Linux has no usable Trash
    /// folder for it, or the drive (a network share, a FAT stick) has no
    /// Recycle Bin.
    NoTrashOnVolume,
    /// The item cannot be moved to the Trash without copying it between
    /// filesystems.
    CrossesFilesystem,
    /// The system could not say which volume holds the item, so whether it
    /// has a bin is unknown.
    BinUnknown,
}

/// A failed operation. When part of a folder was removed before the failure,
/// the counts say how much.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpError {
    /// What was being done.
    pub step: &'static str,
    pub kind: io::ErrorKind,
    /// The system's error code (errno, or the Windows error or HRESULT).
    pub code: Option<i32>,
    /// Bytes already removed from the drive by this operation.
    pub removed_bytes: u64,
    /// Files and folders already removed by this operation.
    pub removed_entries: u64,
}

impl OpError {
    pub(crate) fn io(step: &'static str, error: &io::Error) -> Self {
        Self {
            step,
            kind: error.kind(),
            code: error.raw_os_error(),
            removed_bytes: 0,
            removed_entries: 0,
        }
    }

    #[cfg(windows)]
    pub(crate) fn code(step: &'static str, code: i32) -> Self {
        Self {
            step,
            kind: io::ErrorKind::Other,
            code: Some(code),
            removed_bytes: 0,
            removed_entries: 0,
        }
    }
}

impl std::fmt::Display for OpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} failed ({:?}", self.step, self.kind)?;
        if let Some(code) = self.code {
            write!(f, ", code {code}")?;
        }
        f.write_str(")")?;
        if self.removed_entries > 0 {
            write!(f, "; {} entries were already removed", self.removed_entries)?;
        }
        Ok(())
    }
}

impl std::error::Error for OpError {}

/// What is needed to put an item back. It is only good for the bin entry it
/// was made for.
///
/// The token remembers the item's real place: the path with every link in it
/// resolved, which can differ from the path the caller gave. On Windows it is
/// in plain form (`C:\Users\...`, without the `\\?\` prefix), except for the
/// rare path that has no plain form. A caller that wants to know which of its
/// items a restore concerns keeps the token with that item, and does not
/// compare path strings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestoreToken {
    original: PathBuf,
    inner: TokenInner,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum TokenInner {
    #[cfg(target_os = "linux")]
    Trash(linux::TrashToken),
    #[cfg(windows)]
    Recycled(windows::RecycledToken),
    // Keeps the enum inhabited on systems without a bin.
    #[allow(dead_code)]
    None,
}

impl RestoreToken {
    /// Where the item was before it was moved: its real place, in the form
    /// described on [`RestoreToken`].
    pub fn original_path(&self) -> &Path {
        &self.original
    }
}

/// Which size a byte count is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ByteBasis {
    /// Space the filesystem reserved (Linux). This is what the map shows.
    Allocated,
    /// The sizes of the files. Windows' Recycle Bin reports these, so a figure
    /// shown beside the map's allocated sizes must say so.
    Logical,
}

/// What happened to one item.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Outcome {
    /// The item is in the bin and can be put back with the token. The bytes
    /// are still on the drive: nothing was freed.
    MovedToBin {
        token: RestoreToken,
        /// What the item takes, with hard-linked files counted once. `basis`
        /// says which kind of size this is.
        bytes: u64,
        basis: ByteBasis,
        /// Files in the item that also have a name outside the item.
        shared_files: u64,
    },
    /// The item is gone for good. Either it was deleted permanently as asked,
    /// or the system deleted it instead of recycling it.
    Deleted {
        /// Bytes given back to the drive.
        bytes: u64,
        /// Files that still exist under another name, so their bytes were
        /// not given back.
        shared_files: u64,
    },
    /// The item was not moved because it would be deleted for good. Ask the
    /// user, then use the permanent flow.
    WouldBePermanent {
        reason: PermanentReason,
    },
    Refused {
        reason: RefuseReason,
    },
    Failed {
        error: OpError,
    },
}

/// The outcome of putting an item back.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum RestoreOutcome {
    /// The item is back. `path` is the real place it is at now: links
    /// resolved, and on Windows in plain form (no `\\?\` prefix). It can be
    /// spelled differently from the path the item was given with, so a caller
    /// matches it to an item by the token it kept, not by comparing strings.
    Restored {
        path: PathBuf,
    },
    Refused {
        reason: RefuseReason,
    },
    Failed {
        error: OpError,
    },
}

/// The outcome of emptying a drive's bin.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum EmptyOutcome {
    /// Everything in the drive's bin is gone.
    Emptied {
        /// Bytes given back to the drive, where the system tells (Windows
        /// reports the bin's own size).
        bytes: u64,
        entries: u64,
    },
    /// Some of it is gone; `error` says what stopped the rest.
    Partial {
        bytes: u64,
        entries: u64,
        error: OpError,
    },
    Refused {
        reason: RefuseReason,
    },
    Failed {
        error: OpError,
    },
}

/// What the check decided for one item.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Decision {
    /// The item may be moved (or deleted, for the permanent check).
    Proceed,
    WouldBePermanent(PermanentReason),
    Refused(RefuseReason),
}

/// The check of one item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanEntry {
    /// How firmly the protected-path table guards the item.
    pub level: Level,
    pub reason: Reason,
    /// What the table says about the path, for the confirmation dialog.
    pub note: Option<&'static str>,
    pub decision: Decision,
}

/// The check of a whole selection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    /// One entry per item, in the order given.
    pub entries: Vec<PlanEntry>,
    /// The strictest level in the selection.
    pub strictest: Level,
    /// The indexes of the items that decided the selection's level: all items
    /// at the strictest level.
    pub drivers: Vec<usize>,
    /// Nothing will be touched: an item is blocked, or an item needs a
    /// confirmation it does not have.
    pub selection_refused: bool,
}

/// Whether a check is for the Trash or for a permanent delete.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Trash,
    Permanent,
}

/// An item after the path was looked up.
#[derive(Clone, Debug)]
pub(crate) struct Resolved {
    /// The place the path really leads to.
    pub real: PathBuf,
    pub identity: Identity,
    /// The name is where another mount starts (or another filesystem).
    pub on_mount_point: bool,
    /// The folder that holds the item, kept open from the lookup to the act.
    #[cfg(target_os = "linux")]
    pub(crate) held: linux::Held,
}

/// Why a path could not be looked up.
#[derive(Debug)]
pub(crate) enum ResolveError {
    NotFound,
    CannotResolve,
}

/// The service: it holds the protected-path table, the drives, and any limit
/// on where it may act.
pub struct Disposal {
    knowledge: Knowledge,
    drives: Vec<Drive>,
    restriction: Option<PathBuf>,
    #[cfg(target_os = "linux")]
    trash_env: crate::bins::linux::Env,
    #[cfg(target_os = "linux")]
    topdir_override: Option<PathBuf>,
}

impl Disposal {
    /// The service for this computer: its drives, its environment and its
    /// protected-path table.
    pub fn system() -> Result<Self, DriveError> {
        let drives = crate::drives::list_drives()?;
        let environment = Environment::from_lookup(|name| std::env::var_os(name))
            .with_drive_roots(roots(&drives));
        Ok(Self::new(Knowledge::new(&environment), drives))
    }

    /// A service for the given table and drives. The table should know every
    /// drive root (see `Environment::with_drive_roots`).
    pub fn new(knowledge: Knowledge, drives: Vec<Drive>) -> Self {
        Self {
            knowledge,
            drives,
            restriction: None,
            #[cfg(target_os = "linux")]
            trash_env: crate::bins::linux::Env::system(),
            #[cfg(target_os = "linux")]
            topdir_override: None,
        }
    }

    /// Limits every operation to paths inside `root`: anything else is
    /// refused as [`RefuseReason::OutsideRestriction`], whatever else is true
    /// of it. The root is compared with the place each path really leads to.
    #[must_use]
    pub fn restricted_to(mut self, root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        self.restriction = Some(std::fs::canonicalize(&root).unwrap_or(root));
        self
    }

    /// Whether `path` (already the place it really is) is inside the area the
    /// service is limited to, or there is no limit.
    fn within_restriction(&self, path: &Path) -> bool {
        let Some(root) = &self.restriction else {
            return true;
        };
        #[cfg(windows)]
        {
            winpure::is_inside(&winpure::units_of_path(root), &winpure::units_of_path(path))
        }
        #[cfg(not(windows))]
        {
            path.starts_with(root)
        }
    }

    /// The drives this service knows.
    pub fn drives(&self) -> &[Drive] {
        &self.drives
    }

    /// Uses the given user id and data folder instead of the process's, so a
    /// test never reads or changes the real Trash.
    #[cfg(all(test, target_os = "linux"))]
    fn with_trash_env(mut self, uid: u32, data_home: Option<PathBuf>) -> Self {
        self.trash_env = crate::bins::linux::Env { uid, data_home };
        self
    }

    /// Treats `topdir` as the mount point of every item's volume. Mount points
    /// cannot be created in a test, so the volume logic takes the folder.
    #[cfg(all(test, target_os = "linux"))]
    fn with_topdir(mut self, topdir: impl Into<PathBuf>) -> Self {
        self.topdir_override = Some(topdir.into());
        self
    }

    /// Checks a selection for moving to the bin. Nothing is changed.
    pub fn precheck(&self, items: &[Item], confirmed: Option<&ProtectedConfirmation>) -> Plan {
        self.plan(items, confirmed, Mode::Trash).0
    }

    /// Checks a selection for a permanent delete. Nothing is changed.
    pub fn precheck_permanent(
        &self,
        items: &[Item],
        confirmed: Option<&ProtectedConfirmation>,
    ) -> Plan {
        self.plan(items, confirmed, Mode::Permanent).0
    }

    /// Moves the items to the bin, one outcome per item in the order given.
    ///
    /// The whole selection is checked first; if it is refused, nothing moves.
    /// Items reported as [`Outcome::WouldBePermanent`] are not moved.
    pub fn trash(&self, items: &[Item], confirmed: Option<&ProtectedConfirmation>) -> Vec<Outcome> {
        self.run(items, confirmed, Mode::Trash)
    }

    /// Deletes the items for good, one outcome per item in the order given.
    /// The same checks as [`Disposal::trash`] apply, except the bin's.
    pub fn delete_permanently(
        &self,
        items: &[Item],
        confirmed: Option<&ProtectedConfirmation>,
    ) -> Vec<Outcome> {
        self.run(items, confirmed, Mode::Permanent)
    }

    fn run(
        &self,
        items: &[Item],
        confirmed: Option<&ProtectedConfirmation>,
        mode: Mode,
    ) -> Vec<Outcome> {
        let (plan, resolved) = self.plan(items, confirmed, mode);
        let mut outcomes = Vec::with_capacity(items.len());
        for (index, item) in items.iter().enumerate() {
            let outcome = match (&plan.entries[index].decision, &resolved[index]) {
                (Decision::Refused(reason), _) => Outcome::Refused {
                    reason: reason.clone(),
                },
                (Decision::WouldBePermanent(reason), _) => Outcome::WouldBePermanent {
                    reason: reason.clone(),
                },
                (Decision::Proceed, Some(resolved)) => {
                    self.execute(item, resolved, confirmed, mode)
                }
                (Decision::Proceed, None) => Outcome::Refused {
                    reason: RefuseReason::CannotResolve,
                },
            };
            outcomes.push(outcome);
        }
        outcomes
    }

    /// Looks every item up and decides. Also gives back what was looked up so
    /// that the execution can compare against it.
    fn plan(
        &self,
        items: &[Item],
        confirmed: Option<&ProtectedConfirmation>,
        mode: Mode,
    ) -> (Plan, Vec<Option<Resolved>>) {
        let mut entries = Vec::with_capacity(items.len());
        let mut resolved: Vec<Option<Resolved>> = Vec::with_capacity(items.len());
        for item in items {
            let (entry, found) = self.check_one(item, confirmed);
            entries.push(entry);
            resolved.push(found);
        }

        // An item inside another item (or listed twice) is left out: the outer
        // one takes it along, and moving both would fail halfway.
        for later in 0..items.len() {
            let Some(inner) = resolved[later].as_ref().map(|r| r.real.clone()) else {
                continue;
            };
            for (earlier, outer) in resolved.iter().enumerate() {
                if earlier == later || !matches!(entries[later].decision, Decision::Proceed) {
                    continue;
                }
                let Some(outer) = outer.as_ref() else {
                    continue;
                };
                let same = outer.real == inner;
                // For equal paths only the later one is refused.
                if (inner.starts_with(&outer.real) && !same) || (same && earlier < later) {
                    entries[later].decision =
                        Decision::Refused(RefuseReason::Overlaps { with: earlier });
                }
            }
        }

        let strictest = entries
            .iter()
            .map(|e| e.level)
            .max()
            .unwrap_or(Level::Normal);
        let drivers: Vec<usize> = entries
            .iter()
            .enumerate()
            .filter(|(_, e)| strictest > Level::Normal && e.level == strictest)
            .map(|(i, _)| i)
            .collect();

        // Blocked items, protected ones without their confirmation, and paths
        // that make no sense refuse the whole selection.
        let deciding = entries.iter().position(|e| {
            matches!(
                e.decision,
                Decision::Refused(
                    RefuseReason::Blocked { .. }
                        | RefuseReason::ProtectedNotConfirmed { .. }
                        | RefuseReason::BadPath(_)
                )
            )
        });
        let selection_refused = deciding.is_some();
        if let Some(driver) = deciding {
            for (index, entry) in entries.iter_mut().enumerate() {
                if index != driver && matches!(entry.decision, Decision::Proceed) {
                    entry.decision = Decision::Refused(RefuseReason::SelectionRefused { driver });
                }
            }
        }

        // The bin's own rules apply only to what is still going ahead.
        if mode == Mode::Trash && !selection_refused {
            for (index, entry) in entries.iter_mut().enumerate() {
                if let (Decision::Proceed, Some(found)) = (&entry.decision, &resolved[index])
                    && let Some(why) = self.bin_objection(&items[index].path, found)
                {
                    entry.decision = Decision::WouldBePermanent(why);
                }
            }
        }

        (
            Plan {
                entries,
                strictest,
                drivers,
                selection_refused,
            },
            resolved,
        )
    }

    /// How firmly the table guards a path, counting what a delete of the whole
    /// item would take with it: a folder that holds a blocked place is blocked
    /// too, so `/var` cannot be removed on the strength of a confirmation while
    /// the package databases under it are blocked.
    fn verdict_for(&self, path: &Path) -> bs_knowledge::Verdict {
        let own = self.knowledge.protection(path);
        match self.knowledge.blocked_below(path) {
            // The matching entry describes the inner place, so it is not passed
            // on: its note would say what `/var/lib/dpkg` is under `/var`.
            Some(_) if own.level < Level::Blocked => bs_knowledge::Verdict {
                level: Level::Blocked,
                reason: Reason::HoldsBlocked,
                entry: None,
            },
            _ => own,
        }
    }

    /// Looks one item up and applies the per-item rules.
    fn check_one(
        &self,
        item: &Item,
        confirmed: Option<&ProtectedConfirmation>,
    ) -> (PlanEntry, Option<Resolved>) {
        let entry = |level, reason, note, decision| PlanEntry {
            level,
            reason,
            note,
            decision,
        };
        if let Err(bad) = validate_path(&item.path) {
            // A path in an unusable shape is judged as blocked, so the
            // selection it belongs to is not carried out.
            return (
                entry(
                    Level::Blocked,
                    Reason::Unresolvable,
                    None,
                    Decision::Refused(RefuseReason::BadPath(bad)),
                ),
                None,
            );
        }
        let text = self.verdict_for(&item.path);
        let found = match self.resolve(&item.path) {
            Ok(found) => found,
            Err(error) => {
                let reason = match error {
                    ResolveError::NotFound => RefuseReason::NotFound,
                    ResolveError::CannotResolve => RefuseReason::CannotResolve,
                };
                // The text alone still decides how strict the rule is.
                return Self::judged(text, Decision::Refused(reason), None, item, confirmed);
            }
        };
        // The real place may be stricter than the text: a link in the middle
        // of the path can lead into the system.
        let real = self.verdict_for(&found.real);
        let verdict = if real.level > text.level { real } else { text };

        if !self.within_restriction(&found.real) {
            return Self::judged(
                verdict,
                Decision::Refused(RefuseReason::OutsideRestriction),
                None,
                item,
                confirmed,
            );
        }
        if let Some(seen) = item.seen
            && seen != found.identity
        {
            return Self::judged(
                verdict,
                Decision::Refused(RefuseReason::ChangedSinceScan),
                None,
                item,
                confirmed,
            );
        }
        if found.on_mount_point {
            return Self::judged(
                verdict,
                Decision::Refused(RefuseReason::MountPoint),
                None,
                item,
                confirmed,
            );
        }
        Self::judged(verdict, Decision::Proceed, Some(found), item, confirmed)
    }

    /// Applies the protected-path verdict on top of a decision.
    fn judged(
        verdict: bs_knowledge::Verdict,
        decision: Decision,
        found: Option<Resolved>,
        item: &Item,
        confirmed: Option<&ProtectedConfirmation>,
    ) -> (PlanEntry, Option<Resolved>) {
        let note = match verdict.reason {
            Reason::HoldsBlocked => Some(HOLDS_BLOCKED_NOTE),
            _ => verdict.entry.and_then(|e| e.note),
        };
        let decision = match verdict.level {
            Level::Blocked => Decision::Refused(RefuseReason::Blocked {
                reason: verdict.reason,
                note,
            }),
            Level::Protected if !confirmed.is_some_and(|c| c.covers(&item.path)) => {
                Decision::Refused(RefuseReason::ProtectedNotConfirmed { note })
            }
            _ => decision,
        };
        let found = matches!(decision, Decision::Proceed)
            .then_some(found)
            .flatten();
        (
            PlanEntry {
                level: verdict.level,
                reason: verdict.reason,
                note,
                decision,
            },
            found,
        )
    }

    /// Looks the item up again just before it is touched, and does the work.
    fn execute(
        &self,
        item: &Item,
        planned: &Resolved,
        confirmed: Option<&ProtectedConfirmation>,
        mode: Mode,
    ) -> Outcome {
        // Re-check here: the path may have been swapped for something else
        // since the plan was made, and the table must judge what is there now.
        let (entry, now) = self.check_one(item, confirmed);
        let Some(now) = now.filter(|n| n.identity == planned.identity && n.real == planned.real)
        else {
            return Outcome::Refused {
                reason: match entry.decision {
                    Decision::Refused(reason) => reason,
                    _ => RefuseReason::ChangedSinceScan,
                },
            };
        };
        match mode {
            Mode::Trash => self.trash_one(item, &now),
            Mode::Permanent => self.delete_one(item, &now),
        }
    }

    /// Puts an item back where it was.
    ///
    /// On success, [`RestoreOutcome::Restored`] holds the real path where the
    /// item now is: links resolved, and on Windows in plain form (no `\\?\`
    /// prefix). It can be spelled differently from the path the item was given
    /// with, so match the result to an item by the token, not by the string.
    pub fn restore(&self, token: &RestoreToken) -> RestoreOutcome {
        if self.restriction.is_some() {
            // The token holds the real place the item came from.
            if !self.within_restriction(&token.original) {
                return RestoreOutcome::Refused {
                    reason: RefuseReason::OutsideRestriction,
                };
            }
        }
        self.restore_one(token)
    }

    /// Empties the bin of one drive: only that drive's Trash folders on Linux,
    /// only the drive's Recycle Bin on Windows.
    pub fn empty_bin(&self, drive: &Drive) -> EmptyOutcome {
        self.empty_one(drive)
    }
}

/// Every mount point of every drive.
fn roots(drives: &[Drive]) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for drive in drives {
        roots.push(drive.mount_point.clone());
        roots.extend(drive.mounts.iter().map(|m| m.mount_point.clone()));
    }
    roots
}

/// The knowledge-table environment for a set of drives, for callers that build
/// their own [`Knowledge`].
pub fn environment_for(drives: &[Drive]) -> Environment {
    Environment::from_lookup(|name| std::env::var_os(name)).with_drive_roots(roots(drives))
}

#[cfg(test)]
mod shape_tests {
    use super::*;

    #[test]
    fn paths_must_be_absolute_and_plain() {
        let cases: [(&str, Result<(), BadPath>); 6] = [
            ("", Err(BadPath::Empty)),
            ("relative/file", Err(BadPath::NotAbsolute)),
            ("/a/../b", Err(BadPath::NotCanonical)),
            ("/a/./b", Err(BadPath::NotCanonical)),
            ("/a/b/..", Err(BadPath::NotCanonical)),
            ("/", Err(BadPath::NoName)),
        ];
        if cfg!(unix) {
            for (path, expected) in cases {
                assert_eq!(validate_path(Path::new(path)), expected, "{path:?}");
            }
            assert_eq!(validate_path(Path::new("/a/b")), Ok(()));
            assert_eq!(validate_path(Path::new("/a//b")), Ok(()));
            let long = format!("/{}", "a".repeat(MAX_PATH_BYTES));
            assert_eq!(validate_path(Path::new(&long)), Err(BadPath::TooLong));
        }
    }

    #[cfg(unix)]
    proptest::proptest! {
        #[test]
        fn any_bytes_make_a_path_that_is_judged_without_panicking(
            bytes in proptest::collection::vec(proptest::prelude::any::<u8>(), 0..200)
        ) {
            use std::os::unix::ffi::OsStringExt;
            let path = PathBuf::from(std::ffi::OsString::from_vec(bytes));
            if validate_path(&path).is_ok() {
                // Whatever passes the shape check is absolute, has a last name
                // and has no dot parts.
                proptest::prop_assert!(path.is_absolute());
                proptest::prop_assert!(path.file_name().is_some());
                for part in path.as_os_str().as_encoded_bytes().split(|&b| b == b'/') {
                    proptest::prop_assert!(part != b"." && part != b"..");
                }
            }
        }
    }

    #[test]
    fn confirmation_covers_only_its_paths() {
        let c = ProtectedConfirmation::for_paths(["/a/b", "/c"]);
        assert!(c.covers(Path::new("/a/b")));
        assert!(!c.covers(Path::new("/a")));
        assert!(!c.covers(Path::new("/a/b/c")));
        assert!(!ProtectedConfirmation::default().covers(Path::new("/c")));
    }

    #[test]
    fn errors_say_what_was_already_gone() {
        let error = OpError {
            step: "delete",
            kind: io::ErrorKind::PermissionDenied,
            code: Some(13),
            removed_bytes: 10,
            removed_entries: 3,
        };
        let text = error.to_string();
        assert!(
            text.contains("code 13") && text.contains("3 entries"),
            "{text}"
        );
    }
}
