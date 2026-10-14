//! Small plain-data types shared by batches, the tree and snapshots.

use std::fmt;
use std::ops::{BitOr, BitOrAssign};

/// Longest name stored, in bytes. Longer names are cut to this length and the
/// node is flagged [`NodeFlags::NAME_TRUNCATED`]. Every real filesystem's
/// limit (255 bytes on Linux, 255 UTF-16 units, at most 765 bytes in WTF-8, on
/// Windows) is below it.
pub const MAX_NAME_LEN: usize = 1024;

/// Identifier of a node in the tree. Ids are dense, start at zero (the root)
/// and are never reused or removed.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(pub(crate) u32);

impl NodeId {
    /// The root of the scan. Present once the scan's first record is applied.
    pub const ROOT: NodeId = NodeId(0);

    /// The raw index. Stable for the life of the tree.
    pub const fn index(self) -> u32 {
        self.0
    }

    /// Wraps a raw index, for example one kept by a view. Whether the node
    /// exists is checked when it is looked up in a snapshot.
    pub const fn from_index(index: u32) -> NodeId {
        NodeId(index)
    }
}

impl fmt::Debug for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "NodeId({})", self.0)
    }
}

/// What a directory entry is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum NodeKind {
    /// A regular file.
    File = 0,
    /// A directory. Only directories have children.
    Dir = 1,
    /// A symbolic link, never followed. Its size is the link's own.
    Symlink = 2,
    /// Anything else: device nodes, sockets, pipes, junctions, mount points
    /// that were not entered.
    Other = 3,
}

impl NodeKind {
    pub(crate) const fn from_bits(bits: u8) -> NodeKind {
        match bits & 3 {
            0 => NodeKind::File,
            1 => NodeKind::Dir,
            2 => NodeKind::Symlink,
            _ => NodeKind::Other,
        }
    }

    pub(crate) const fn from_u8(value: u8) -> Option<NodeKind> {
        match value {
            0 => Some(NodeKind::File),
            1 => Some(NodeKind::Dir),
            2 => Some(NodeKind::Symlink),
            3 => Some(NodeKind::Other),
            _ => None,
        }
    }
}

/// Per-node facts. Some are set by the scanner that produced the entry
/// ([`NodeFlags::PRODUCER_MASK`]), the rest by the tree as it aggregates.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct NodeFlags(u16);

impl NodeFlags {
    /// No flags.
    pub const EMPTY: NodeFlags = NodeFlags(0);
    /// Everything at and below this node has been counted. Set by the tree.
    pub const COMPLETE: NodeFlags = NodeFlags(1 << 0);
    /// This entry could not be read. Its sizes are what the scanner knew,
    /// and its contents (if a directory) are missing. Set by the tree.
    pub const UNREADABLE: NodeFlags = NodeFlags(1 << 1);
    /// Something below this node could not be read, so its totals are a lower
    /// bound. Set by the tree.
    pub const UNREADABLE_BELOW: NodeFlags = NodeFlags(1 << 2);
    /// The file has more than one name. The tree sets it on both names when
    /// a second name with the same link key arrives, so a keyed file seen
    /// once is not flagged. A scanner that knows better (a link count above
    /// one) may set it itself, which is how a file whose other names lie
    /// outside the scanned area is still reported.
    pub const HARD_LINK: NodeFlags = NodeFlags(1 << 3);
    /// A later name of an already counted hard-linked file. Counts zero
    /// bytes. Set by the tree.
    pub const HARD_LINK_DUPLICATE: NodeFlags = NodeFlags(1 << 4);
    /// The file shares data blocks with other files (reflinks, snapshots).
    /// Set by the scanner.
    pub const SHARED: NodeFlags = NodeFlags(1 << 5);
    /// A cloud placeholder: sizes come from the directory listing only, the
    /// file was never opened. Set by the scanner.
    pub const CLOUD_PLACEHOLDER: NodeFlags = NodeFlags(1 << 6);
    /// A link, junction or mount point the scanner did not follow or enter.
    /// Set by the scanner.
    pub const NOT_FOLLOWED: NodeFlags = NodeFlags(1 << 7);
    /// Sparse or compressed: allocated size differs from logical size on
    /// purpose. Set by the scanner.
    pub const SPARSE_OR_COMPRESSED: NodeFlags = NodeFlags(1 << 8);
    /// The name was longer than [`MAX_NAME_LEN`] and was cut. Set by the
    /// tree.
    pub const NAME_TRUNCATED: NodeFlags = NodeFlags(1 << 9);
    /// At least one record for an entry at or below this directory was
    /// refused or lost, so its totals are a lower bound even though the
    /// directory may be complete. Set by the tree on the directory the record
    /// belonged to and on all its ancestors; on the root when the directory
    /// is not known.
    pub const RECORDS_LOST_BELOW: NodeFlags = NodeFlags(1 << 10);
    /// The allocated size of this entry, or of something at or below this
    /// directory, is an estimate: the filesystem gave no allocation figure,
    /// so the scanner rounded the logical size up to a cluster. A producer
    /// sets it on the entries it estimated; the tree sets it on every
    /// ancestor too, so a total that includes a guess says so. Unlike the
    /// lower-bound flags it says the figure may be off in either direction,
    /// and an interface shows "≈" for it. Set by the scanner.
    pub const ESTIMATED_SIZE: NodeFlags = NodeFlags(1 << 11);

    /// The flags a scanner may set. The tree ignores any other bit in a
    /// record.
    pub const PRODUCER_MASK: NodeFlags = NodeFlags(
        Self::HARD_LINK.0
            | Self::SHARED.0
            | Self::CLOUD_PLACEHOLDER.0
            | Self::NOT_FOLLOWED.0
            | Self::SPARSE_OR_COMPRESSED.0
            | Self::ESTIMATED_SIZE.0,
    );

    /// Every defined bit.
    pub(crate) const ALL: u16 = (1 << 12) - 1;

    /// The raw bits.
    pub const fn bits(self) -> u16 {
        self.0
    }

    /// Builds flags from raw bits, dropping undefined ones.
    pub const fn from_bits_truncate(bits: u16) -> NodeFlags {
        NodeFlags(bits & Self::ALL)
    }

    /// Whether every flag in `other` is set.
    pub const fn contains(self, other: NodeFlags) -> bool {
        self.0 & other.0 == other.0
    }

    /// Whether any flag in `other` is set.
    pub const fn intersects(self, other: NodeFlags) -> bool {
        self.0 & other.0 != 0
    }

    pub(crate) const fn and(self, other: NodeFlags) -> NodeFlags {
        NodeFlags(self.0 & other.0)
    }
}

impl BitOr for NodeFlags {
    type Output = NodeFlags;
    fn bitor(self, rhs: NodeFlags) -> NodeFlags {
        NodeFlags(self.0 | rhs.0)
    }
}

impl BitOrAssign for NodeFlags {
    fn bitor_assign(&mut self, rhs: NodeFlags) {
        self.0 |= rhs.0;
    }
}

impl fmt::Debug for NodeFlags {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const NAMES: [(NodeFlags, &str); 12] = [
            (NodeFlags::COMPLETE, "COMPLETE"),
            (NodeFlags::UNREADABLE, "UNREADABLE"),
            (NodeFlags::UNREADABLE_BELOW, "UNREADABLE_BELOW"),
            (NodeFlags::HARD_LINK, "HARD_LINK"),
            (NodeFlags::HARD_LINK_DUPLICATE, "HARD_LINK_DUPLICATE"),
            (NodeFlags::SHARED, "SHARED"),
            (NodeFlags::CLOUD_PLACEHOLDER, "CLOUD_PLACEHOLDER"),
            (NodeFlags::NOT_FOLLOWED, "NOT_FOLLOWED"),
            (NodeFlags::SPARSE_OR_COMPRESSED, "SPARSE_OR_COMPRESSED"),
            (NodeFlags::NAME_TRUNCATED, "NAME_TRUNCATED"),
            (NodeFlags::RECORDS_LOST_BELOW, "RECORDS_LOST_BELOW"),
            (NodeFlags::ESTIMATED_SIZE, "ESTIMATED_SIZE"),
        ];
        let mut first = true;
        write!(f, "NodeFlags(")?;
        for (flag, name) in NAMES {
            if self.contains(flag) {
                if !first {
                    write!(f, " | ")?;
                }
                write!(f, "{name}")?;
                first = false;
            }
        }
        if first {
            write!(f, "empty")?;
        }
        write!(f, ")")
    }
}

/// Category of a file, filled in from the knowledge tables later. Zero is
/// "other" / not yet classified.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default, Debug)]
pub struct Category(pub u8);

impl Category {
    /// Not classified.
    pub const OTHER: Category = Category(0);
}

/// Why an entry could not be read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum ReadErrorKind {
    /// Access denied.
    PermissionDenied = 1,
    /// Vanished while the scan was running.
    NotFound = 2,
    /// Input/output error.
    Io = 3,
    /// The on-disk structure is damaged or inconsistent.
    Corrupt = 4,
    /// Nesting or path length beyond what the scanner supports.
    TooDeep = 5,
    /// Anything else; see the OS code.
    Other = 6,
}

impl ReadErrorKind {
    pub(crate) const fn from_u8(value: u8) -> Option<ReadErrorKind> {
        match value {
            1 => Some(ReadErrorKind::PermissionDenied),
            2 => Some(ReadErrorKind::NotFound),
            3 => Some(ReadErrorKind::Io),
            4 => Some(ReadErrorKind::Corrupt),
            5 => Some(ReadErrorKind::TooDeep),
            6 => Some(ReadErrorKind::Other),
            _ => None,
        }
    }
}

/// An unreadable entry's error: a portable kind plus the raw OS error code
/// (0 when there is none).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ReadFailure {
    /// The portable kind.
    pub kind: ReadErrorKind,
    /// The raw OS error code, or 0.
    pub code: i32,
}

/// Identity of a file on its volume, used to count hard links once. On Linux
/// this is `(device, inode)`; on Windows the volume serial number and the
/// 128-bit file ID.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FileKey {
    /// Device number or volume serial number.
    pub volume: u64,
    /// Inode number or file ID.
    pub id: u128,
}

/// Scanner-chosen identifier of a directory, unique within one scan. Entries
/// name their parent by this key, so scanners can produce records without
/// asking the tree for node ids.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct DirKey(pub u32);

impl DirKey {
    /// "No parent": the parent of the scan root. Also never a valid key for a
    /// directory.
    pub const NONE: DirKey = DirKey(u32::MAX);
}

/// Whether a node, and the bytes under it, are still on disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
#[repr(u8)]
pub enum DeleteState {
    /// Still there.
    #[default]
    None = 0,
    /// Moved to the Recycle Bin or Trash. The space is not free yet.
    InBin = 1,
    /// Permanently deleted.
    Deleted = 2,
}

impl DeleteState {
    pub(crate) const fn from_bits(bits: u16) -> DeleteState {
        match bits & 3 {
            1 => DeleteState::InBin,
            2 => DeleteState::Deleted,
            _ => DeleteState::None,
        }
    }
}

/// Bytes and file count, as a plain sum.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct ByteCount {
    /// Logical (apparent) size in bytes.
    pub logical: u64,
    /// Allocated size in bytes (size on disk).
    pub allocated: u64,
    /// Number of non-directory entries, counting every name of a
    /// hard-linked file.
    pub files: u64,
}

impl ByteCount {
    pub(crate) fn checked_add(self, other: ByteCount) -> Option<ByteCount> {
        Some(ByteCount {
            logical: self.logical.checked_add(other.logical)?,
            allocated: self.allocated.checked_add(other.allocated)?,
            files: self.files.checked_add(other.files)?,
        })
    }

    pub(crate) fn checked_sub(self, other: ByteCount) -> Option<ByteCount> {
        Some(ByteCount {
            logical: self.logical.checked_sub(other.logical)?,
            allocated: self.allocated.checked_sub(other.allocated)?,
            files: self.files.checked_sub(other.files)?,
        })
    }

    pub(crate) fn is_zero(self) -> bool {
        self == ByteCount::default()
    }
}

/// Bytes under a node that have been marked deleted, by state. Counted-size
/// totals never change; these are subtracted when "current" sizes are asked
/// for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct DeleteTotals {
    /// Moved to the Recycle Bin or Trash.
    pub in_bin: ByteCount,
    /// Permanently deleted.
    pub deleted: ByteCount,
    /// How many of the marks counted here covered a subtree whose totals
    /// were only a lower bound (see [`DeleteTotals::is_lower_bound`]).
    pub(crate) lower_bound_marks: u32,
}

impl DeleteTotals {
    pub(crate) fn field_mut(&mut self, state: DeleteState) -> Option<&mut ByteCount> {
        match state {
            DeleteState::InBin => Some(&mut self.in_bin),
            DeleteState::Deleted => Some(&mut self.deleted),
            DeleteState::None => None,
        }
    }

    pub(crate) fn is_zero(&self) -> bool {
        self.in_bin.is_zero() && self.deleted.is_zero() && self.lower_bound_marks == 0
    }

    /// Whether these byte counts are only a lower bound: some marked subtree
    /// had unreadable parts or lost records, so more bytes were affected than
    /// counted. A confirmation should say "at least".
    pub fn is_lower_bound(&self) -> bool {
        self.lower_bound_marks > 0
    }
}

/// What a delete mark (or its undo) moved.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct Moved {
    /// The bytes moved into (or back out of) the mark's state.
    pub bytes: ByteCount,
    /// The bytes are only a lower bound: the subtree had unreadable parts or
    /// lost records. Say "at least" when showing them.
    pub lower_bound: bool,
}
