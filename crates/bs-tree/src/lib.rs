//! The size tree: arena storage, the name pool, size aggregation,
//! completeness tracking and snapshots for the layout and the renderer.
//!
//! # How the pieces fit
//!
//! * A scanner produces [`Batch`]es of [`Record`]s (plain data, optionally
//!   serialized with [`Batch::encode`]).
//! * One owner thread holds an [`Aggregator`], applies the batches, and calls
//!   [`Aggregator::publish`] now and then (throttled with
//!   [`Aggregator::publish_if_due`]).
//! * Any number of readers hold a [`SnapshotReader`] and call
//!   [`SnapshotReader::load`] to get an immutable [`Snapshot`]. Loading never
//!   waits for aggregation (the one lock involved guards a single pointer and
//!   is held only to clone or replace it), and a snapshot never changes after
//!   it is published. Walk it with [`Snapshot::root`] and [`NodeRef`].
//!
//! # What the numbers mean
//!
//! * Sizes are **allocated** bytes (size on disk) with **logical** bytes
//!   alongside. A directory's size is its own entry plus everything below.
//! * The file count of a node is the number of non-directory entries at and
//!   below it: regular files, links and special files, including every
//!   name of a hard-linked file (only the first name carries bytes).
//! * Times are whole seconds since the Unix epoch; anything finer is dropped.
//! * A directory is **complete** once its listing is finished and every
//!   subdirectory is complete. Until then its sizes are a lower bound, and
//!   [`NodeRef::bytes_are_lower_bound`] says so. Unreadable directories are
//!   kept, flagged with their error, and count as finished so the scan can
//!   complete around them; their ancestors are flagged too.
//! * A hard-linked file is counted once, at the first name seen; later names
//!   are flagged and add zero bytes. Entries without a file identity (some
//!   filesystems have none) are never treated as hard links.
//! * Deleting never removes nodes. Marks move bytes into "in bin" or
//!   "deleted" totals that sit beside the counted sizes, so a layout built
//!   from counted sizes stays valid, and undo restores everything exactly.
//!
//! # Memory
//!
//! Each node takes 47 bytes of column storage plus its name (and a one-byte
//! length prefix, two for names of 128 bytes or more) in a shared pool.
//! Columns and pool live in `Arc`-shared chunks, so a published snapshot
//! shares everything the owner has not changed since.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod aggregator;
mod batch;
mod error;
mod snapshot;
mod store;
mod types;

pub use aggregator::{Aggregator, ApplierLimits, ApplyReport, FinishReport, SnapshotReader};
pub use batch::{BATCH_FORMAT_VERSION, Batch, DECODE_MAX_EXPANSION, Entry, Meta, NameSpan, Record};
pub use error::{ApplyError, BatchFull, DecodeError, DeleteError, Violation};
pub use snapshot::{Children, NodeRef, Progress, Snapshot};
pub use types::{
    ByteCount, Category, DeleteState, DeleteTotals, DirKey, FileKey, MAX_NAME_LEN, Moved,
    NodeFlags, NodeId, NodeKind, ReadErrorKind, ReadFailure,
};
