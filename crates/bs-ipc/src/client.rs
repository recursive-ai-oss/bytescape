//! The interface's side of a conversation, over any pair of byte streams.
//!
//! Launching the helper and the operating-system checks on who is at the
//! other end belong to the platform crate. This part only speaks the protocol
//! and checks that what comes back is in the right order.

use crate::codec::Direction;
use crate::frame::{FrameReader, ReadError, WriteError, write_message};
use crate::{
    DeleteRequest, ErrorReport, Hello, ItemOutcome, MAX_FRAME_LEN, Message, NONCE_LEN,
    PROTOCOL_VERSION, Progress, ScanDone, ScanRequest,
};
use bs_tree::Batch;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

/// Why a conversation with the helper failed.
#[derive(Debug)]
#[non_exhaustive]
pub enum ClientError {
    /// The connection failed.
    Read(ReadError),
    Write(WriteError),
    /// The helper closed the connection before it was done.
    Disconnected,
    /// The helper speaks another protocol version.
    Version(u16),
    /// The helper's hello did not carry the value it was launched with.
    WrongNonce,
    /// The helper sent something that is not valid here.
    Unexpected(&'static str),
    /// The helper reported that it could not do the work.
    Helper(ErrorReport),
    /// The request was invalid (a delete request must be permanent).
    BadRequest(&'static str),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Read(e) => e.fmt(f),
            Self::Write(e) => e.fmt(f),
            Self::Disconnected => f.write_str("the helper closed the connection"),
            Self::Version(v) => write!(f, "the helper speaks protocol version {v}"),
            Self::WrongNonce => f.write_str("the helper did not know the launch value"),
            Self::Unexpected(what) => write!(f, "the helper sent {what} where it does not belong"),
            Self::Helper(report) => write!(f, "the helper could not do it: {}", report.message),
            Self::BadRequest(why) => write!(f, "invalid request: {why}"),
        }
    }
}

impl std::error::Error for ClientError {}

impl From<ReadError> for ClientError {
    fn from(e: ReadError) -> Self {
        Self::Read(e)
    }
}

impl From<WriteError> for ClientError {
    fn from(e: WriteError) -> Self {
        Self::Write(e)
    }
}

/// A delete that did not finish, with what is known about the items.
#[derive(Debug)]
pub struct DeleteFailure {
    pub error: ClientError,
    /// The outcomes that arrived before the failure, by item index. Items not
    /// listed have an unknown outcome: they may have been deleted.
    pub completed: Vec<(u32, ItemOutcome)>,
}

impl std::fmt::Display for DeleteFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} ({} items have an outcome)",
            self.error,
            self.completed.len()
        )
    }
}

impl std::error::Error for DeleteFailure {}

/// One event of a running scan.
#[derive(Debug)]
#[non_exhaustive]
pub enum ScanEvent {
    Batch(Batch),
    Progress(Progress),
    Done(ScanDone),
}

/// A connection to a helper that has answered the hello.
pub struct Client<R: Read, W: Write> {
    reader: FrameReader<R>,
    writer: W,
    /// A scan was started and its end has not been returned yet.
    scanning: bool,
}

impl<R: Read, W: Write> Client<R, W> {
    /// Says hello with `nonce` and checks the answer: the same protocol
    /// version and the same value back.
    pub fn connect(reader: R, mut writer: W, nonce: [u8; NONCE_LEN]) -> Result<Self, ClientError> {
        write_message(
            &mut writer,
            &Message::Hello(Hello {
                version: PROTOCOL_VERSION,
                nonce,
            }),
        )?;
        let mut reader = FrameReader::new(reader, Direction::ToInterface, MAX_FRAME_LEN);
        match reader.read()? {
            Some(Message::Hello(hello)) => {
                if hello.version != PROTOCOL_VERSION {
                    return Err(ClientError::Version(hello.version));
                }
                if hello.nonce != nonce {
                    return Err(ClientError::WrongNonce);
                }
            }
            Some(Message::Error(report)) => return Err(ClientError::Helper(report)),
            Some(_) => return Err(ClientError::Unexpected("a message instead of hello")),
            None => return Err(ClientError::Disconnected),
        }
        Ok(Self {
            reader,
            writer,
            scanning: false,
        })
    }

    /// Starts a scan and returns a stream of its events, which borrows the
    /// connection. Use [`Client::start_scan`] and [`Client::next_scan_event`]
    /// to keep the connection in a structure instead.
    pub fn scan(&mut self, request: ScanRequest) -> Result<ScanStream<'_, R, W>, ClientError> {
        self.start_scan(request)?;
        Ok(ScanStream { client: self })
    }

    /// Sends a scan request.
    pub fn start_scan(&mut self, request: ScanRequest) -> Result<(), ClientError> {
        write_message(&mut self.writer, &Message::Scan(request))?;
        self.scanning = true;
        Ok(())
    }

    /// The next event of the scan started with [`Client::start_scan`];
    /// `Ok(None)` when no scan is running or its end has been returned.
    pub fn next_scan_event(&mut self) -> Result<Option<ScanEvent>, ClientError> {
        if !self.scanning {
            return Ok(None);
        }
        let event = match self.reader.read() {
            Ok(Some(Message::Batch(batch))) => Ok(Some(ScanEvent::Batch(batch))),
            Ok(Some(Message::Progress(progress))) => Ok(Some(ScanEvent::Progress(progress))),
            Ok(Some(Message::ScanDone(done))) => {
                self.scanning = false;
                Ok(Some(ScanEvent::Done(done)))
            }
            Ok(Some(Message::Error(report))) => Err(ClientError::Helper(report)),
            Ok(Some(_)) => Err(ClientError::Unexpected(
                "a message that is not part of a scan",
            )),
            Ok(None) => Err(ClientError::Disconnected),
            Err(e) => Err(e.into()),
        };
        if event.is_err() {
            self.scanning = false;
        }
        event
    }

    /// Tells the helper to stop the running scan. The scan still ends with a
    /// [`ScanEvent::Done`] (cancelled) unless the helper is already gone.
    pub fn stop_scan(&mut self) -> Result<(), ClientError> {
        Ok(write_message(&mut self.writer, &Message::Bye)?)
    }

    /// Deletes the items permanently and returns one outcome per item, in the
    /// order given. The request must say `permanent`.
    pub fn delete(&mut self, request: DeleteRequest) -> Result<Vec<ItemOutcome>, DeleteFailure> {
        let fail = |error: ClientError, completed: Vec<(u32, ItemOutcome)>| DeleteFailure {
            error,
            completed,
        };
        if !request.permanent {
            return Err(fail(
                ClientError::BadRequest("the helper deletes permanently only"),
                Vec::new(),
            ));
        }
        let count = request.items.len();
        write_message(&mut self.writer, &Message::Delete(request))
            .map_err(|e| fail(e.into(), Vec::new()))?;
        let mut slots: Vec<Option<ItemOutcome>> = vec![None; count];
        let completed = |slots: &[Option<ItemOutcome>]| -> Vec<(u32, ItemOutcome)> {
            slots
                .iter()
                .enumerate()
                .filter_map(|(i, o)| Some((u32::try_from(i).ok()?, (*o)?)))
                .collect()
        };
        loop {
            let message = match self.reader.read() {
                Ok(Some(message)) => message,
                Ok(None) => return Err(fail(ClientError::Disconnected, completed(&slots))),
                Err(e) => return Err(fail(e.into(), completed(&slots))),
            };
            match message {
                Message::DeleteItem { index, outcome } => {
                    let slot = usize::try_from(index).ok().and_then(|i| slots.get_mut(i));
                    match slot {
                        Some(slot @ None) => *slot = Some(outcome),
                        _ => {
                            return Err(fail(
                                ClientError::Unexpected(
                                    "an outcome for an unknown or finished item",
                                ),
                                completed(&slots),
                            ));
                        }
                    }
                }
                Message::DeleteDone { items } => {
                    let all = slots.iter().all(Option::is_some);
                    if usize::try_from(items).ok() != Some(count) || !all {
                        return Err(fail(
                            ClientError::Unexpected("a finish before every item had an outcome"),
                            completed(&slots),
                        ));
                    }
                    return Ok(slots.into_iter().flatten().collect());
                }
                Message::Error(report) => {
                    return Err(fail(ClientError::Helper(report), completed(&slots)));
                }
                _ => {
                    return Err(fail(
                        ClientError::Unexpected("a message that is not part of a delete"),
                        completed(&slots),
                    ));
                }
            }
        }
    }

    /// Tells the helper the conversation is over.
    pub fn finish(mut self) {
        // The helper exits when the connection closes whether or not this
        // arrives.
        let _ = write_message(&mut self.writer, &Message::Bye);
    }
}

/// The events of one running scan, borrowing the connection.
pub struct ScanStream<'a, R: Read, W: Write> {
    client: &'a mut Client<R, W>,
}

impl<R: Read, W: Write> ScanStream<'_, R, W> {
    /// The next event; `Ok(None)` after the scan's end has been returned.
    pub fn next_event(&mut self) -> Result<Option<ScanEvent>, ClientError> {
        self.client.next_scan_event()
    }

    /// Asks the helper to stop the scan. The stream still ends with
    /// [`ScanEvent::Done`] (cancelled) unless the helper is already gone.
    pub fn stop(&mut self) -> Result<(), ClientError> {
        self.client.stop_scan()
    }
}

/// A writer that can be cloned and used from several threads, one write at a
/// time. The interface gives one clone to the thread that cancels a scan, so
/// the goodbye can be sent while another thread waits for the next event.
pub struct SharedWriter<W: Write>(Arc<Mutex<W>>);

impl<W: Write> SharedWriter<W> {
    pub fn new(inner: W) -> Self {
        Self(Arc::new(Mutex::new(inner)))
    }
}

impl<W: Write> Clone for SharedWriter<W> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl<W: Write> Write for SharedWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .map_err(|_| std::io::Error::other("the connection was poisoned"))?
            .write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0
            .lock()
            .map_err(|_| std::io::Error::other("the connection was poisoned"))?
            .flush()
    }
}
