//! The messages of the protocol.

use bs_tree::Batch;

/// What the requester asks a scan to read with.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Backend {
    /// The best reader the helper has for the volume, falling back to the
    /// walker.
    Auto,
    /// The directory walker, which works on every filesystem.
    Walker,
    /// The raw ext4 reader (Linux).
    Ext4,
    /// The NTFS master-file-table reader (Windows).
    Ntfs,
    /// The btrfs tree search reader (Linux).
    Btrfs,
    /// The XFS bulk-stat reader (Linux).
    Xfs,
}

impl Backend {
    pub(crate) fn code(self) -> u8 {
        match self {
            Backend::Auto => 0,
            Backend::Walker => 1,
            Backend::Ext4 => 2,
            Backend::Ntfs => 3,
            Backend::Btrfs => 4,
            Backend::Xfs => 5,
        }
    }

    pub(crate) fn from_code(code: u8) -> Option<Backend> {
        Some(match code {
            0 => Backend::Auto,
            1 => Backend::Walker,
            2 => Backend::Ext4,
            3 => Backend::Ntfs,
            4 => Backend::Btrfs,
            5 => Backend::Xfs,
            _ => return None,
        })
    }
}

/// The first message in each direction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hello {
    /// The protocol version the sender speaks.
    pub version: u16,
    /// The one-time value the interface gave the helper when it launched it.
    pub nonce: [u8; crate::NONCE_LEN],
}

/// One of the user's places (a home folder, an application-data folder) as the
/// interface sees it. The helper runs as another user, so it cannot read
/// these from its own environment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Place {
    /// A variable name from the protected-path tables, such as `HOME`.
    pub name: String,
    /// The path, in the form of [`crate::path_to_wire`].
    pub value: Vec<u8>,
}

/// Read a volume.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScanRequest {
    /// The root of the drive to read, in the form of [`crate::path_to_wire`].
    pub volume: Vec<u8>,
    pub backend: Backend,
    /// Mount IDs of further mounts of the same drive that the scan may enter
    /// (Linux).
    pub extra_mount_ids: Vec<u64>,
    /// The user's places, for categories.
    pub places: Vec<Place>,
}

/// What the requester saw at a path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Seen {
    /// Device number on Linux; volume serial number on Windows.
    pub volume: u64,
    /// Inode on Linux; file ID on Windows.
    pub file: u128,
}

/// One item to delete.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeleteItem {
    /// The absolute path, in the form of [`crate::path_to_wire`].
    pub path: Vec<u8>,
    /// The user held the confirmation for this protected path.
    pub confirmed_protected: bool,
    /// What the scan saw there; the item is left alone if it differs.
    pub seen: Option<Seen>,
}

/// Delete these items.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeleteRequest {
    /// Must be `true`: the helper deletes permanently only.
    pub permanent: bool,
    pub items: Vec<DeleteItem>,
}

/// Counters of a running scan.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Progress {
    pub allocated_bytes: u64,
    pub logical_bytes: u64,
    pub files: u64,
    pub dirs: u64,
    pub unreadable: u64,
    pub vanished: u64,
    pub batches: u64,
    pub estimated: u64,
    /// The bytes the system says are in use on the volume; 0 when unknown.
    pub os_used_bytes: u64,
}

/// How a scan ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ScanEnd {
    Completed,
    Cancelled,
}

/// Why the reader that ran is not the one that was asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Fallback {
    /// The reader asked for ran.
    None,
    /// The helper has no such reader (yet), so the walker ran.
    Unavailable,
    /// The filesystem needs more memory than the reader may use.
    TooLarge,
    /// The mount shows only part of its filesystem.
    PartialMount,
    /// The reader cannot read this filesystem.
    NotSupported,
    /// A mount of the drive could not be placed in the reader's view of the
    /// filesystem, so a tree from it would not match what is mounted.
    MountNotPlaced,
}

/// A scan finished.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScanDone {
    /// The reader that produced the batches.
    pub backend: Backend,
    pub end: ScanEnd,
    pub fallback: Fallback,
    /// The final counters.
    pub progress: Progress,
}

/// What kind of error ended the helper's work.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ErrorCode {
    /// The request makes no sense.
    BadRequest,
    /// The scan could not run.
    ScanFailed,
    /// The helper cannot do this on this system.
    Unsupported,
    /// The helper will not do this.
    Refused,
    /// Something went wrong inside the helper.
    Internal,
    /// An administrator delete cannot be judged on this computer: the
    /// account that asked has no entry in the local account database, so the
    /// helper does not know which folders are the user's own to protect.
    /// Nothing was deleted.
    NoAccount,
}

impl ErrorCode {
    pub(crate) fn code(self) -> u8 {
        match self {
            ErrorCode::BadRequest => 1,
            ErrorCode::ScanFailed => 2,
            ErrorCode::Unsupported => 3,
            ErrorCode::Refused => 4,
            ErrorCode::Internal => 5,
            ErrorCode::NoAccount => 6,
        }
    }

    pub(crate) fn from_code(code: u8) -> Option<ErrorCode> {
        Some(match code {
            1 => ErrorCode::BadRequest,
            2 => ErrorCode::ScanFailed,
            3 => ErrorCode::Unsupported,
            4 => ErrorCode::Refused,
            5 => ErrorCode::Internal,
            6 => ErrorCode::NoAccount,
            _ => return None,
        })
    }
}

/// The helper could not do what it was asked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ErrorReport {
    pub code: ErrorCode,
    /// A short description, with no paths.
    pub message: String,
}

/// Why an item was not deleted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Refusal {
    /// Part of the operating system.
    Blocked,
    /// Protected, and the request does not say the user confirmed it.
    ProtectedNotConfirmed,
    /// Another item decided the whole selection is refused; `driver` is its
    /// index.
    SelectionRefused { driver: u32 },
    /// The path is empty, relative, has `.` or `..` parts, is too long, or
    /// names a root.
    BadPath,
    /// Nothing is at the path.
    NotFound,
    /// The path could not be looked up.
    CannotResolve,
    /// The item is not what the requester saw there.
    ChangedSinceScan,
    /// The name is where another filesystem starts.
    MountPoint,
    /// Another item in the request contains this one or is this one;
    /// `with` is its index.
    Overlaps { with: u32 },
    /// The path goes through a link or is not spelled the way the place
    /// really is. The helper acts only on real paths.
    NotRealPath,
    /// The request did not ask for a permanent delete.
    NotPermanent,
}

/// What happened to one item.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ItemOutcome {
    /// The item is gone for good.
    Deleted {
        /// Bytes given back to the volume.
        bytes: u64,
        entries: u64,
        /// Files that still exist under another name.
        shared_files: u64,
    },
    /// Nothing was touched.
    Refused(Refusal),
    /// The delete started or was tried and did not complete. Some of the item
    /// may be gone.
    Failed {
        /// 1 when the item could not be examined, 2 when removing it stopped
        /// part way.
        step: u8,
        /// The system's error number, or 0.
        os_code: i32,
        removed_bytes: u64,
        removed_entries: u64,
    },
}

/// A message of the protocol.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Message {
    /// Both directions, first.
    Hello(Hello),
    /// Interface to helper: read a volume.
    Scan(ScanRequest),
    /// Interface to helper: delete these.
    Delete(DeleteRequest),
    /// Either direction: the work is over, or stop it now.
    Bye,
    /// Helper to interface: some of the tree.
    Batch(Batch),
    /// Helper to interface: how far the scan is.
    Progress(Progress),
    /// Helper to interface: the scan is over.
    ScanDone(ScanDone),
    /// Helper to interface: the request could not be done.
    Error(ErrorReport),
    /// Helper to interface: what happened to one item of a delete.
    DeleteItem { index: u32, outcome: ItemOutcome },
    /// Helper to interface: every item has an outcome.
    DeleteDone { items: u32 },
}
