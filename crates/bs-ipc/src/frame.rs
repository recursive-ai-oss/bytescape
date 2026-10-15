//! Frames on a byte stream.

use crate::codec::{Direction, decode_body, encode_body};
use crate::{DecodeError, EncodeError, MAX_FRAME_LEN, Message};
use std::io::{self, Read, Write};

/// Why a frame could not be read.
#[derive(Debug)]
#[non_exhaustive]
pub enum ReadError {
    /// The stream failed, or ended in the middle of a frame.
    Io(io::Error),
    /// The length prefix is below the smallest frame or above the limit.
    BadLength(u32),
    /// The frame body is not a valid message.
    Decode(DecodeError),
}

impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "the connection failed: {e}"),
            Self::BadLength(n) => write!(f, "frame length {n} is not allowed"),
            Self::Decode(e) => write!(f, "bad frame: {e}"),
        }
    }
}

impl std::error::Error for ReadError {}

/// Why a message could not be sent.
#[derive(Debug)]
#[non_exhaustive]
pub enum WriteError {
    Io(io::Error),
    Encode(EncodeError),
}

impl std::fmt::Display for WriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "the connection failed: {e}"),
            Self::Encode(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for WriteError {}

/// Writes `message` as one frame and flushes.
pub fn write_message<W: Write>(out: &mut W, message: &Message) -> Result<(), WriteError> {
    // The length goes first, so the body is built after a placeholder.
    let mut frame = vec![0u8; 4];
    encode_body(message, &mut frame).map_err(WriteError::Encode)?;
    let body_len = frame
        .len()
        .checked_sub(4)
        .ok_or(WriteError::Encode(EncodeError::TooLarge))?;
    if body_len > MAX_FRAME_LEN {
        return Err(WriteError::Encode(EncodeError::TooLarge));
    }
    let prefix = u32::try_from(body_len).map_err(|_| WriteError::Encode(EncodeError::TooLarge))?;
    if let Some(head) = frame.get_mut(..4) {
        head.copy_from_slice(&prefix.to_le_bytes());
    }
    out.write_all(&frame).map_err(WriteError::Io)?;
    out.flush().map_err(WriteError::Io)
}

/// Reads frames from a stream, refusing any longer than its limit.
pub struct FrameReader<R> {
    inner: R,
    limit: usize,
    direction: Direction,
    body: Vec<u8>,
}

impl<R: Read> FrameReader<R> {
    /// A reader for frames travelling in `direction`, none longer than
    /// `limit` bytes (and never more than [`MAX_FRAME_LEN`]).
    pub fn new(inner: R, direction: Direction, limit: usize) -> Self {
        Self {
            inner,
            limit: limit.min(MAX_FRAME_LEN),
            direction,
            body: Vec::new(),
        }
    }

    /// The next message; `Ok(None)` when the stream ends between frames.
    pub fn read(&mut self) -> Result<Option<Message>, ReadError> {
        let mut prefix = [0u8; 4];
        let mut have = 0usize;
        while have < prefix.len() {
            let Some(rest) = prefix.get_mut(have..) else {
                break;
            };
            match self.inner.read(rest) {
                Ok(0) if have == 0 => return Ok(None),
                Ok(0) => {
                    return Err(ReadError::Io(io::ErrorKind::UnexpectedEof.into()));
                }
                Ok(n) => have = have.saturating_add(n),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(ReadError::Io(e)),
            }
        }
        let declared = u32::from_le_bytes(prefix);
        let len = usize::try_from(declared).map_err(|_| ReadError::BadLength(declared))?;
        // The smallest frame is a version and a kind.
        if len < 2 || len > self.limit {
            return Err(ReadError::BadLength(declared));
        }
        self.body.clear();
        // The buffer grows with the data that arrives, never to the declared
        // length in one step.
        let got = (&mut self.inner)
            .take(u64::from(declared))
            .read_to_end(&mut self.body)
            .map_err(ReadError::Io)?;
        if got != len {
            return Err(ReadError::Io(io::ErrorKind::UnexpectedEof.into()));
        }
        decode_body(&self.body, self.direction)
            .map(Some)
            .map_err(ReadError::Decode)
    }
}
