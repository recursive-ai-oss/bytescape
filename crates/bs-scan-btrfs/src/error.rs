//! What can go wrong, in two sizes: an error that stops the whole read, and a
//! problem with one structure, which marks that part of the tree and lets the
//! rest of the read go on.

use std::fmt;
use std::io;

/// Why the file system could not be read at all.
#[derive(Debug)]
#[non_exhaustive]
pub enum BtrfsError {
    /// The kernel refused a request, or the source could not be read, where
    /// the read cannot go on without it. Searching a tree needs administrator
    /// rights.
    Io(io::Error),
    /// The path is not on a btrfs file system.
    NotBtrfs,
    /// The tree search handed back something that cannot be a tree search
    /// result: an item running past the buffer, keys out of order.
    Hostile(&'static str),
    /// The directory where the scan starts is missing or unusable, so there is
    /// no tree.
    BadRoot(&'static str),
    /// The file system would need more memory than the reader's limit.
    TooLarge(&'static str),
    /// A mount that belongs to the scan could not be placed in the trees (it
    /// could not be traced to a subvolume, or the folder it covers is not in
    /// the tree). Reading on would present the folder it covers, which the
    /// mount hides, as if it were the mount's contents, so there is no tree
    /// and the caller falls back to the walker.
    MountNotPlaced(&'static str),
}

impl fmt::Display for BtrfsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BtrfsError::Io(e) => write!(f, "cannot search the file system's trees: {e}"),
            BtrfsError::NotBtrfs => f.write_str("not a btrfs file system"),
            BtrfsError::Hostile(why) => write!(f, "implausible tree search result: {why}"),
            BtrfsError::BadRoot(why) => write!(f, "unusable start directory: {why}"),
            BtrfsError::TooLarge(why) => write!(f, "file system too large to read: {why}"),
            BtrfsError::MountNotPlaced(why) => write!(f, "cannot place a mount: {why}"),
        }
    }
}

impl std::error::Error for BtrfsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            BtrfsError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for BtrfsError {
    fn from(e: io::Error) -> Self {
        BtrfsError::Io(e)
    }
}

/// One kind of inconsistency the reader met and worked around.
///
/// Some are damage; some are what a live file system looks like while it
/// changes (a directory entry naming an inode that was created or removed
/// between two pages of the search). Neither is fatal, and neither is ever
/// hidden: the reader counts them, keeps a few examples, and marks the
/// affected directory or entry as unreadable where bytes could be missing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
#[repr(u8)]
pub enum ProblemKind {
    /// An item is shorter than its type needs, or its fields contradict its
    /// length. The item is ignored; the file or directory it belongs to is
    /// marked unreadable.
    ItemMalformed,
    /// An inode has an impossible type.
    InodeInvalid,
    /// Directory or extent items for an inode with no inode item. They are
    /// ignored.
    ItemWithoutInode,
    /// A directory entry names an inode that is not in the tree. On a live
    /// file system this is usually a file removed a moment ago.
    DirentToMissingInode,
    /// A directory entry's recorded type disagrees with its inode's type. The
    /// entry is skipped and its directory is marked unreadable, because the
    /// inode's bytes (and, for a directory, everything below it) are missing
    /// from the tree.
    DirentTypeMismatch,
    /// A directory entry has a name that cannot exist (empty, or containing
    /// `/` or NUL). The entry is skipped and its directory marked unreadable.
    DirentBadName,
    /// A directory is entered by a second entry (a loop, or a hard-linked
    /// directory, which btrfs does not allow). The second entry is skipped
    /// and its directory marked unreadable.
    DirectoryLinkedTwice,
    /// Two file extent items name the same extent with different sizes. The
    /// file is marked unreadable.
    ExtentSizeMismatch,
    /// A nested subvolume's tree could not be read. The subvolume appears as
    /// an unreadable directory.
    SubvolumeUnreadable,
    /// A subvolume is reached by two entries, there are more nested
    /// subvolumes than the reader follows, or the root tree could not say
    /// whether an entry's subvolume is nested where it is named. The extra or
    /// unplaced entry is shown as an unreadable directory.
    SubvolumeSkipped,
    /// The tree is nested deeper than the reader follows.
    TooDeep,
    /// An inode is entered or listed in a tree that failed halfway; nothing
    /// of that tree is kept.
    TreeDiscarded,
    /// A mount that is not part of the scan lies on a path the trees do not
    /// hold (it was made or removed while the trees were read). Nothing is
    /// lost by it: what it covers is not followed anyway.
    MountNotPlaced,
}

impl ProblemKind {
    /// Every kind, in declaration order.
    pub const ALL: [ProblemKind; 13] = [
        ProblemKind::ItemMalformed,
        ProblemKind::InodeInvalid,
        ProblemKind::ItemWithoutInode,
        ProblemKind::DirentToMissingInode,
        ProblemKind::DirentTypeMismatch,
        ProblemKind::DirentBadName,
        ProblemKind::DirectoryLinkedTwice,
        ProblemKind::ExtentSizeMismatch,
        ProblemKind::SubvolumeUnreadable,
        ProblemKind::SubvolumeSkipped,
        ProblemKind::TooDeep,
        ProblemKind::TreeDiscarded,
        ProblemKind::MountNotPlaced,
    ];

    fn index(self) -> usize {
        self as usize
    }
}

/// An example of a problem: what, and where.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Problem {
    /// What kind.
    pub kind: ProblemKind,
    /// The subvolume's tree id.
    pub tree: u64,
    /// The inode it was met in, or 0.
    pub inode: u64,
}

/// How many examples are kept; the counts are exact regardless.
pub const MAX_SAMPLES: usize = 64;

/// Counts of the problems met, and a few examples of them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Diagnostics {
    counts: [u64; ProblemKind::ALL.len()],
    samples: Vec<Problem>,
}

impl Diagnostics {
    pub(crate) fn note(&mut self, kind: ProblemKind, tree: u64, inode: u64) {
        if let Some(count) = self.counts.get_mut(kind.index()) {
            *count = count.saturating_add(1);
        }
        if self.samples.len() < MAX_SAMPLES {
            self.samples.push(Problem { kind, tree, inode });
        }
    }

    /// Adds another set of counts and examples to this one.
    pub(crate) fn absorb(&mut self, other: &Diagnostics) {
        for (mine, theirs) in self.counts.iter_mut().zip(other.counts.iter()) {
            *mine = mine.saturating_add(*theirs);
        }
        for sample in &other.samples {
            if self.samples.len() < MAX_SAMPLES {
                self.samples.push(*sample);
            }
        }
    }

    /// How many times `kind` happened.
    pub fn count(&self, kind: ProblemKind) -> u64 {
        self.counts.get(kind.index()).copied().unwrap_or(0)
    }

    /// How many problems of any kind.
    pub fn total(&self) -> u64 {
        self.counts.iter().fold(0u64, |a, &c| a.saturating_add(c))
    }

    /// The first few problems, in the order they were met.
    pub fn samples(&self) -> &[Problem] {
        &self.samples
    }

    /// The kinds that happened, with their counts.
    pub fn nonzero(&self) -> impl Iterator<Item = (ProblemKind, u64)> + '_ {
        ProblemKind::ALL
            .iter()
            .map(|&k| (k, self.count(k)))
            .filter(|&(_, c)| c > 0)
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::cast_possible_truncation,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects
    )]
    use super::*;

    #[test]
    fn kinds_are_numbered_densely_in_order() {
        for (i, k) in ProblemKind::ALL.iter().enumerate() {
            assert_eq!(*k as usize, i);
        }
    }

    #[test]
    fn counts_are_exact_and_samples_are_capped() {
        let mut d = Diagnostics::default();
        for i in 0..(MAX_SAMPLES as u64 + 10) {
            d.note(ProblemKind::DirentToMissingInode, 5, i);
        }
        d.note(ProblemKind::TooDeep, 5, 2);
        assert_eq!(
            d.count(ProblemKind::DirentToMissingInode),
            MAX_SAMPLES as u64 + 10
        );
        assert_eq!(d.count(ProblemKind::TooDeep), 1);
        assert_eq!(d.samples().len(), MAX_SAMPLES);
        assert_eq!(d.total(), MAX_SAMPLES as u64 + 11);
        assert_eq!(d.nonzero().count(), 2);
    }
}
