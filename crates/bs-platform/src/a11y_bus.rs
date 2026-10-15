//! Which buses the Linux screen-reader adapter may talk to.
//!
//! The adapter reaches screen readers over two D-Bus connections: the session
//! bus, and the accessibility bus whose address comes from
//! `AT_SPI_BUS_ADDRESS` or from the session bus's `org.a11y.Bus.GetAddress`
//! reply. The D-Bus library behind it also understands `tcp:` and `unixexec:`
//! addresses, which would open an internet socket or start a program on
//! Bytescape's behalf. So before the adapter exists, both addresses are checked
//! here: every `;`-separated entry must use exactly the `unix` transport. The
//! accessibility bus address that passed is then written to
//! `AT_SPI_BUS_ADDRESS`, so the adapter uses the address that was checked and
//! never asks again.
//!
//! When anything is refused or cannot be found, the caller runs without the
//! adapter.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::appearance::{Bus, session_bus};

/// How long asking the session bus for the accessibility bus may take.
const ASK_LIMIT: Duration = Duration::from_millis(1500);
/// A real reply is one short address.
const MAX_REPLY: u64 = 4096;

/// Why the adapter must not be used.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Refusal {
    /// The session bus address is not made up entirely of `unix:` entries, or
    /// there is no session bus.
    SessionBusNotLocal,
    /// The accessibility bus address is not made up entirely of `unix:`
    /// entries.
    AtSpiBusNotLocal,
    /// There is no accessibility bus to ask for, or the answer was unusable.
    AtSpiBusUnavailable,
    /// Threads are already running, so the environment cannot be changed
    /// safely.
    ThreadsRunning,
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Refusal::SessionBusNotLocal => {
                "the session bus address is not a local socket (only unix: addresses are used)"
            }
            Refusal::AtSpiBusNotLocal => {
                "the accessibility bus address is not a local socket (only unix: addresses are used)"
            }
            Refusal::AtSpiBusUnavailable => "no accessibility bus is available",
            Refusal::ThreadsRunning => "the program had already started threads",
        })
    }
}

/// Whether `address` is a D-Bus address list in which every entry uses the
/// `unix` transport.
///
/// Entries are separated by `;`, and an entry's transport is the text before
/// its first `:`. That text is compared whole, so `unixexec`, `unix2` or
/// `UNIX` do not pass. Empty entries are skipped, but at least one entry is
/// needed. Anything outside printable ASCII is refused, since D-Bus escapes
/// everything else with `%`.
pub fn is_unix_only(address: &str) -> bool {
    if !address.bytes().all(|b| b.is_ascii_graphic()) {
        return false;
    }
    let mut entries = address
        .split(';')
        .filter(|entry| !entry.is_empty())
        .peekable();
    entries.peek().is_some()
        && entries.all(|entry| matches!(entry.split_once(':'), Some(("unix", _))))
}

/// The address inside the text `gdbus call` prints for a method that returns
/// one string, such as `('unix:path=/run/user/1000/at-spi/bus_0',)`.
pub fn parse_reply(reply: &str) -> Option<String> {
    let inner = reply.trim().strip_prefix("(")?.strip_suffix(",)")?;
    let inner = inner
        .strip_prefix('\'')
        .and_then(|s| s.strip_suffix('\''))
        .or_else(|| inner.strip_prefix('"').and_then(|s| s.strip_suffix('"')))?;
    // An escaped or nested quote means the text is not a plain address.
    if inner.is_empty() || inner.contains(['\'', '"', '\\']) {
        return None;
    }
    Some(inner.to_owned())
}

/// What the environment holds, as the plan needs it.
pub struct Inputs<'a> {
    pub session_bus_address: Option<&'a OsStr>,
    pub runtime_dir: Option<&'a OsStr>,
    pub at_spi_bus_address: Option<&'a OsStr>,
}

/// Asks the session bus for the accessibility bus address.
pub trait AskBus {
    /// The raw address from `org.a11y.Bus.GetAddress`, reaching the session
    /// bus at `session_address`; `None` when it could not be had.
    fn at_spi_address(&self, session_address: &OsStr) -> Option<String>;
}

/// The environment changes that make the adapter use the checked addresses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    /// The session bus address to set, when the environment had none.
    pub session_bus_address: Option<OsString>,
    /// The accessibility bus address to set.
    pub at_spi_bus_address: String,
}

/// Checks both buses and decides what to pin. Changes nothing.
pub fn plan(
    inputs: &Inputs<'_>,
    socket_exists: &dyn Fn(&Path) -> bool,
    ask: &dyn AskBus,
) -> Result<Plan, Refusal> {
    let (session, pinned) = match session_bus(
        inputs.session_bus_address,
        inputs.runtime_dir,
        socket_exists,
    ) {
        Some(Bus::Inherited) => (inputs.session_bus_address.map(OsStr::to_os_string), false),
        Some(Bus::Pinned(address)) => (Some(address), true),
        None => return Err(Refusal::SessionBusNotLocal),
    };
    let session = session.ok_or(Refusal::SessionBusNotLocal)?;
    // The session address may be a list; the checks above covered every entry.
    let at_spi = match inputs.at_spi_bus_address {
        // An empty value counts as unset, as the adapter treats it.
        Some(value) if !value.is_empty() => {
            value.to_str().ok_or(Refusal::AtSpiBusNotLocal)?.to_owned()
        }
        _ => ask
            .at_spi_address(&session)
            .ok_or(Refusal::AtSpiBusUnavailable)?,
    };
    if !is_unix_only(&at_spi) {
        return Err(Refusal::AtSpiBusNotLocal);
    }
    Ok(Plan {
        session_bus_address: pinned.then_some(session),
        at_spi_bus_address: at_spi,
    })
}

/// Asks with `gdbus`, run as the normal user. The program is started only
/// with a session address that has already passed the check.
struct Gdbus;

impl AskBus for Gdbus {
    fn at_spi_address(&self, session_address: &OsStr) -> Option<String> {
        let mut command = Command::new("gdbus");
        command
            .args([
                "call",
                "--session",
                "--timeout",
                "2",
                "--dest",
                "org.a11y.Bus",
                "--object-path",
                "/org/a11y/bus",
                "--method",
                "org.a11y.Bus.GetAddress",
            ])
            .env("DBUS_SESSION_BUS_ADDRESS", session_address);
        let output = run_with_deadline(command, ASK_LIMIT)?;
        parse_reply(&output)
    }
}

/// Runs the command and returns its standard output if it succeeded in time.
/// No thread is started: this runs before the program has any.
fn run_with_deadline(mut command: Command, limit: Duration) -> Option<String> {
    let deadline = Instant::now() + limit;
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return None;
                }
                let mut bytes = Vec::new();
                child
                    .stdout
                    .take()?
                    .take(MAX_REPLY)
                    .read_to_end(&mut bytes)
                    .ok()?;
                return String::from_utf8(bytes).ok();
            }
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(1));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

/// The number of threads in this process, or `None` when it can't be told.
fn thread_count() -> Option<usize> {
    Some(std::fs::read_dir("/proc/self/task").ok()?.count())
}

/// Checks both buses and, if they are local, pins the accessibility bus
/// address for the adapter. Call it at the very start of `main`, before
/// anything starts a thread.
///
/// Returns why the adapter must not be used when it can't be.
pub fn pin_for_adapter() -> Result<(), Refusal> {
    // Changing the environment is sound only while this is the one thread.
    if thread_count() != Some(1) {
        return Err(Refusal::ThreadsRunning);
    }
    let session = std::env::var_os("DBUS_SESSION_BUS_ADDRESS");
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR");
    let at_spi = std::env::var_os("AT_SPI_BUS_ADDRESS");
    let plan = plan(
        &Inputs {
            session_bus_address: session.as_deref(),
            runtime_dir: runtime_dir.as_deref(),
            at_spi_bus_address: at_spi.as_deref(),
        },
        &|p| p.exists(),
        &Gdbus,
    )?;
    // Asking may have taken a moment; nothing started a thread meanwhile, but
    // the count is checked again right before the write.
    if thread_count() != Some(1) {
        return Err(Refusal::ThreadsRunning);
    }
    apply(&plan);
    Ok(())
}

#[allow(unsafe_code)]
fn apply(plan: &Plan) {
    // SAFETY: `pin_for_adapter` has just seen that this process has exactly
    // one thread, the one calling, so no other thread can be reading or
    // writing the environment while it changes. The values hold only printable
    // ASCII (the accessibility address was checked by `is_unix_only`) or an
    // operating-system string that came from the environment itself, so
    // neither contains a NUL byte.
    unsafe {
        if let Some(address) = &plan.session_bus_address {
            std::env::set_var("DBUS_SESSION_BUS_ADDRESS", address);
        }
        std::env::set_var("AT_SPI_BUS_ADDRESS", &plan.at_spi_bus_address);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[test]
    fn only_the_exact_unix_transport_passes() {
        let cases = [
            ("unix:path=/run/user/1000/at-spi/bus_0", true),
            ("unix:abstract=/tmp/dbus-AbCd,guid=1234", true),
            ("unix:path=/a;unix:path=/b", true),
            ("unix:path=/a;", true),
            (";unix:path=/a", true),
            ("tcp:host=127.0.0.1,port=4000", false),
            ("nonce-tcp:host=h,port=1,noncefile=/x", false),
            ("unixexec:path=/bin/sh", false),
            ("autolaunch:", false),
            ("launchd:env=X", false),
            ("unix:path=/a;tcp:host=h,port=1", false),
            ("tcp:host=h,port=1;unix:path=/a", false),
            ("unix:path=/a;unixexec:path=/bin/sh", false),
            ("unix:path=/a;;autolaunch:", false),
            ("UNIX:path=/a", false),
            (" unix:path=/a", false),
            ("unix :path=/a", false),
            ("unix2:path=/a", false),
            ("unix", false),
            ("path=/a", false),
            ("", false),
            (";", false),
            (";;", false),
            ("garbage", false),
            ("unix:path=/a\n;tcp:host=h", false),
            ("unix:path=/caf\u{e9}", false),
            ("unix:path=/a\0b", false),
        ];
        for (address, expected) in cases {
            assert_eq!(is_unix_only(address), expected, "{address:?}");
        }
    }

    #[test]
    fn a_very_long_hostile_address_is_refused_without_trouble() {
        let long = format!("unix:path=/{}", "a".repeat(100_000));
        assert!(is_unix_only(&long));
        let bad = format!("{long};tcp:host=h");
        assert!(!is_unix_only(&bad));
        assert!(!is_unix_only(&"\u{202e}".repeat(1000)));
    }

    #[test]
    fn the_reply_is_read_only_when_it_is_one_plain_string() {
        let cases = [
            (
                "('unix:path=/run/user/1000/at-spi/bus_0,guid=ab',)\n",
                Some("unix:path=/run/user/1000/at-spi/bus_0,guid=ab"),
            ),
            ("(\"unix:path=/x\",)", Some("unix:path=/x")),
            ("('tcp:host=h,port=1',)", Some("tcp:host=h,port=1")),
            ("('unix:path=/x', 'tcp:host=h')", None),
            ("('unix:path=/it's',)", None),
            ("('unix:path=/a\\'b',)", None),
            ("('',)", None),
            ("()", None),
            ("", None),
            ("unix:path=/x", None),
            ("(<'unix:path=/x'>,)", None),
        ];
        for (reply, expected) in cases {
            assert_eq!(parse_reply(reply).as_deref(), expected, "{reply:?}");
        }
    }

    struct Fake {
        reply: Option<&'static str>,
        asked_with: RefCell<Vec<OsString>>,
    }

    impl Fake {
        fn new(reply: Option<&'static str>) -> Self {
            Self {
                reply,
                asked_with: RefCell::new(Vec::new()),
            }
        }
    }

    impl AskBus for Fake {
        fn at_spi_address(&self, session_address: &OsStr) -> Option<String> {
            self.asked_with
                .borrow_mut()
                .push(session_address.to_owned());
            self.reply.map(str::to_owned)
        }
    }

    fn os(s: &str) -> Option<&OsStr> {
        Some(OsStr::new(s))
    }

    const SESSION: &str = "unix:path=/run/user/1000/bus";
    const ATSPI: &str = "unix:path=/run/user/1000/at-spi/bus_0";

    fn run(
        session: Option<&str>,
        at_spi: Option<&str>,
        reply: Option<&'static str>,
    ) -> (Result<Plan, Refusal>, Fake) {
        let fake = Fake::new(reply);
        let result = plan(
            &Inputs {
                session_bus_address: session.map(OsStr::new),
                runtime_dir: os("/run/user/1000"),
                at_spi_bus_address: at_spi.map(OsStr::new),
            },
            &|_| true,
            &fake,
        );
        (result, fake)
    }

    #[test]
    fn local_buses_are_pinned_and_the_bus_is_not_asked_when_the_variable_is_set() {
        let (result, fake) = run(Some(SESSION), Some(ATSPI), None);
        assert_eq!(
            result,
            Ok(Plan {
                session_bus_address: None,
                at_spi_bus_address: ATSPI.into()
            })
        );
        assert!(fake.asked_with.borrow().is_empty());
    }

    #[test]
    fn the_reply_is_used_and_checked_when_the_variable_is_unset_or_empty() {
        for variable in [None, Some("")] {
            let (result, fake) = run(Some(SESSION), variable, Some(ATSPI));
            assert_eq!(result.unwrap().at_spi_bus_address, ATSPI);
            assert_eq!(*fake.asked_with.borrow(), vec![OsString::from(SESSION)]);
        }
    }

    #[test]
    fn an_unset_session_address_is_pinned_to_the_runtime_socket() {
        let (result, fake) = run(None, None, Some(ATSPI));
        let plan = result.unwrap();
        assert_eq!(
            plan.session_bus_address,
            Some(OsString::from("unix:path=/run/user/1000/bus"))
        );
        // The question is asked of exactly the bus that was pinned.
        assert_eq!(
            *fake.asked_with.borrow(),
            vec![OsString::from("unix:path=/run/user/1000/bus")]
        );
    }

    #[test]
    fn a_non_local_session_bus_is_refused_and_nothing_is_asked() {
        for session in [
            "tcp:host=example.org,port=4000",
            "unix:path=/a;tcp:host=h,port=1",
            "unixexec:path=/bin/sh",
            "autolaunch:",
            "nonce-tcp:host=h,port=1",
            "launchd:env=X",
            "",
            "garbage",
        ] {
            let (result, fake) = run(Some(session), Some(ATSPI), Some(ATSPI));
            assert_eq!(result, Err(Refusal::SessionBusNotLocal), "{session:?}");
            assert!(fake.asked_with.borrow().is_empty(), "{session:?}");
        }
    }

    #[test]
    fn a_non_local_accessibility_bus_from_the_variable_is_refused() {
        for at_spi in [
            "tcp:host=example.org,port=4000",
            "nonce-tcp:host=h,port=1,noncefile=/x",
            "unixexec:path=/bin/sh,argv1=-c",
            "autolaunch:",
            "launchd:env=X",
            "unix:path=/a;tcp:host=h,port=1",
            "tcp:host=h,port=1;unix:path=/a",
            "garbage",
        ] {
            let (result, fake) = run(Some(SESSION), Some(at_spi), Some(ATSPI));
            assert_eq!(result, Err(Refusal::AtSpiBusNotLocal), "{at_spi:?}");
            // A bad variable is never replaced by asking the bus.
            assert!(fake.asked_with.borrow().is_empty(), "{at_spi:?}");
        }
    }

    #[test]
    fn a_non_local_accessibility_bus_from_the_reply_is_refused() {
        for reply in [
            "tcp:host=example.org,port=4000",
            "unixexec:path=/bin/sh",
            "unix:path=/a;nonce-tcp:host=h,port=1",
            "autolaunch:",
            "launchd:env=X",
        ] {
            let (result, _) = run(Some(SESSION), None, Some(reply));
            assert_eq!(result, Err(Refusal::AtSpiBusNotLocal), "{reply:?}");
        }
    }

    #[test]
    fn no_answer_means_no_adapter() {
        let (result, _) = run(Some(SESSION), None, None);
        assert_eq!(result, Err(Refusal::AtSpiBusUnavailable));
    }

    #[cfg(unix)]
    #[test]
    fn an_environment_value_that_is_not_text_is_refused() {
        use std::os::unix::ffi::OsStrExt;
        let fake = Fake::new(Some(ATSPI));
        let raw = OsStr::from_bytes(b"unix:path=/run/\xff");
        let result = plan(
            &Inputs {
                session_bus_address: os(SESSION),
                runtime_dir: None,
                at_spi_bus_address: Some(raw),
            },
            &|_| true,
            &fake,
        );
        assert_eq!(result, Err(Refusal::AtSpiBusNotLocal));
    }

    #[test]
    fn a_program_that_never_answers_is_stopped() {
        let mut command = Command::new("sleep");
        command.arg("30");
        let start = Instant::now();
        assert_eq!(run_with_deadline(command, Duration::from_millis(100)), None);
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_failing_program_gives_no_answer() {
        assert_eq!(run_with_deadline(Command::new("false"), ASK_LIMIT), None);
        assert_eq!(
            run_with_deadline(Command::new("/nonexistent/gdbus"), ASK_LIMIT),
            None
        );
    }

    #[test]
    fn the_environment_is_left_alone_once_threads_exist() {
        // A second thread is guaranteed, however the harness runs the test.
        let (stop, parked) = std::sync::mpsc::channel::<()>();
        let helper = std::thread::spawn(move || {
            let _ = parked.recv();
        });
        let before = std::env::var_os("AT_SPI_BUS_ADDRESS");
        assert_eq!(pin_for_adapter(), Err(Refusal::ThreadsRunning));
        assert_eq!(std::env::var_os("AT_SPI_BUS_ADDRESS"), before);
        drop(stop);
        helper.join().unwrap();
    }

    #[test]
    fn refusals_read_as_one_plain_line() {
        for refusal in [
            Refusal::SessionBusNotLocal,
            Refusal::AtSpiBusNotLocal,
            Refusal::AtSpiBusUnavailable,
            Refusal::ThreadsRunning,
        ] {
            let text = refusal.to_string();
            assert!(!text.contains('\n') && !text.is_empty());
        }
    }
}
