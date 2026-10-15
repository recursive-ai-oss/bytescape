//! Starting the elevated helper and talking to it.
//!
//! The interface never runs elevated. When the user asks for a full scan or an
//! elevated delete, this module starts the helper through the system's
//! elevation prompt, checks that what answers is really that helper, and
//! speaks the protocol of `bs-ipc` to it. Each helper does one job and exits.
//!
//! # Linux
//!
//! The helper is started with `pkexec`, which asks the user to authenticate.
//! Its standard input and output are pipes that only this process holds the
//! other ends of, so the connection itself is the proof of who is on the other
//! end. The helper checks its own side (who started it, and that it talks over
//! pipes). A scan and a delete are separate actions for the system's policy:
//! the prompt for a scan may be remembered for the session, the prompt for a
//! delete is asked every time.
//!
//! # Windows
//!
//! This process creates a pipe with a random name, open to the current user
//! only and to no remote client, and starts the helper through the shell's
//! `runas` verb, which shows the elevation prompt. It then checks that the
//! process that connected is the one it started and runs the helper's program,
//! and the helper checks the same about this process. The pipe is created
//! before the helper starts, as its first instance, so no other process can
//! take its name.
//!
//! # Using it
//!
//! ```no_run
//! use bs_platform::elevate::{Elevation, ScanEvent};
//! use bs_ipc::{Backend, ScanRequest, path_to_wire};
//!
//! let elevation = Elevation::system()?;
//! let mut scan = elevation.scan(ScanRequest {
//!     volume: path_to_wire(std::path::Path::new("/")),
//!     backend: Backend::Auto,
//!     extra_mount_ids: vec![],
//!     places: bs_platform::elevate::user_places(),
//! })?;
//! let stop = scan.canceller(); // give this to the thread that handles Cancel
//! while let Some(event) = scan.next_event()? {
//!     if let ScanEvent::Done(_) = event {
//!         break;
//!     }
//! }
//! # let _ = stop;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

pub use bs_ipc::{Backend, Fallback, Progress, Refusal, ScanDone, ScanEnd, ScanEvent};
use bs_ipc::{
    Client, ClientError, DeleteItem, DeleteRequest, ItemOutcome, NONCE_LEN, Place, ScanRequest,
    Seen, SharedWriter,
};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(windows)]
mod windows;

/// Why the helper could not be used.
#[derive(Debug)]
#[non_exhaustive]
pub enum ElevateError {
    /// Elevation is not available here: no `pkexec`, the helper is not
    /// installed where it is expected, or this process is already elevated.
    Unavailable(&'static str),
    /// The user declined the prompt, or is not allowed to authorize it.
    Declined,
    /// The helper could not be started.
    Launch(std::io::Error),
    /// What connected is not the helper that was started.
    PeerNotVerified(&'static str),
    /// The helper did not connect in time.
    TimedOut,
    /// The conversation with the helper failed.
    Protocol(ClientError),
}

impl std::fmt::Display for ElevateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(why) => write!(f, "elevation is not available: {why}"),
            Self::Declined => f.write_str("the request for administrator rights was declined"),
            Self::Launch(e) => write!(f, "the helper could not be started: {e}"),
            Self::PeerNotVerified(why) => {
                write!(f, "the program that answered is not the helper: {why}")
            }
            Self::TimedOut => f.write_str("the helper did not answer in time"),
            Self::Protocol(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ElevateError {}

impl From<ClientError> for ElevateError {
    fn from(e: ClientError) -> Self {
        Self::Protocol(e)
    }
}

/// An elevated delete that did not finish.
#[derive(Debug)]
pub struct ElevatedDeleteError {
    pub error: ElevateError,
    /// The items that have an outcome (by index in the request). Any other
    /// item may or may not have been deleted.
    pub completed: Vec<(u32, ItemOutcome)>,
}

impl std::fmt::Display for ElevatedDeleteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} ({} items have an outcome)",
            self.error,
            self.completed.len()
        )
    }
}

impl std::error::Error for ElevatedDeleteError {}

/// One item to delete with the helper's rights.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeleteTarget {
    /// The absolute path, as the scan reported it.
    pub path: PathBuf,
    /// The user held the confirmation for this protected path.
    pub confirmed_protected: bool,
    /// What the scan saw there (volume and file identity); the helper leaves
    /// the item alone if it is something else now.
    pub seen: Option<(u64, u128)>,
}

type Reader = Box<dyn Read + Send>;
type Writer = SharedWriter<Box<dyn Write + Send>>;

/// How to start the helper.
#[derive(Clone, Debug)]
pub struct Elevation {
    helper: PathBuf,
    #[cfg(unix)]
    pkexec: Option<PathBuf>,
}

impl Elevation {
    /// The helper that is installed with this program: the packaged location on
    /// Linux, the program beside this one on Windows.
    pub fn system() -> Result<Self, ElevateError> {
        #[cfg(target_os = "linux")]
        {
            Ok(Self {
                helper: PathBuf::from(linux::INSTALLED_HELPER),
                pkexec: None,
            })
        }
        #[cfg(windows)]
        {
            Ok(Self {
                helper: windows::helper_beside_this_program()?,
            })
        }
        #[cfg(not(any(target_os = "linux", windows)))]
        {
            Err(ElevateError::Unavailable("not supported on this system"))
        }
    }

    /// Starts the helper at `helper` instead of the installed one (a portable
    /// or unpacked copy). On Linux the system's prompt is then the generic one,
    /// because the policy names the installed path.
    pub fn with_helper(helper: impl Into<PathBuf>) -> Self {
        Self {
            helper: helper.into(),
            #[cfg(unix)]
            pkexec: None,
        }
    }

    /// Uses the given `pkexec` program. For tests, which start the helper
    /// through a stand-in that does not ask for a password.
    #[doc(hidden)]
    #[cfg(unix)]
    #[must_use]
    pub fn with_pkexec(mut self, pkexec: impl Into<PathBuf>) -> Self {
        self.pkexec = Some(pkexec.into());
        self
    }

    /// The program that will be started.
    pub fn helper_path(&self) -> &Path {
        &self.helper
    }

    /// Starts a helper for a scan and sends the request.
    pub fn scan(&self, request: ScanRequest) -> Result<ElevatedScan, ElevateError> {
        let mut session = self.launch("scan")?;
        session.client.start_scan(request)?;
        Ok(ElevatedScan { session })
    }

    /// Starts a helper for a delete, sends the request and waits for every
    /// item's outcome. The outcomes are in the order of `items`.
    ///
    /// The helper deletes permanently only; on Linux that is the only way an
    /// elevated delete can work (the superuser's Trash is not the user's), and
    /// the confirmation the interface shows must say so.
    pub fn delete(&self, items: &[DeleteTarget]) -> Result<Vec<ItemOutcome>, ElevatedDeleteError> {
        let none = |error| ElevatedDeleteError {
            error,
            completed: Vec::new(),
        };
        let request = DeleteRequest {
            permanent: true,
            items: items
                .iter()
                .map(|t| DeleteItem {
                    path: bs_ipc::path_to_wire(&t.path),
                    confirmed_protected: t.confirmed_protected,
                    seen: t.seen.map(|(volume, file)| Seen { volume, file }),
                })
                .collect(),
        };
        let mut session = self.launch("delete").map_err(none)?;
        // The helper exits by itself after a delete, and when the connection
        // closes below.
        session
            .client
            .delete(request)
            .map_err(|failure| ElevatedDeleteError {
                error: failure.error.into(),
                completed: failure.completed,
            })
    }

    fn launch(&self, mode: &'static str) -> Result<Session, ElevateError> {
        let nonce = random_nonce()?;
        #[cfg(target_os = "linux")]
        let launched = linux::launch(self, mode, nonce)?;
        #[cfg(windows)]
        let launched = windows::launch(self, mode, nonce)?;
        #[cfg(not(any(target_os = "linux", windows)))]
        let launched: Launched = {
            let _ = (mode, nonce);
            return Err(ElevateError::Unavailable("not supported on this system"));
        };
        let writer = SharedWriter::new(launched.writer);
        let client = match Client::connect(launched.reader, writer.clone(), nonce) {
            Ok(client) => client,
            Err(error) => return Err(launched.guard.explain(error)),
        };
        Ok(Session {
            client,
            writer,
            _guard: launched.guard,
        })
    }
}

/// A started helper, before the connection is made.
pub(crate) struct Launched {
    pub(crate) reader: Reader,
    pub(crate) writer: Box<dyn Write + Send>,
    pub(crate) guard: Guard,
}

#[cfg(target_os = "linux")]
pub(crate) use linux::Guard;
#[cfg(windows)]
pub(crate) use windows::Guard;
#[cfg(not(any(target_os = "linux", windows)))]
pub(crate) struct Guard;

struct Session {
    // The connection closes before the guard runs, so the helper sees it end.
    client: Client<Reader, Writer>,
    writer: Writer,
    _guard: Guard,
}

/// A running elevated scan.
pub struct ElevatedScan {
    session: Session,
}

impl ElevatedScan {
    /// The next event; `Ok(None)` after the scan's end has been returned.
    pub fn next_event(&mut self) -> Result<Option<ScanEvent>, ElevateError> {
        Ok(self.session.client.next_scan_event()?)
    }

    /// A handle that stops the scan from another thread. The scan then ends
    /// with a done event marked cancelled.
    pub fn canceller(&self) -> Canceller {
        Canceller {
            writer: self.session.writer.clone(),
        }
    }
}

/// Stops a running elevated scan. Cloneable, usable from any thread.
#[derive(Clone)]
pub struct Canceller {
    writer: Writer,
}

impl Canceller {
    /// Asks the helper to stop. Does nothing if it has already finished.
    pub fn cancel(&self) {
        let mut writer = self.writer.clone();
        let _ = bs_ipc::write_message(&mut writer, &bs_ipc::Message::Bye);
    }
}

/// The user's places as the interface sees them, for the helper to use as the
/// base of the category tables: the helper runs as another user, so it cannot
/// read them from its own environment.
pub fn user_places() -> Vec<Place> {
    bs_knowledge::Environment::variable_names()
        .iter()
        .filter_map(|name| {
            let value = std::env::var_os(name)?;
            let path = PathBuf::from(value);
            path.is_absolute().then(|| Place {
                name: (*name).to_owned(),
                value: bs_ipc::path_to_wire(&path),
            })
        })
        .collect()
}

/// Starts a helper with the installed location and the system's prompt, and
/// reads `volume`. The stream of events is the scan.
pub fn elevate_and_scan(
    volume: &Path,
    backend: Backend,
    extra_mount_ids: Vec<u64>,
) -> Result<ElevatedScan, ElevateError> {
    Elevation::system()?.scan(ScanRequest {
        volume: bs_ipc::path_to_wire(volume),
        backend,
        extra_mount_ids,
        places: user_places(),
    })
}

/// Starts a helper with the system's prompt and deletes `items` permanently,
/// with one outcome per item.
pub fn elevate_and_delete(items: &[DeleteTarget]) -> Result<Vec<ItemOutcome>, ElevatedDeleteError> {
    Elevation::system()
        .map_err(|error| ElevatedDeleteError {
            error,
            completed: Vec::new(),
        })?
        .delete(items)
}

/// Random bytes from the system, for the launch value and the pipe name.
pub(crate) fn random_bytes(buf: &mut [u8]) -> Result<(), ElevateError> {
    #[cfg(target_os = "linux")]
    {
        linux::random_bytes(buf)
    }
    #[cfg(windows)]
    {
        windows::random_bytes(buf)
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = buf;
        Err(ElevateError::Unavailable("not supported on this system"))
    }
}

fn random_nonce() -> Result<[u8; NONCE_LEN], ElevateError> {
    let mut nonce = [0u8; NONCE_LEN];
    random_bytes(&mut nonce)?;
    Ok(nonce)
}

/// The nonce as the 32 lower-case hex digits the command line carries.
pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
