//! The versioned, length-prefixed binary protocol between the interface and
//! the elevated helper.
//!
//! It sits on the privilege boundary and parses data from another process, so
//! the panicking shortcuts and unchecked arithmetic and casts are denied in
//! this crate.
//!
//! # Frames
//!
//! ```text
//! length (u32, little-endian) | version (u8) | kind (u8) | fields
//! ```
//!
//! `length` counts everything after itself. A length below 2 or above the
//! reader's limit ends the conversation before any of the frame is read, so a
//! peer cannot make the reader allocate more than the limit. [`MAX_FRAME_LEN`]
//! is the hard maximum for any frame; the helper reads the interface's
//! requests with the much smaller [`MAX_REQUEST_FRAME_LEN`].
//!
//! # A conversation
//!
//! 1. The interface sends [`Message::Hello`] with the one-time value it gave
//!    the helper at launch. The helper answers with its own hello carrying the
//!    same value. Anything else, or any other version, ends the conversation.
//! 2. The interface sends one request: [`Message::Scan`] or
//!    [`Message::Delete`].
//! 3. A scan answers with [`Message::Batch`] and [`Message::Progress`] frames
//!    and ends with [`Message::ScanDone`]. A delete answers with one
//!    [`Message::DeleteItem`] per item and ends with [`Message::DeleteDone`].
//!    Either can instead end with [`Message::Error`].
//! 4. Either side may send [`Message::Bye`]. From the interface it stops a
//!    running scan. The helper exits after one request, or when the
//!    connection closes.
//!
//! Any frame that is not valid at its place in this order is a protocol error,
//! and the helper answers it by exiting.
//!
//! # Paths
//!
//! Paths travel as bytes: the raw bytes on Linux, UTF-16 code units (little
//! endian) on Windows. See [`path_to_wire`] and [`path_from_wire`].

#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation
)]

mod client;
mod codec;
mod frame;
mod msg;
mod path;
mod wire;

#[cfg(test)]
mod tests;

pub use client::{Client, ClientError, DeleteFailure, ScanEvent, ScanStream, SharedWriter};
pub use codec::{Direction, decode_body, encode_body};
pub use frame::{FrameReader, ReadError, WriteError, write_message};
pub use msg::{
    Backend, DeleteItem, DeleteRequest, ErrorCode, ErrorReport, Fallback, Hello, ItemOutcome,
    Message, Place, Progress, Refusal, ScanDone, ScanEnd, ScanRequest, Seen,
};
pub use path::{WirePathError, path_from_wire, path_to_wire, wire_has_nul};

/// The protocol version both sides must speak.
pub const PROTOCOL_VERSION: u16 = 1;
/// The version byte at the start of every frame body.
pub const FRAME_VERSION: u8 = 1;
/// The length of the one-time value that proves who launched the helper.
pub const NONCE_LEN: usize = 16;

/// The hard maximum of any frame body, in bytes.
pub const MAX_FRAME_LEN: usize = 4 * 1024 * 1024;
/// The most the helper reads in a request frame.
pub const MAX_REQUEST_FRAME_LEN: usize = 1024 * 1024;
/// The longest path on the wire, in bytes.
pub const MAX_WIRE_PATH: usize = 32 * 1024;
/// The most items in one delete request.
pub const MAX_DELETE_ITEMS: usize = 8192;
/// The most extra mounts in a scan request.
pub const MAX_EXTRA_MOUNTS: usize = 256;
/// The most places in a scan request.
pub const MAX_PLACES: usize = 16;
/// The longest place name.
pub const MAX_PLACE_NAME: usize = 64;
/// The longest place value, in bytes.
pub const MAX_PLACE_VALUE: usize = 4096;
/// The longest error text; longer text is cut.
pub const MAX_ERROR_TEXT: usize = 1024;

/// Why a frame body could not be read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum DecodeError {
    /// The data ends before the message does.
    Truncated,
    /// Bytes are left after the message.
    TrailingBytes,
    /// A length or count is above its limit.
    TooLong,
    /// A field holds a value that does not exist.
    BadValue,
    /// The frame is from a protocol version this build does not speak.
    UnsupportedVersion(u8),
    /// The message kind does not exist.
    UnknownKind(u8),
    /// The message kind exists but does not travel this way.
    UnexpectedKind(u8),
    /// The tree batch inside a batch frame is bad.
    Batch(bs_tree::DecodeError),
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Truncated => f.write_str("the message ends early"),
            Self::TrailingBytes => f.write_str("the message has bytes left over"),
            Self::TooLong => f.write_str("a length or count is over its limit"),
            Self::BadValue => f.write_str("a field holds an unknown value"),
            Self::UnsupportedVersion(v) => write!(f, "unsupported frame version {v}"),
            Self::UnknownKind(k) => write!(f, "unknown message kind {k}"),
            Self::UnexpectedKind(k) => write!(f, "message kind {k} is not valid in this direction"),
            Self::Batch(e) => write!(f, "bad tree batch: {e}"),
        }
    }
}

impl std::error::Error for DecodeError {}

/// Why a message could not be written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum EncodeError {
    /// A field is over its limit, or the whole frame is over the maximum.
    TooLarge,
}

impl std::fmt::Display for EncodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the message is too large to send")
    }
}

impl std::error::Error for EncodeError {}
