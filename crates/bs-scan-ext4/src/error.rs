//! What can go wrong, in two sizes: an error that stops the whole read, and a
//! problem with one structure, which marks that part of the tree and lets the
//! rest of the read go on.

use std::fmt;
use std::io;

/// Why the file system could not be read at all.
#[derive(Debug)]
#[non_exhaustive]
pub enum Ext4Error {
    /// The device could not be read where the read cannot go on without it.
    Io(io::Error),
    /// There is no ext2/3/4 superblock.
    NotExt4,
    /// The superblock is damaged or describes something impossible.
    BadSuperblock(&'static str),
    /// The file system uses a feature this reader does not handle.
    Unsupported(&'static str),
    /// The root directory is missing or unusable, so there is no tree.
    BadRoot(&'static str),
    /// The file system would need more memory than the reader's limit.
    TooLarge(&'static str),
    /// The mount being scanned shows only a subdirectory of the file system
    /// (a bind mount of a subdirectory, a btrfs-style subvolume mount). The
    /// reader presents the whole file system at the mount point, which would
    /// misstate what the user sees there, so it does not read it.
    PartialMount,
}

impl fmt::Display for Ext4Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Ext4Error::Io(e) => write!(f, "cannot read the device: {e}"),
            Ext4Error::NotExt4 => f.write_str("not an ext2/3/4 file system"),
            Ext4Error::BadSuperblock(why) => write!(f, "damaged superblock: {why}"),
            Ext4Error::Unsupported(what) => write!(f, "unsupported file system feature: {what}"),
            Ext4Error::BadRoot(why) => write!(f, "unusable root directory: {why}"),
            Ext4Error::TooLarge(why) => write!(f, "file system too large to read: {why}"),
            Ext4Error::PartialMount => f.write_str(
                "the mount shows only part of the file system, which the raw reader cannot present",
            ),
        }
    }
}

impl std::error::Error for Ext4Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Ext4Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for Ext4Error {
    fn from(e: io::Error) -> Self {
        Ext4Error::Io(e)
    }
}

/// One kind of inconsistency the reader met and worked around.
///
/// Some are damage (a length that does not fit its block); some are what a
/// live file system looks like while it changes (a directory entry naming an
/// inode that was freed after the inode tables were read). Neither is fatal,
/// and neither is ever hidden: the reader counts them, keeps a few examples,
/// and marks the affected directory or entry as unreadable where bytes could
/// be missing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
#[repr(u8)]
pub enum ProblemKind {
    /// A group descriptor block could not be read, or a group's inode table
    /// lies outside the file system. The group's inodes are unreadable.
    GroupDamaged,
    /// A chunk of an inode table could not be read.
    InodeTableUnreadable,
    /// An inode has an impossible type or size.
    InodeInvalid,
    /// A directory entry names an inode number outside the file system. The
    /// entry is skipped and its directory is marked unreadable, because
    /// nothing says what the entry stood for.
    DirentBadInode,
    /// A directory entry names an inode that is not in use. On a live file
    /// system this is usually a file deleted a moment ago.
    DirentToFreeInode,
    /// A directory entry's recorded type disagrees with its inode's type,
    /// as when an inode was freed and reused. The entry is skipped; unless
    /// the inode was already reached by another name, its directory is
    /// marked unreadable, because the inode's bytes (and, for a directory,
    /// everything below it) are missing from the tree.
    DirentTypeMismatch,
    /// A directory entry names a reserved inode. The entry is skipped and
    /// its directory is marked unreadable.
    DirentToReservedInode,
    /// A directory entry has a name that cannot exist (empty, or containing
    /// `/` or NUL).
    DirentBadName,
    /// A directory block's entry lengths do not fit the block. The rest of
    /// the block is unreadable.
    DirectoryBlockCorrupt,
    /// A directory is entered by a second entry (a loop or a hard-linked
    /// directory). The second entry is skipped.
    DirectoryLinkedTwice,
    /// A directory's `..` entry does not name the directory that lists it.
    /// The entry that led there is skipped, and its directory is marked
    /// unreadable, because the subtree is not counted anywhere else.
    DirectoryParentMismatch,
    /// A directory has no `..` entry.
    DirectoryNoParent,
    /// An extent tree node is invalid: bad magic, depth, count or block.
    ExtentTreeCorrupt,
    /// An extent tree points back into itself.
    ExtentTreeLoop,
    /// A block pointer lies outside the file system.
    BlockOutOfRange,
    /// A block of a directory or its map could not be read.
    BlockUnreadable,
    /// The inline data of a directory is invalid.
    InlineDataCorrupt,
    /// The directory blocks read so far exceed the number of blocks in the
    /// file system, which only blocks shared between directories can cause.
    /// Reading stops for the directory to keep the time bounded.
    ReadBudgetExceeded,
    /// The tree is nested deeper than the reader follows.
    TooDeep,
    /// Too many entries or name bytes to number.
    TooManyEntries,
}

impl ProblemKind {
    /// Every kind, in declaration order.
    pub const ALL: [ProblemKind; 20] = [
        ProblemKind::GroupDamaged,
        ProblemKind::InodeTableUnreadable,
        ProblemKind::InodeInvalid,
        ProblemKind::DirentBadInode,
        ProblemKind::DirentToFreeInode,
        ProblemKind::DirentTypeMismatch,
        ProblemKind::DirentToReservedInode,
        ProblemKind::DirentBadName,
        ProblemKind::DirectoryBlockCorrupt,
        ProblemKind::DirectoryLinkedTwice,
        ProblemKind::DirectoryParentMismatch,
        ProblemKind::DirectoryNoParent,
        ProblemKind::ExtentTreeCorrupt,
        ProblemKind::ExtentTreeLoop,
        ProblemKind::BlockOutOfRange,
        ProblemKind::BlockUnreadable,
        ProblemKind::InlineDataCorrupt,
        ProblemKind::ReadBudgetExceeded,
        ProblemKind::TooDeep,
        ProblemKind::TooManyEntries,
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
    /// The inode it was met in (the directory being read, or the inode
    /// looked at), or 0.
    pub inode: u32,
    /// A kind-specific number: the bad inode or block number, say.
    pub detail: u64,
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
    pub(crate) fn note(&mut self, kind: ProblemKind, inode: u32, detail: u64) {
        if let Some(count) = self.counts.get_mut(kind.index()) {
            *count = count.saturating_add(1);
        }
        if self.samples.len() < MAX_SAMPLES {
            self.samples.push(Problem {
                kind,
                inode,
                detail,
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
        for i in 0..(MAX_SAMPLES as u32 + 10) {
            d.note(ProblemKind::DirentToFreeInode, i, 0);
        }
        d.note(ProblemKind::TooDeep, 1, 2);
        assert_eq!(
            d.count(ProblemKind::DirentToFreeInode),
            MAX_SAMPLES as u64 + 10
        );
        assert_eq!(d.count(ProblemKind::TooDeep), 1);
        assert_eq!(d.samples().len(), MAX_SAMPLES);
        assert_eq!(d.total(), MAX_SAMPLES as u64 + 11);
        assert_eq!(d.nonzero().count(), 2);
    }
}
