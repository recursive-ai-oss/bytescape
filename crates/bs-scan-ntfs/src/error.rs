//! What can go wrong, in two sizes: an error that stops the whole read, and a
//! problem with one structure, which marks that part of the tree and lets the
//! rest of the read go on.

use std::fmt;
use std::io;

/// Why the volume could not be read at all.
#[derive(Debug)]
#[non_exhaustive]
pub enum NtfsError {
    /// The device could not be read where the read cannot go on without it.
    Io(io::Error),
    /// There is no NTFS boot sector.
    NotNtfs,
    /// The boot sector is damaged or describes something impossible.
    BadBootSector(&'static str),
    /// The master file table cannot be located or described, so there is no
    /// tree.
    BadMft(&'static str),
    /// The root directory is missing or unusable, so there is no tree.
    BadRoot(&'static str),
    /// The volume would need more memory than the reader's limit.
    TooLarge(&'static str),
}

impl fmt::Display for NtfsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NtfsError::Io(e) => write!(f, "cannot read the volume: {e}"),
            NtfsError::NotNtfs => f.write_str("not an NTFS volume"),
            NtfsError::BadBootSector(why) => write!(f, "damaged boot sector: {why}"),
            NtfsError::BadMft(why) => write!(f, "unusable master file table: {why}"),
            NtfsError::BadRoot(why) => write!(f, "unusable root directory: {why}"),
            NtfsError::TooLarge(why) => write!(f, "volume too large to read: {why}"),
        }
    }
}

impl std::error::Error for NtfsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            NtfsError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for NtfsError {
    fn from(e: io::Error) -> Self {
        NtfsError::Io(e)
    }
}

/// One kind of inconsistency the reader met and worked around.
///
/// Some are damage (a length that does not fit its record); some are what a
/// live volume looks like while it changes (a name whose parent was deleted
/// after the table was read). Neither is fatal, and neither is hidden: the
/// reader counts them, keeps a few examples, and marks the affected entry (or
/// the root, when it cannot tell which entry) as unreadable where bytes could
/// be missing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
#[repr(u8)]
pub enum ProblemKind {
    /// A stretch of the master file table could not be read from the device.
    /// The records in it are unknown.
    MftUnreadable,
    /// The table has a hole (a sparse run) where records should be. The
    /// records in it are unknown.
    MftHole,
    /// A record in use does not begin with `FILE`, or begins with `BAAD`
    /// (the mark of an interrupted write). It is skipped.
    RecordBadMagic,
    /// A record's update sequence does not match its sectors (a torn
    /// write), or the sequence itself does not fit. It is skipped.
    RecordFixup,
    /// A record's header is inconsistent: offsets or lengths out of range, or
    /// it names a record number that is not its own slot. It is skipped.
    RecordHeader,
    /// An attribute does not fit in its record. The attributes before it are
    /// kept; the file is marked damaged.
    AttributeCorrupt,
    /// A run list is invalid: truncated, negative, or outside the volume.
    /// The file is marked damaged.
    RunlistCorrupt,
    /// An attribute list is invalid, or could not be read.
    AttributeListCorrupt,
    /// An attribute list names an extension record that is not in use, does
    /// not belong to the file, or lies outside the table. The file is marked
    /// damaged.
    ExtensionMissing,
    /// An extension record in use is not named by its file's attribute list.
    /// It is ignored: it is stale or damaged.
    ExtensionUnlisted,
    /// A `$FILE_NAME` attribute is invalid. That name is skipped.
    NameCorrupt,
    /// A name's parent is not a directory in use, or is a different file
    /// than when the name was written. The file is not reachable by that
    /// name.
    ParentStale,
    /// A directory is entered by a second name (a loop or a hard-linked
    /// directory). The second name is skipped.
    DirectoryLinkedTwice,
    /// The tree is nested deeper than the reader follows.
    TooDeep,
    /// A stream's recorded sizes disagree with its run list. The run list
    /// wins for the allocated size.
    SizeMismatch,
    /// The attribute-list reads for this volume exceeded their budget. The
    /// files whose lists were not read are marked as possibly incomplete.
    ReadBudgetExceeded,
    /// A name contains NUL, a slash or a backslash, which no real directory
    /// can hold, and is recorded as damaged.
    NameUnusable,
    /// A file has more names than NTFS allows (1,024 links, each with a
    /// short alias). The names past the limit are dropped and the file is
    /// marked damaged.
    TooManyNames,
    /// The paths that category lookups need would take more memory than the
    /// limit leaves, so directories below this point share their parent's
    /// path. Only categories that depend on the full path are affected; no
    /// size is.
    PathBudget,
}

impl ProblemKind {
    /// Every kind, in declaration order.
    pub const ALL: [ProblemKind; 19] = [
        ProblemKind::MftUnreadable,
        ProblemKind::MftHole,
        ProblemKind::RecordBadMagic,
        ProblemKind::RecordFixup,
        ProblemKind::RecordHeader,
        ProblemKind::AttributeCorrupt,
        ProblemKind::RunlistCorrupt,
        ProblemKind::AttributeListCorrupt,
        ProblemKind::ExtensionMissing,
        ProblemKind::ExtensionUnlisted,
        ProblemKind::NameCorrupt,
        ProblemKind::ParentStale,
        ProblemKind::DirectoryLinkedTwice,
        ProblemKind::TooDeep,
        ProblemKind::SizeMismatch,
        ProblemKind::ReadBudgetExceeded,
        ProblemKind::NameUnusable,
        ProblemKind::TooManyNames,
        ProblemKind::PathBudget,
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
    /// The record number it was met in, or 0.
    pub record: u64,
    /// A kind-specific number: the other record, or the offending value.
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
    pub(crate) fn note(&mut self, kind: ProblemKind, record: u64, detail: u64) {
        if let Some(count) = self.counts.get_mut(kind.index()) {
            *count = count.saturating_add(1);
        }
        if self.samples.len() < MAX_SAMPLES {
            self.samples.push(Problem {
                kind,
                record,
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
        for i in 0..(MAX_SAMPLES as u64 + 10) {
            d.note(ProblemKind::ParentStale, i, 0);
        }
        d.note(ProblemKind::TooDeep, 1, 2);
        assert_eq!(d.count(ProblemKind::ParentStale), MAX_SAMPLES as u64 + 10);
        assert_eq!(d.count(ProblemKind::TooDeep), 1);
        assert_eq!(d.samples().len(), MAX_SAMPLES);
        assert_eq!(d.total(), MAX_SAMPLES as u64 + 11);
        assert_eq!(d.nonzero().count(), 2);
        let mut other = Diagnostics::default();
        other.note(ProblemKind::TooDeep, 9, 9);
        d.absorb(&other);
        assert_eq!(d.count(ProblemKind::TooDeep), 2);
    }
}
