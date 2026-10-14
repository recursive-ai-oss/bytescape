//! Error types.

use crate::types::{DirKey, NodeId};
use std::fmt;

/// Why one record was not applied. The rest of the batch is still applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApplyError {
    /// The first record of a scan must be a directory.
    RootMustBeDirectory,
    /// A second record claims to be the scan root.
    RootAlreadyPresent,
    /// A directory used the reserved "no key" value.
    ReservedKey,
    /// A directory names itself as its parent.
    SelfParent,
    /// Directories name each other as parents, so they can never be attached.
    Cycle,
    /// Two directories share a key.
    DuplicateKey(DirKey),
    /// The parent directory is already complete; nothing may be added to it.
    ParentComplete,
    /// The directory's listing was already reported finished.
    ListingAlreadyDone,
    /// The scan was already finished; nothing more can be added.
    ScanFinished,
    /// Too many records are waiting for a parent that has not arrived.
    ParkLimit,
    /// A name's offset or length points outside the batch.
    BadNameSpan,
    /// The tree is full: node ids are `u32`.
    NodeIdsExhausted,
    /// The name pool is full.
    NamePoolFull,
    /// The sum of all sizes does not fit in 64 bits.
    SizeOverflow,
}

impl fmt::Display for ApplyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            ApplyError::RootMustBeDirectory => "the scan root must be a directory",
            ApplyError::RootAlreadyPresent => "the scan root was already given",
            ApplyError::ReservedKey => "directory key is reserved",
            ApplyError::SelfParent => "directory is its own parent",
            ApplyError::Cycle => "directories form a parent cycle",
            ApplyError::DuplicateKey(key) => return write!(f, "duplicate directory key {}", key.0),
            ApplyError::ParentComplete => "parent directory is already complete",
            ApplyError::ListingAlreadyDone => "directory listing was already finished",
            ApplyError::ScanFinished => "the scan was already finished",
            ApplyError::ParkLimit => "too many records waiting for their parent",
            ApplyError::BadNameSpan => "name lies outside the batch",
            ApplyError::NodeIdsExhausted => "tree is full (node ids exhausted)",
            ApplyError::NamePoolFull => "name pool is full",
            ApplyError::SizeOverflow => "total size overflows 64 bits",
        };
        f.write_str(text)
    }
}

impl std::error::Error for ApplyError {}

/// Why a delete mark or its undo was refused. Nothing changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeleteError {
    /// No such node.
    UnknownNode(NodeId),
    /// The node's subtree is still being counted. Only complete subtrees can
    /// be acted on.
    NotComplete(NodeId),
    /// The node is already marked.
    AlreadyMarked(NodeId),
    /// The node is not marked.
    NotMarked(NodeId),
    /// An ancestor is marked, so this node's bytes are already accounted
    /// for there. Undo the ancestor first.
    AncestorMarked(NodeId),
    /// The requested state is [`DeleteState::None`](crate::DeleteState::None);
    /// use the undo call instead.
    NoState,
    /// Internal accounting did not add up. The tree is left unchanged.
    Inconsistent(NodeId),
}

impl fmt::Display for DeleteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DeleteError::UnknownNode(n) => write!(f, "no node {}", n.index()),
            DeleteError::NotComplete(n) => write!(f, "node {} is still being counted", n.index()),
            DeleteError::AlreadyMarked(n) => write!(f, "node {} is already marked", n.index()),
            DeleteError::NotMarked(n) => write!(f, "node {} is not marked", n.index()),
            DeleteError::AncestorMarked(n) => {
                write!(f, "an ancestor of node {} is marked", n.index())
            }
            DeleteError::NoState => f.write_str("no delete state given"),
            DeleteError::Inconsistent(n) => {
                write!(f, "delete accounting inconsistent at node {}", n.index())
            }
        }
    }
}

impl std::error::Error for DeleteError {}

/// Why a serialized batch could not be read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// The data ended early.
    Truncated,
    /// Not a batch.
    BadMagic,
    /// A format version this build does not know.
    UnsupportedVersion(u16),
    /// Reserved header bits that this version requires to be zero.
    ReservedBits,
    /// A tag or enumeration value that does not exist.
    BadValue,
    /// A count or length larger than the data could hold.
    BadLength,
    /// A name lies outside the names block.
    BadNameSpan,
    /// Bytes left over after the last record.
    TrailingBytes,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            DecodeError::Truncated => "batch data ends early",
            DecodeError::BadMagic => "not a batch",
            DecodeError::UnsupportedVersion(v) => {
                return write!(f, "unsupported batch version {v}");
            }
            DecodeError::ReservedBits => "reserved header bits are set",
            DecodeError::BadValue => "batch holds an unknown value",
            DecodeError::BadLength => "batch length field exceeds the data",
            DecodeError::BadNameSpan => "name lies outside the batch",
            DecodeError::TrailingBytes => "unexpected bytes after the last record",
        };
        f.write_str(text)
    }
}

impl std::error::Error for DecodeError {}

/// The batch has no room for more data (its names block would pass 4 GiB).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BatchFull;

impl fmt::Display for BatchFull {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("batch is full")
    }
}

impl std::error::Error for BatchFull {}

/// A broken tree invariant found by [`Snapshot::verify`](crate::Snapshot::verify).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Violation {
    /// The node where the problem shows.
    pub node: NodeId,
    /// What is wrong.
    pub what: &'static str,
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "node {}: {}", self.node.index(), self.what)
    }
}

impl std::error::Error for Violation {}
