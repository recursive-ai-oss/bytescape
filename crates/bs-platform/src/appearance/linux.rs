//! Light/dark and accent on Linux, from the desktop portal and GNOME settings.

use super::{Appearance, ColorScheme, Rgb};
use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

/// How long all the reads together may take.
pub(crate) const OVERALL_BUDGET: Duration = Duration::from_millis(1500);
/// How long one helper program may run.
pub(crate) const PER_CALL_LIMIT: Duration = Duration::from_millis(1000);
/// Output beyond this is ignored; real answers are a few dozen bytes.
const MAX_OUTPUT: u64 = 4096;

/// Runs a program and returns its standard output if it succeeded within
/// `limit`.
pub(crate) trait Runner {
    fn run(&self, program: &str, args: &[&str], limit: Duration) -> Option<String>;
}

/// How the helper programs reach the session bus.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Bus {
    /// The address in the environment, which is a local socket.
    Inherited,
    /// No address in the environment: use the user's runtime socket.
    Pinned(OsString),
}

/// Decides whether the helper programs may be started, and how they reach the
/// session bus.
///
/// A D-Bus address can name a TCP transport, and a program started with one
/// would open an internet socket on Bytescape's behalf. So the programs are
/// started only when the bus is reached over a local Unix socket: the address
/// in `DBUS_SESSION_BUS_ADDRESS` has to be made up entirely of `unix:`
/// transports, or the variable has to be unset and the user's runtime
/// directory has to hold a `bus` socket, which is then named explicitly.
/// Anything else, including an empty or unreadable value, means no program is
/// started.
pub(crate) fn session_bus(
    address: Option<&OsStr>,
    runtime_dir: Option<&OsStr>,
    exists: &dyn Fn(&Path) -> bool,
) -> Option<Bus> {
    match address {
        Some(address) => is_unix_only(address.to_str()?).then_some(Bus::Inherited),
        None => {
            let socket = Path::new(runtime_dir?).join("bus");
            if !socket.is_absolute() || !exists(&socket) {
                return None;
            }
            let mut pinned = OsString::from("unix:path=");
            pinned.push(socket.as_os_str());
            Some(Bus::Pinned(pinned))
        }
    }
}

/// Whether every transport in a D-Bus address list is `unix:`. Entries are
/// separated by `;`. Empty entries are ignored, but at least one is needed.
fn is_unix_only(address: &str) -> bool {
    let mut entries = address
        .split(';')
        .filter(|entry| !entry.is_empty())
        .peekable();
    entries.peek().is_some() && entries.all(|entry| entry.starts_with("unix:"))
}

struct Processes {
    bus: Bus,
}

impl Runner for Processes {
    fn run(&self, program: &str, args: &[&str], limit: Duration) -> Option<String> {
        let mut command = Command::new(program);
        command.args(args);
        if let Bus::Pinned(address) = &self.bus {
            command.env("DBUS_SESSION_BUS_ADDRESS", address);
        }
        run_with_deadline(command, limit)
    }
}

/// Runs the command, killing it if it has not finished by the deadline.
/// Returns its standard output when it exits successfully.
fn run_with_deadline(mut command: Command, limit: Duration) -> Option<String> {
    let deadline = Instant::now() + limit;
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = stdout.by_ref().take(MAX_OUTPUT).read_to_end(&mut buffer);
        let _ = sender.send(buffer);
    });
    if let Ok(buffer) = receiver.recv_timeout(limit) {
        // The output has ended, so the program should be exiting.
        while Instant::now() < deadline {
            match child.try_wait() {
                Ok(Some(status)) => {
                    return status
                        .success()
                        .then(|| String::from_utf8_lossy(&buffer).into_owned());
                }
                Ok(None) => thread::sleep(Duration::from_millis(5)),
                Err(_) => break,
            }
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    None
}

/// Reads the appearance from the real system.
pub(crate) fn read_system() -> Appearance {
    let address = std::env::var_os("DBUS_SESSION_BUS_ADDRESS");
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR");
    let bus = session_bus(address.as_deref(), runtime_dir.as_deref(), &|p| p.exists());
    match bus {
        Some(bus) => read(&Processes { bus }, true, OVERALL_BUDGET, PER_CALL_LIMIT),
        None => read(
            &Processes {
                bus: Bus::Inherited,
            },
            false,
            OVERALL_BUDGET,
            PER_CALL_LIMIT,
        ),
    }
}

/// Spends a fixed time across several runs.
struct Budget<'a> {
    runner: &'a dyn Runner,
    deadline: Instant,
    per_call: Duration,
}

impl Budget<'_> {
    fn run(&self, program: &str, args: &[&str]) -> Option<String> {
        let left = self.deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return None;
        }
        self.runner.run(program, args, left.min(self.per_call))
    }
}

/// Reads the setting. With `bus_allowed` false nothing is started and the
/// default is returned. All the runs together take at most `budget`, and each
/// at most `per_call`.
pub(crate) fn read(
    runner: &dyn Runner,
    bus_allowed: bool,
    budget: Duration,
    per_call: Duration,
) -> Appearance {
    if !bus_allowed {
        return Appearance::default();
    }
    let budget = Budget {
        runner,
        deadline: Instant::now() + budget,
        per_call,
    };
    let portal = |key: &str| -> Option<String> {
        // `ReadOne` is the current call; older portals only have `Read`.
        ["ReadOne", "Read"].iter().find_map(|method| {
            let method = format!("org.freedesktop.portal.Settings.{method}");
            budget.run(
                "gdbus",
                &[
                    "call",
                    "--session",
                    "--timeout",
                    "1",
                    "--dest",
                    "org.freedesktop.portal.Desktop",
                    "--object-path",
                    "/org/freedesktop/portal/desktop",
                    "--method",
                    &method,
                    "org.freedesktop.appearance",
                    key,
                ],
            )
        })
    };

    let scheme = match portal("color-scheme")
        .as_deref()
        .and_then(parse_portal_color_scheme)
    {
        Some(PortalScheme::Dark) => ColorScheme::Dark,
        Some(PortalScheme::Light) => ColorScheme::Light,
        // No preference, or no portal: ask GNOME's settings.
        Some(PortalScheme::NoPreference) | None => budget
            .run(
                "gsettings",
                &["get", "org.gnome.desktop.interface", "color-scheme"],
            )
            .as_deref()
            .and_then(parse_gsettings_color_scheme)
            .unwrap_or(ColorScheme::Light),
    };
    let accent = portal("accent-color")
        .as_deref()
        .and_then(parse_portal_accent);
    Appearance { scheme, accent }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PortalScheme {
    NoPreference,
    Dark,
    Light,
}

/// Parses the reply to a portal read of `color-scheme`, such as
/// `(<uint32 1>,)` or `(<<uint32 1>>,)`. 0 is no preference, 1 prefers dark
/// and 2 prefers light. Any other value is not understood.
pub(crate) fn parse_portal_color_scheme(output: &str) -> Option<PortalScheme> {
    let after = output.split_once("uint32")?.1;
    let digits: String = after
        .trim_start()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    match digits.as_str() {
        "0" => Some(PortalScheme::NoPreference),
        "1" => Some(PortalScheme::Dark),
        "2" => Some(PortalScheme::Light),
        _ => None,
    }
}

/// Parses the reply to a portal read of `accent-color`, such as
/// `(<(0.208, 0.518, 0.894)>,)`: red, green and blue between 0 and 1. A value
/// outside that range is how the portal says the user has no accent color.
pub(crate) fn parse_portal_accent(output: &str) -> Option<Rgb> {
    let inner = output.rsplit_once('(')?.1;
    let inner = inner.split_once(')')?.0;
    let mut numbers = inner.split(',').map(|part| part.trim().parse::<f64>());
    let (r, g, b) = (
        numbers.next()?.ok()?,
        numbers.next()?.ok()?,
        numbers.next()?.ok()?,
    );
    if numbers.next().is_some() {
        return None;
    }
    Rgb::from_unit_floats(r, g, b)
}

/// Parses `gsettings get ... color-scheme`, which prints `'prefer-dark'`,
/// `'prefer-light'` or `'default'`.
pub(crate) fn parse_gsettings_color_scheme(output: &str) -> Option<ColorScheme> {
    match output.trim().trim_matches('\'') {
        "prefer-dark" => Some(ColorScheme::Dark),
        "prefer-light" | "default" => Some(ColorScheme::Light),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[test]
    fn color_scheme_replies() {
        let cases = [
            ("(<uint32 1>,)", Some(PortalScheme::Dark)),
            ("(<<uint32 1>>,)\n", Some(PortalScheme::Dark)),
            ("(<uint32 2>,)", Some(PortalScheme::Light)),
            ("(<uint32 0>,)", Some(PortalScheme::NoPreference)),
            ("(<uint32 3>,)", None),
            ("(<uint32 12>,)", None),
            ("(<uint32 >,)", None),
            ("(<'dark'>,)", None),
            ("", None),
            ("uint32", None),
        ];
        for (text, expected) in cases {
            assert_eq!(parse_portal_color_scheme(text), expected, "{text:?}");
        }
    }

    #[test]
    fn accent_replies() {
        assert_eq!(
            parse_portal_accent("(<(0.2078, 0.5176, 0.8941)>,)"),
            Some(Rgb {
                r: 53,
                g: 132,
                b: 228
            })
        );
        assert_eq!(
            parse_portal_accent("(<<(1.0, 0.0, 0.5)>>,)\n"),
            Some(Rgb {
                r: 255,
                g: 0,
                b: 128
            })
        );
        let none: [&str; 8] = [
            // The portal's way of saying "no accent".
            "(<(-1.0, -1.0, -1.0)>,)",
            "(<(2.0, 0.0, 0.0)>,)",
            "(<(0.1, 0.2)>,)",
            "(<(0.1, 0.2, 0.3, 0.4)>,)",
            "(<(a, b, c)>,)",
            "(<(nan, 0.0, 0.0)>,)",
            "(<uint32 1>,)",
            "",
        ];
        for text in none {
            assert_eq!(parse_portal_accent(text), None, "{text:?}");
        }
    }

    #[test]
    fn gsettings_replies() {
        assert_eq!(
            parse_gsettings_color_scheme("'prefer-dark'\n"),
            Some(ColorScheme::Dark)
        );
        assert_eq!(
            parse_gsettings_color_scheme("'prefer-light'\n"),
            Some(ColorScheme::Light)
        );
        assert_eq!(
            parse_gsettings_color_scheme("'default'\n"),
            Some(ColorScheme::Light)
        );
        assert_eq!(parse_gsettings_color_scheme("'unknown'"), None);
        assert_eq!(parse_gsettings_color_scheme(""), None);
    }

    fn os(text: &str) -> Option<&OsStr> {
        Some(OsStr::new(text))
    }

    #[test]
    fn only_local_socket_buses_may_be_used() {
        let socket_present = |_: &Path| true;
        let no_socket = |_: &Path| false;
        #[rustfmt::skip]
        let cases: [(&str, bool); 14] = [
            ("unix:path=/run/user/1000/bus", true),
            ("unix:abstract=/tmp/dbus-AbCd,guid=1234", true),
            ("unix:path=/a;unix:path=/b", true),
            ("unix:path=/a;", true),
            // Anything that is not a Unix socket is refused, alone or in a list.
            ("tcp:host=localhost,port=55555", false),
            ("tcp:host=10.0.0.5,port=1", false),
            ("nonce-tcp:host=localhost,port=1,noncefile=/x", false),
            ("unix:path=/a;tcp:host=h,port=1", false),
            ("tcp:host=h,port=1;unix:path=/a", false),
            ("unixexec:path=/bin/sh", false),
            ("autolaunch:", false),
            ("launchd:env=X", false),
            ("", false),
            (";", false),
        ];
        for (address, allowed) in cases {
            let got = session_bus(os(address), os("/run/user/1000"), &socket_present);
            assert_eq!(got.is_some(), allowed, "{address:?}");
            if allowed {
                assert_eq!(got, Some(Bus::Inherited), "{address:?}");
            }
            // The runtime socket never rescues a set address.
            let got = session_bus(os(address), os("/run/user/1000"), &no_socket);
            assert_eq!(got.is_some(), allowed, "{address:?}");
        }
        // Case matters: transports are lower case.
        assert_eq!(session_bus(os("UNIX:path=/x"), None, &socket_present), None);
        // An address that is not text cannot be checked, so it is refused.
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let raw = OsStr::from_bytes(b"unix:path=/run/\xff");
            assert_eq!(session_bus(Some(raw), None, &socket_present), None);
        }
    }

    #[test]
    fn an_unset_address_uses_the_runtime_socket_only_if_it_exists() {
        let exists = |p: &Path| p == Path::new("/run/user/1000/bus");
        assert_eq!(
            session_bus(None, os("/run/user/1000"), &exists),
            Some(Bus::Pinned(OsString::from("unix:path=/run/user/1000/bus")))
        );
        assert_eq!(session_bus(None, os("/run/user/1001"), &exists), None);
        assert_eq!(session_bus(None, None, &exists), None);
        // A relative runtime directory is not trusted.
        let anything = |_: &Path| true;
        assert_eq!(session_bus(None, os("run/user"), &anything), None);
        assert_eq!(session_bus(None, os(""), &anything), None);
    }

    /// Answers programs from a script and records what was asked.
    struct Script {
        portal_scheme: Option<&'static str>,
        portal_accent: Option<&'static str>,
        gsettings: Option<&'static str>,
        /// Whether the old `Read` method works while `ReadOne` fails.
        only_old_read: bool,
        asked: RefCell<Vec<String>>,
    }

    impl Script {
        fn new() -> Self {
            Self {
                portal_scheme: None,
                portal_accent: None,
                gsettings: None,
                only_old_read: false,
                asked: RefCell::new(Vec::new()),
            }
        }
    }

    impl Runner for Script {
        fn run(&self, program: &str, args: &[&str], _limit: Duration) -> Option<String> {
            self.asked
                .borrow_mut()
                .push(format!("{program} {}", args.join(" ")));
            match program {
                "gdbus" => {
                    let method = args
                        .iter()
                        .position(|a| *a == "--method")
                        .map(|i| args[i + 1])?;
                    if self.only_old_read && method.ends_with(".ReadOne") {
                        return None;
                    }
                    match *args.last()? {
                        "color-scheme" => self.portal_scheme.map(str::to_owned),
                        "accent-color" => self.portal_accent.map(str::to_owned),
                        _ => None,
                    }
                }
                "gsettings" => self.gsettings.map(str::to_owned),
                _ => None,
            }
        }
    }

    fn read_with(script: &Script, bus_allowed: bool) -> Appearance {
        read(script, bus_allowed, OVERALL_BUDGET, PER_CALL_LIMIT)
    }

    #[test]
    fn portal_answer_wins_over_gsettings() {
        let mut script = Script::new();
        script.portal_scheme = Some("(<uint32 1>,)");
        script.portal_accent = Some("(<(0.0, 1.0, 0.0)>,)");
        script.gsettings = Some("'prefer-light'");
        let got = read_with(&script, true);
        assert_eq!(got.scheme, ColorScheme::Dark);
        assert_eq!(got.accent, Some(Rgb { r: 0, g: 255, b: 0 }));
        assert!(
            !script
                .asked
                .borrow()
                .iter()
                .any(|a| a.starts_with("gsettings"))
        );
    }

    #[test]
    fn no_preference_falls_back_to_gsettings() {
        let mut script = Script::new();
        script.portal_scheme = Some("(<uint32 0>,)");
        script.gsettings = Some("'prefer-dark'\n");
        assert_eq!(read_with(&script, true).scheme, ColorScheme::Dark);
    }

    #[test]
    fn no_portal_falls_back_to_gsettings_then_light() {
        let mut script = Script::new();
        script.gsettings = Some("'prefer-dark'\n");
        assert_eq!(read_with(&script, true).scheme, ColorScheme::Dark);
        let nothing = Script::new();
        assert_eq!(
            read_with(&nothing, true),
            Appearance {
                scheme: ColorScheme::Light,
                accent: None
            }
        );
    }

    #[test]
    fn without_a_permitted_bus_no_program_is_started() {
        let mut script = Script::new();
        script.portal_scheme = Some("(<uint32 1>,)");
        script.portal_accent = Some("(<(0.0, 1.0, 0.0)>,)");
        script.gsettings = Some("'prefer-dark'");
        let got = read_with(&script, false);
        assert_eq!(got, Appearance::default());
        assert!(script.asked.borrow().is_empty(), "{:?}", script.asked);
    }

    #[test]
    fn older_portals_without_read_one_are_handled() {
        let mut script = Script::new();
        script.only_old_read = true;
        script.portal_scheme = Some("(<<uint32 2>>,)");
        assert_eq!(read_with(&script, true).scheme, ColorScheme::Light);
        script.portal_scheme = Some("(<<uint32 1>>,)");
        assert_eq!(read_with(&script, true).scheme, ColorScheme::Dark);
    }

    #[test]
    fn garbage_from_the_portal_is_ignored() {
        let mut script = Script::new();
        script.portal_scheme = Some("\u{0}\u{1}garbage");
        script.portal_accent = Some("(<(9, 9, 9)>,)");
        let got = read_with(&script, true);
        assert_eq!(
            got,
            Appearance {
                scheme: ColorScheme::Light,
                accent: None
            }
        );
    }

    /// Takes as long as it is allowed to, and answers nothing, like a desktop
    /// that does not reply.
    struct Hanging {
        limits: RefCell<Vec<Duration>>,
    }

    impl Runner for Hanging {
        fn run(&self, _program: &str, _args: &[&str], limit: Duration) -> Option<String> {
            self.limits.borrow_mut().push(limit);
            thread::sleep(limit);
            None
        }
    }

    #[test]
    fn all_reads_together_stay_inside_one_deadline() {
        let hanging = Hanging {
            limits: RefCell::new(Vec::new()),
        };
        let started = Instant::now();
        let got = read(
            &hanging,
            true,
            Duration::from_millis(300),
            Duration::from_millis(200),
        );
        let elapsed = started.elapsed();
        assert_eq!(got, Appearance::default());
        let limits = hanging.limits.borrow();
        assert!(!limits.is_empty());
        assert!(limits.iter().all(|l| *l <= Duration::from_millis(200)));
        let granted: Duration = limits.iter().sum();
        assert!(
            granted <= Duration::from_millis(300),
            "granted {granted:?} over {limits:?}"
        );
        assert!(elapsed < Duration::from_millis(900), "took {elapsed:?}");
    }

    #[test]
    fn the_real_budget_is_at_most_a_second_and_a_half() {
        assert!(OVERALL_BUDGET <= Duration::from_millis(1500));
        assert!(PER_CALL_LIMIT <= OVERALL_BUDGET);
    }

    fn shell(program: &str, args: &[&str]) -> Command {
        let mut command = Command::new(program);
        command.args(args);
        command
    }

    #[test]
    fn a_program_that_hangs_is_killed_at_the_deadline() {
        let start = Instant::now();
        let out = run_with_deadline(shell("sleep", &["30"]), Duration::from_millis(200));
        assert_eq!(out, None);
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn output_of_a_successful_program_is_returned() {
        let out = run_with_deadline(shell("echo", &["hello"]), Duration::from_secs(5));
        assert_eq!(out.as_deref(), Some("hello\n"));
    }

    #[test]
    fn failure_and_missing_programs_give_none() {
        assert_eq!(
            run_with_deadline(shell("false", &[]), Duration::from_secs(5)),
            None
        );
        assert_eq!(
            run_with_deadline(
                shell("bytescape-no-such-program", &[]),
                Duration::from_secs(5)
            ),
            None
        );
    }

    #[test]
    fn a_pinned_bus_address_reaches_the_child() {
        let processes = Processes {
            bus: Bus::Pinned(OsString::from("unix:path=/run/user/1/bus")),
        };
        let out = processes.run(
            "sh",
            &["-c", "printf %s \"$DBUS_SESSION_BUS_ADDRESS\""],
            Duration::from_secs(5),
        );
        assert_eq!(out.as_deref(), Some("unix:path=/run/user/1/bus"));
    }
}
