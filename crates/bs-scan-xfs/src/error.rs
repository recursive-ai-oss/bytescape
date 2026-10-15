//! What can go wrong, in two sizes: an error that stops the whole read, and a
//! problem with one structure, which marks that part of the tree and lets the
//! rest of the read go on.

use std::fmt;
use std::io;

/// Why the file system could not be read at all.
#[derive(Debug)]
#[non_exhaustive]
pub enum XfsError {
    /// The kernel refused a request, or the source could not be read, where
    /// the read cannot go on without it. Reading inodes in bulk needs
    /// administrator rights.
    Io(io::Error),
    /// The path is not on an XFS file system.
    NotXfs,
    /// The bulk inode reply cannot be one: a count the buffer cannot hold,
    /// inode numbers that do not increase.
    Hostile(&'static str),
    /// The directory where the scan starts is missing or unusable.
    BadRoot(&'static str),
    /// The file system would need more memory than the reader's limit.
    TooLarge(&'static str),
}

impl fmt::Display for XfsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            XfsError::Io(e) => write!(f, "cannot read the file system's inodes: {e}"),
            XfsError::NotXfs => f.write_str("not an XFS file system"),
            XfsError::Hostile(why) => write!(f, "implausible bulk inode reply: {why}"),
            XfsError::BadRoot(why) => write!(f, "unusable start directory: {why}"),
            XfsError::TooLarge(why) => write!(f, "file system too large to read: {why}"),
        }
    }
}

impl std::error::Error for XfsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            XfsError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for XfsError {
    fn from(e: io::Error) -> Self {
        XfsError::Io(e)
    }
}

/// One kind of inconsistency the reader met and worked around.
///
/// Some are damage; some are what a live file system looks like while it
/// changes. Neither is fatal, and neither is ever hidden: the reader counts
/// them, keeps a few examples, and marks the affected directory or entry as
/// unreadable where bytes could be missing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
#[repr(u8)]
pub enum ProblemKind {
    /// A directory's listing is damaged (a record that does not fit). What
    /// was read before it is kept and the directory is marked unreadable.
    ListingCorrupt,
    /// A directory entry has a name that cannot exist (empty, or containing
    /// `/` or NUL). The entry is skipped and its directory marked unreadable.
    EntryBadName,
    /// An entry's type in the listing disagrees with the inode's mode. The
    /// entry is skipped and its directory marked unreadable, because the
    /// inode's bytes (and, for a directory, everything below it) are missing
    /// from the tree.
    EntryTypeMismatch,
    /// An entry names an inode that neither the bulk read nor a direct
    /// lookup could find: usually a file removed a moment ago.
    EntryToMissingInode,
    /// An inode has an impossible mode.
    InodeInvalid,
    /// A directory is listed through a second entry (a loop, or a bind mount
    /// of the same directory). The second entry is skipped and its directory
    /// marked unreadable.
    DirectoryLinkedTwice,
    /// A directory could not be opened or listed. It appears as an
    /// unreadable directory.
    DirectoryUnreadable,
    /// The tree is nested deeper than the reader follows.
    TooDeep,
}

impl ProblemKind {
    /// Every kind, in declaration order.
    pub const ALL: [ProblemKind; 8] = [
        ProblemKind::ListingCorrupt,
        ProblemKind::EntryBadName,
        ProblemKind::EntryTypeMismatch,
        ProblemKind::EntryToMissingInode,
        ProblemKind::InodeInvalid,
        ProblemKind::DirectoryLinkedTwice,
        ProblemKind::DirectoryUnreadable,
        ProblemKind::TooDeep,
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
    /// The inode of the directory it was met in, or 0.
    pub directory: u64,
    /// The inode of the entry, or 0.
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
    pub(crate) fn note(&mut self, kind: ProblemKind, directory: u64, inode: u64) {
        if let Some(count) = self.counts.get_mut(kind.index()) {
            *count = count.saturating_add(1);
        }
        if self.samples.len() < MAX_SAMPLES {
            self.samples.push(Problem {
                kind,
                directory,
                inode,
            });
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
            d.note(ProblemKind::EntryToMissingInode, 5, i);
        }
        d.note(ProblemKind::TooDeep, 5, 2);
        assert_eq!(
            d.count(ProblemKind::EntryToMissingInode),
            MAX_SAMPLES as u64 + 10
        );
        assert_eq!(d.samples().len(), MAX_SAMPLES);
        assert_eq!(d.total(), MAX_SAMPLES as u64 + 11);
        assert_eq!(d.nonzero().count(), 2);
    }
}
