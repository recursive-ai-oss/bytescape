//! One conversation: the handshake, one request, and the answer.
//!
//! The helper reads in one thread and works in another, so that it notices a
//! closed connection at once even in the middle of a long scan or delete, and
//! so that it can give up on a peer that has gone quiet.

use crate::args::Mode;
use crate::delete::Judge;
use crate::scan::{self, Host};
use bs_ipc::{
    Direction, ErrorCode, ErrorReport, FrameReader, Hello, MAX_REQUEST_FRAME_LEN, Message,
    NONCE_LEN, PROTOCOL_VERSION, ReadError, WriteError, write_message,
};
use std::cell::RefCell;
use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

/// How long the helper waits for the interface to say hello and then to make
/// its request. The elevation prompt has already been answered by then, so a
/// healthy interface sends both at once.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

/// How long one write to the interface may stay blocked. A peer that stops
/// reading cannot hold the helper (and its rights) open.
pub const WRITE_STALL: Duration = Duration::from_secs(60);

/// Why the helper stopped, and so its exit status.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Exit {
    /// The request was answered.
    Done,
    /// The connection closed (or the interface said goodbye) before or during
    /// the work. Nothing more was done.
    PipeClosed,
    /// The peer did not say hello or make a request in time, or stopped
    /// reading.
    Idle,
    /// The first message was not a hello with this run's value. No work was
    /// done and nothing was answered.
    WrongHello,
    /// A message that is not valid at its place in the conversation.
    Protocol(&'static str),
    /// A request of the kind this run may not serve.
    WrongMode,
    /// The helper could not work.
    Failed(String),
}

impl Exit {
    /// The process exit status.
    pub fn code(&self) -> u8 {
        match self {
            Exit::Done | Exit::PipeClosed => 0,
            Exit::Failed(_) => 1,
            Exit::WrongHello => 3,
            Exit::Protocol(_) | Exit::WrongMode => 4,
            Exit::Idle => 5,
        }
    }
}

/// What a run needs.
pub struct Config {
    pub nonce: [u8; NONCE_LEN],
    pub mode: Mode,
    pub request_timeout: Duration,
    pub write_stall: Duration,
    pub judge: Judge,
    pub host: Host,
    /// Called from the watchdog when a write has been stuck for too long. The
    /// process entry points make this end the process.
    pub on_stall: Box<dyn Fn() + Send + Sync>,
}

/// What the reading thread hands over.
pub(crate) enum Event {
    Message(Message),
    Closed,
    Bad(&'static str),
}

/// The writing side, with a clock the watchdog reads while a write is stuck.
pub struct Out<W: Write> {
    inner: W,
    started: Instant,
    /// Milliseconds since `started` when the current write began, or
    /// `u64::MAX` when none is in progress.
    writing_since: Arc<AtomicU64>,
}

const NOT_WRITING: u64 = u64::MAX;

impl<W: Write> Out<W> {
    pub(crate) fn send(&mut self, message: &Message) -> Result<(), Exit> {
        let now = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(0);
        self.writing_since.store(now, Ordering::Relaxed);
        let result = write_message(&mut self.inner, message);
        self.writing_since.store(NOT_WRITING, Ordering::Relaxed);
        match result {
            Ok(()) => Ok(()),
            Err(WriteError::Io(_)) => Err(Exit::PipeClosed),
            Err(_) => Err(Exit::Failed(String::from(
                "a message was too large to send",
            ))),
        }
    }

    pub(crate) fn error(&mut self, code: ErrorCode, message: &str) -> Result<(), Exit> {
        self.send(&Message::Error(ErrorReport {
            code,
            message: message.to_owned(),
        }))
    }
}

/// Compares without stopping at the first difference.
fn same_nonce(a: &[u8; NONCE_LEN], b: &[u8; NONCE_LEN]) -> bool {
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Reads frames until the stream ends or goes wrong, handing each over.
fn read_loop<R: Read>(input: R, events: &mpsc::Sender<Event>) {
    let mut reader = FrameReader::new(input, Direction::ToHelper, MAX_REQUEST_FRAME_LEN);
    loop {
        let event = match reader.read() {
            Ok(Some(message)) => Event::Message(message),
            Ok(None) => Event::Closed,
            Err(ReadError::Io(_)) => Event::Closed,
            Err(ReadError::BadLength(_)) => Event::Bad("a frame has a length that is not allowed"),
            Err(_) => Event::Bad("a frame is not a valid message"),
        };
        let last = !matches!(event, Event::Message(_));
        if events.send(event).is_err() || last {
            return;
        }
    }
}

/// Ends the process-wide wait of the watchdog thread when dropped.
struct WatchdogStop(Arc<AtomicBool>);

impl Drop for WatchdogStop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

fn start_watchdog(
    writing_since: Arc<AtomicU64>,
    started: Instant,
    stall: Duration,
    on_stall: Box<dyn Fn() + Send + Sync>,
) -> Option<WatchdogStop> {
    let done = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&done);
    let tick = stall
        .checked_div(4)
        .unwrap_or(stall)
        .clamp(Duration::from_millis(5), Duration::from_millis(500));
    let spawned = std::thread::Builder::new()
        .name("watchdog".into())
        .spawn(move || {
            while !flag.load(Ordering::Relaxed) {
                std::thread::sleep(tick);
                let since = writing_since.load(Ordering::Relaxed);
                if since != NOT_WRITING {
                    let now = u64::try_from(started.elapsed().as_millis()).unwrap_or(0);
                    let stuck = Duration::from_millis(now.saturating_sub(since));
                    if stuck >= stall {
                        on_stall();
                        return;
                    }
                }
            }
        });
    spawned.ok().map(|_| WatchdogStop(done))
}

/// Waits for the next event; a quiet peer ends the run.
fn next(events: &Receiver<Event>, wait: Duration) -> Result<Message, Exit> {
    match events.recv_timeout(wait) {
        Ok(Event::Message(message)) => Ok(message),
        Ok(Event::Closed) | Err(RecvTimeoutError::Disconnected) => Err(Exit::PipeClosed),
        Ok(Event::Bad(why)) => Err(Exit::Protocol(why)),
        Err(RecvTimeoutError::Timeout) => Err(Exit::Idle),
    }
}

/// Serves one conversation on `input` and `output` and says how it ended.
pub fn run<R, W>(input: R, output: W, config: Config) -> Exit
where
    R: Read + Send + 'static,
    W: Write,
{
    let Config {
        nonce,
        mode,
        request_timeout,
        write_stall,
        judge,
        host,
        on_stall,
    } = config;
    let (tx, events) = mpsc::channel();
    if std::thread::Builder::new()
        .name("reader".into())
        .spawn(move || read_loop(input, &tx))
        .is_err()
    {
        return Exit::Failed(String::from("cannot start the reading thread"));
    }
    let started = Instant::now();
    let writing_since = Arc::new(AtomicU64::new(NOT_WRITING));
    let _watchdog = start_watchdog(Arc::clone(&writing_since), started, write_stall, on_stall);
    let mut out = Out {
        inner: output,
        started,
        writing_since,
    };

    // Nothing is answered to a peer that cannot say the launch value.
    match next(&events, request_timeout) {
        Ok(Message::Hello(Hello {
            version,
            nonce: got,
        })) if version == PROTOCOL_VERSION && same_nonce(&got, &nonce) => {}
        Ok(Message::Hello(_)) => return Exit::WrongHello,
        Ok(_) => return Exit::Protocol("the first message was not a hello"),
        Err(exit) => return exit,
    }
    let hello = Message::Hello(Hello {
        version: PROTOCOL_VERSION,
        nonce,
    });
    if let Err(exit) = out.send(&hello) {
        return exit;
    }

    let request = match next(&events, request_timeout) {
        Ok(message) => message,
        Err(exit) => return exit,
    };
    match (request, mode) {
        (Message::Scan(request), Mode::Scan) => scan::serve(request, &mut out, &events, &host),
        (Message::Delete(request), Mode::Delete) => {
            serve_delete(&request, &mut out, &events, &judge)
        }
        // The prompt the user answered was for the other kind of work.
        (Message::Scan(_) | Message::Delete(_), _) => Exit::WrongMode,
        (Message::Bye, _) => Exit::PipeClosed,
        _ => Exit::Protocol("the request was not a scan or a delete"),
    }
}

fn serve_delete<W: Write>(
    request: &bs_ipc::DeleteRequest,
    out: &mut Out<W>,
    events: &Receiver<Event>,
    judge: &Judge,
) -> Exit {
    // Checked before any item is looked at: a table that cannot be trusted
    // judges nothing.
    if let Some((code, why)) = judge.refusal() {
        return match out.error(code, why) {
            Ok(()) => Exit::Failed(why.to_owned()),
            Err(exit) => exit,
        };
    }
    let stopped: RefCell<Option<Exit>> = RefCell::new(None);
    let stop = |exit: Exit| {
        stopped.borrow_mut().get_or_insert(exit);
    };
    let reported = judge.run(
        request,
        || {
            // The interface going away, or saying stop, ends the work between
            // items. What was already deleted stays deleted.
            match events.try_recv() {
                Ok(Event::Message(Message::Bye)) | Ok(Event::Closed) => stop(Exit::PipeClosed),
                Ok(Event::Message(_)) => {
                    stop(Exit::Protocol("a message arrived during a delete"));
                }
                Ok(Event::Bad(why)) => stop(Exit::Protocol(why)),
                Err(_) => {}
            }
            stopped.borrow().is_none()
        },
        |index, outcome| {
            let message = Message::DeleteItem {
                index: u32::try_from(index).unwrap_or(u32::MAX),
                outcome,
            };
            match out.send(&message) {
                Ok(()) => true,
                Err(exit) => {
                    stop(exit);
                    false
                }
            }
        },
    );
    if let Some(exit) = stopped.take() {
        return exit;
    }
    let done = Message::DeleteDone {
        items: u32::try_from(reported).unwrap_or(u32::MAX),
    };
    match out.send(&done) {
        Ok(()) => Exit::Done,
        Err(exit) => exit,
    }
}
