//! Light/dark, accent and reduced motion on Linux, from the desktop portal,
//! GNOME settings and KDE's `kdeglobals` file.

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
/// Only this much of `kdeglobals` is read. Real files are a few kilobytes;
/// the entries wanted are near the top or in small groups.
const MAX_CONFIG_BYTES: u64 = 256 * 1024;

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

/// Where KDE keeps its settings: `$XDG_CONFIG_HOME/kdeglobals`, or
/// `~/.config/kdeglobals`. Relative locations are ignored, as the XDG
/// specification says.
pub(crate) fn kdeglobals_path(
    config_home: Option<&OsStr>,
    home: Option<&OsStr>,
) -> Option<std::path::PathBuf> {
    let absolute = |value: Option<&OsStr>| {
        value
            .map(Path::new)
            .filter(|p| p.is_absolute())
            .map(Path::to_path_buf)
    };
    let base = absolute(config_home).or_else(|| absolute(home).map(|h| h.join(".config")))?;
    Some(base.join("kdeglobals"))
}

/// The text at the start of a settings file, if it is a regular file.
fn read_config(path: &Path) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(MAX_CONFIG_BYTES).read_to_end(&mut bytes).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// Reads the appearance from the real system.
pub(crate) fn read_system() -> Appearance {
    let address = std::env::var_os("DBUS_SESSION_BUS_ADDRESS");
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR");
    let bus = session_bus(address.as_deref(), runtime_dir.as_deref(), &|p| p.exists());
    let config = kdeglobals_path(
        std::env::var_os("XDG_CONFIG_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    )
    .and_then(|path| read_config(&path));
    match bus {
        Some(bus) => read(
            &Processes { bus },
            true,
            OVERALL_BUDGET,
            PER_CALL_LIMIT,
            config.as_deref(),
        ),
        None => read(
            &Processes {
                bus: Bus::Inherited,
            },
            false,
            OVERALL_BUDGET,
            PER_CALL_LIMIT,
            config.as_deref(),
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

/// Reads the setting. With `bus_allowed` false no program is started, and only
/// `kdeglobals` (the text of KDE's settings file, if there is one) can say
/// anything. All the runs together take at most `budget`, and each at most
/// `per_call`.
pub(crate) fn read(
    runner: &dyn Runner,
    bus_allowed: bool,
    budget: Duration,
    per_call: Duration,
    kdeglobals: Option<&str>,
) -> Appearance {
    let kde = kdeglobals.map(parse_kdeglobals).unwrap_or_default();
    if !bus_allowed {
        return Appearance {
            scheme: ColorScheme::Light,
            accent: kde.accent,
            reduce_motion: kde.reduces_motion(),
        };
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
    let gsetting = |key: &str| -> Option<String> {
        budget.run("gsettings", &["get", "org.gnome.desktop.interface", key])
    };

    let scheme_reply = portal("color-scheme");
    // A portal that cannot say light or dark will not know the newer keys
    // either, so they are not asked for.
    let portal_answers = scheme_reply.is_some();
    let scheme = match scheme_reply.as_deref().and_then(parse_portal_color_scheme) {
        Some(PortalScheme::Dark) => ColorScheme::Dark,
        Some(PortalScheme::Light) => ColorScheme::Light,
        // No preference, or no portal: ask GNOME's settings.
        Some(PortalScheme::NoPreference) | None => gsetting("color-scheme")
            .as_deref()
            .and_then(parse_gsettings_color_scheme)
            .unwrap_or(ColorScheme::Light),
    };

    let accent = portal_answers
        .then(|| portal("accent-color"))
        .flatten()
        .as_deref()
        .and_then(parse_portal_accent)
        .or(kde.accent)
        .or_else(|| {
            gsetting("accent-color")
                .as_deref()
                .and_then(parse_gsettings_accent)
        });

    // Any source that asks for less motion wins; see the module notes.
    let portal_motion = portal_answers
        .then(|| portal("reduced-motion"))
        .flatten()
        .as_deref()
        .and_then(parse_portal_reduced_motion);
    // GNOME's setting is asked only when the portal does not know the key.
    let reduce_motion = portal_motion == Some(true)
        || kde.reduces_motion()
        || (portal_motion.is_none()
            && gsetting("enable-animations")
                .as_deref()
                .and_then(parse_gsettings_bool)
                == Some(false));
    Appearance {
        scheme,
        accent,
        reduce_motion,
    }
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

/// Parses the reply to a portal read of `reduced-motion`, such as
/// `(<uint32 1>,)`. 0 is no preference and 1 asks for reduced motion. Any
/// other value is not understood.
pub(crate) fn parse_portal_reduced_motion(output: &str) -> Option<bool> {
    let after = output.split_once("uint32")?.1;
    let digits: String = after
        .trim_start()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    match digits.as_str() {
        "0" => Some(false),
        "1" => Some(true),
        _ => None,
    }
}

/// Parses `gsettings get ... enable-animations`, which prints `true` or
/// `false`.
pub(crate) fn parse_gsettings_bool(output: &str) -> Option<bool> {
    match output.trim() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

/// Parses `gsettings get ... accent-color`, which prints the name of one of
/// GNOME's nine accent colors, such as `'blue'`. The values are the ones
/// libadwaita draws those names with.
pub(crate) fn parse_gsettings_accent(output: &str) -> Option<Rgb> {
    let hex: u32 = match output.trim().trim_matches('\'') {
        "blue" => 0x3584e4,
        "teal" => 0x2190a4,
        "green" => 0x3a944a,
        "yellow" => 0xc88800,
        "orange" => 0xed5b00,
        "red" => 0xe62d42,
        "pink" => 0xd56199,
        "purple" => 0x9141ac,
        "slate" => 0x6f8396,
        _ => return None,
    };
    Some(Rgb {
        r: (hex >> 16) as u8,
        g: (hex >> 8) as u8,
        b: hex as u8,
    })
}

/// What `kdeglobals` says that matters here.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct KdeSettings {
    /// `AccentColor` in `[General]`.
    pub accent: Option<Rgb>,
    /// `AnimationDurationFactor` in `[KDE]`: 1 is normal speed and 0 is no
    /// animation at all.
    pub animation_factor: Option<f64>,
}

impl KdeSettings {
    pub(crate) fn reduces_motion(&self) -> bool {
        self.animation_factor.is_some_and(|factor| factor <= 0.0)
    }
}

/// Reads the two entries out of the text of `kdeglobals`, an INI file. Later
/// lines win. An entry that cannot be understood is ignored.
pub(crate) fn parse_kdeglobals(text: &str) -> KdeSettings {
    let mut settings = KdeSettings::default();
    let mut group = "";
    for line in text.lines() {
        let line = line.trim();
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            group = name;
        } else if let Some((key, value)) = line.split_once('=') {
            let (key, value) = (key.trim(), value.trim());
            match (group, key) {
                ("General", "AccentColor") => settings.accent = parse_kde_color(value),
                ("KDE", "AnimationDurationFactor") => {
                    settings.animation_factor = value.parse::<f64>().ok().filter(|f| f.is_finite());
                }
                _ => {}
            }
        }
    }
    settings
}

/// A KDE color entry: `r,g,b` or `r,g,b,a` with channels 0 to 255, or
/// `#rrggbb`.
fn parse_kde_color(value: &str) -> Option<Rgb> {
    if let Some(hex) = value.strip_prefix('#') {
        if hex.len() != 6 || !hex.is_ascii() {
            return None;
        }
        let channel = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
        return Some(Rgb {
            r: channel(0)?,
            g: channel(2)?,
            b: channel(4)?,
        });
    }
    let mut numbers = value.split(',').map(|part| part.trim().parse::<u8>().ok());
    let (r, g, b) = (numbers.next()??, numbers.next()??, numbers.next()??);
    // An optional alpha, and nothing more.
    if let Some(alpha) = numbers.next() {
        alpha?;
    }
    if numbers.next().is_some() {
        return None;
    }
    Some(Rgb { r, g, b })
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
    use crate::appearance::DEFAULT_ACCENT;
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
    #[derive(Default)]
    struct Script {
        portal_scheme: Option<&'static str>,
        portal_accent: Option<&'static str>,
        portal_motion: Option<&'static str>,
        gsettings: Option<&'static str>,
        gsettings_accent: Option<&'static str>,
        gsettings_animations: Option<&'static str>,
        /// Whether the old `Read` method works while `ReadOne` fails.
        only_old_read: bool,
        asked: RefCell<Vec<String>>,
    }

    impl Script {
        fn new() -> Self {
            Self::default()
        }

        fn asked_any(&self, prefix: &str) -> bool {
            self.asked.borrow().iter().any(|a| a.starts_with(prefix))
        }

        fn asked_portal_for(&self, key: &str) -> bool {
            self.asked
                .borrow()
                .iter()
                .any(|a| a.starts_with("gdbus") && a.ends_with(key))
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
                        "reduced-motion" => self.portal_motion.map(str::to_owned),
                        _ => None,
                    }
                }
                "gsettings" => match *args.last()? {
                    "color-scheme" => self.gsettings.map(str::to_owned),
                    "accent-color" => self.gsettings_accent.map(str::to_owned),
                    "enable-animations" => self.gsettings_animations.map(str::to_owned),
                    _ => None,
                },
                _ => None,
            }
        }
    }

    fn read_with(script: &Script, bus_allowed: bool) -> Appearance {
        read(script, bus_allowed, OVERALL_BUDGET, PER_CALL_LIMIT, None)
    }

    fn read_with_kde(script: &Script, bus_allowed: bool, kdeglobals: &str) -> Appearance {
        read(
            script,
            bus_allowed,
            OVERALL_BUDGET,
            PER_CALL_LIMIT,
            Some(kdeglobals),
        )
    }

    fn rgb(r: u8, g: u8, b: u8) -> Rgb {
        Rgb { r, g, b }
    }

    #[test]
    fn portal_answer_wins_over_gsettings() {
        let mut script = Script::new();
        script.portal_scheme = Some("(<uint32 1>,)");
        script.portal_accent = Some("(<(0.0, 1.0, 0.0)>,)");
        script.portal_motion = Some("(<uint32 0>,)");
        script.gsettings = Some("'prefer-light'");
        script.gsettings_accent = Some("'red'");
        script.gsettings_animations = Some("false");
        let got = read_with(&script, true);
        assert_eq!(got.scheme, ColorScheme::Dark);
        assert_eq!(got.accent, Some(rgb(0, 255, 0)));
        assert!(!got.reduce_motion);
        // A portal that answers every question leaves nothing for GNOME to add.
        assert!(!script.asked_any("gsettings"), "{:?}", script.asked);
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
        assert_eq!(read_with(&nothing, true), Appearance::default());
    }

    #[test]
    fn without_a_permitted_bus_no_program_is_started() {
        let mut script = Script::new();
        script.portal_scheme = Some("(<uint32 1>,)");
        script.portal_accent = Some("(<(0.0, 1.0, 0.0)>,)");
        script.portal_motion = Some("(<uint32 1>,)");
        script.gsettings = Some("'prefer-dark'");
        script.gsettings_animations = Some("false");
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
        script.portal_motion = Some("(<uint32 7>,)");
        let got = read_with(&script, true);
        assert_eq!(got, Appearance::default());
    }

    #[test]
    fn a_portal_that_cannot_say_light_or_dark_is_not_asked_for_more() {
        let script = Script::new();
        let _ = read_with(&script, true);
        assert!(script.asked_portal_for("color-scheme"));
        assert!(
            !script.asked_portal_for("accent-color"),
            "{:?}",
            script.asked
        );
        assert!(
            !script.asked_portal_for("reduced-motion"),
            "{:?}",
            script.asked
        );
    }

    #[test]
    fn portal_accent_replies_come_first_and_then_kde_and_gnome() {
        let kde = "[General]\nAccentColor=61,174,233\n";
        let mut script = Script::new();
        script.portal_scheme = Some("(<uint32 2>,)");
        script.portal_accent = Some("(<(0.0, 1.0, 0.0)>,)");
        script.gsettings_accent = Some("'red'");
        assert_eq!(
            read_with_kde(&script, true, kde).accent,
            Some(rgb(0, 255, 0))
        );
        // The portal says "no accent": the desktop's own settings are consulted.
        script.portal_accent = Some("(<(-1.0, -1.0, -1.0)>,)");
        assert_eq!(
            read_with_kde(&script, true, kde).accent,
            Some(rgb(61, 174, 233))
        );
        assert_eq!(read_with(&script, true).accent, Some(rgb(0xe6, 0x2d, 0x42)));
        // Nobody knows: no system accent, and the interface uses its own.
        script.gsettings_accent = Some("'unknown'");
        assert_eq!(read_with(&script, true).accent, None);
        assert_eq!(read_with(&script, true).accent_or_default(), DEFAULT_ACCENT);
    }

    #[test]
    fn kde_accent_works_without_a_session_bus_and_starts_nothing() {
        let script = Script::new();
        let got = read_with_kde(&script, false, "[General]\nAccentColor=1,2,3\n");
        assert_eq!(got.accent, Some(rgb(1, 2, 3)));
        assert_eq!(got.scheme, ColorScheme::Light);
        assert!(script.asked.borrow().is_empty());
    }

    #[test]
    fn reduced_motion_from_the_portal() {
        let mut script = Script::new();
        script.portal_scheme = Some("(<uint32 2>,)");
        script.portal_motion = Some("(<uint32 1>,)");
        assert!(read_with(&script, true).reduce_motion);
        // The portal said so; GNOME's animation setting is not needed.
        assert!(!script.asked_any("gsettings get org.gnome.desktop.interface enable-animations"));
        script.portal_motion = Some("(<<uint32 1>>,)");
        assert!(read_with(&script, true).reduce_motion);
        // A portal that knows the key and says "no preference" is believed.
        script.portal_motion = Some("(<uint32 0>,)");
        script.gsettings_animations = Some("false");
        assert!(!read_with(&script, true).reduce_motion);
    }

    #[test]
    fn reduced_motion_from_gnome_when_the_portal_lacks_the_key() {
        let mut script = Script::new();
        script.portal_scheme = Some("(<uint32 2>,)");
        script.gsettings_animations = Some("false\n");
        assert!(read_with(&script, true).reduce_motion);
        script.gsettings_animations = Some("true\n");
        assert!(!read_with(&script, true).reduce_motion);
        script.gsettings_animations = Some("maybe");
        assert!(!read_with(&script, true).reduce_motion);
        // No portal at all: still asked of GNOME.
        let mut bare = Script::new();
        bare.gsettings_animations = Some("false");
        assert!(read_with(&bare, true).reduce_motion);
    }

    #[test]
    fn reduced_motion_from_kde() {
        let script = Script::new();
        let off = "[KDE]\nAnimationDurationFactor=0\n";
        assert!(read_with_kde(&script, true, off).reduce_motion);
        assert!(read_with_kde(&script, false, off).reduce_motion);
        let slow = "[KDE]\nAnimationDurationFactor=0.5\n";
        assert!(!read_with_kde(&script, true, slow).reduce_motion);
        // The same entry in another group means nothing.
        let wrong = "[Other]\nAnimationDurationFactor=0\n";
        assert!(!read_with_kde(&script, true, wrong).reduce_motion);
    }

    #[test]
    fn portal_reply_forms_for_reduced_motion() {
        let cases = [
            ("(<uint32 1>,)", Some(true)),
            ("(<<uint32 1>>,)\n", Some(true)),
            ("(<uint32 0>,)", Some(false)),
            ("(<uint32 2>,)", None),
            ("(<uint32 10>,)", None),
            ("(<true>,)", None),
            ("", None),
        ];
        for (text, expected) in cases {
            assert_eq!(parse_portal_reduced_motion(text), expected, "{text:?}");
        }
    }

    #[test]
    fn gnome_setting_forms() {
        assert_eq!(parse_gsettings_bool("true\n"), Some(true));
        assert_eq!(parse_gsettings_bool("false"), Some(false));
        assert_eq!(parse_gsettings_bool("'false'"), None);
        assert_eq!(parse_gsettings_bool(""), None);
        // The nine accents libadwaita draws.
        let names = [
            ("blue", 0x3584e4_u32),
            ("teal", 0x2190a4),
            ("green", 0x3a944a),
            ("yellow", 0xc88800),
            ("orange", 0xed5b00),
            ("red", 0xe62d42),
            ("pink", 0xd56199),
            ("purple", 0x9141ac),
            ("slate", 0x6f8396),
        ];
        for (name, hex) in names {
            let want = rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8);
            assert_eq!(parse_gsettings_accent(&format!("'{name}'\n")), Some(want));
        }
        for bad in ["'Blue'", "'cyan'", "", "blue blue", "3584e4"] {
            assert_eq!(parse_gsettings_accent(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn kdeglobals_entries() {
        let text = "[ColorEffects:Disabled]\nAccentColor=9,9,9\n\
                    [General]\nColorScheme=BreezeDark\nAccentColor=61, 174, 233\n\
                    [KDE]\nAnimationDurationFactor=0.35\nAccentColor=7,7,7\n";
        let got = parse_kdeglobals(text);
        assert_eq!(
            got,
            KdeSettings {
                accent: Some(rgb(61, 174, 233)),
                animation_factor: Some(0.35),
            }
        );
        assert!(!got.reduces_motion());
        let cases: [(&str, Option<Rgb>); 9] = [
            ("1,2,3,255", Some(rgb(1, 2, 3))),
            ("#3daee9", Some(rgb(0x3d, 0xae, 0xe9))),
            ("#3DAEE9", Some(rgb(0x3d, 0xae, 0xe9))),
            ("256,0,0", None),
            ("1,2", None),
            ("1,2,3,4,5", None),
            ("#12345", None),
            ("#12345é", None),
            ("", None),
        ];
        for (value, expected) in cases {
            let got = parse_kdeglobals(&format!("[General]\nAccentColor={value}\n")).accent;
            assert_eq!(got, expected, "{value:?}");
        }
        // The last line wins, bad values are ignored, and so is binary junk.
        let later =
            parse_kdeglobals("[KDE]\nAnimationDurationFactor=1\nAnimationDurationFactor=0\n");
        assert!(later.reduces_motion());
        for junk in [
            "AnimationDurationFactor=nan",
            "AnimationDurationFactor=abc",
            "AnimationDurationFactor=",
        ] {
            let got = parse_kdeglobals(&format!("[KDE]\n{junk}\n"));
            assert_eq!(got.animation_factor, None, "{junk}");
            assert!(!got.reduces_motion());
        }
        assert_eq!(parse_kdeglobals("\u{0}\u{1}[[[==="), KdeSettings::default());
        assert_eq!(parse_kdeglobals(""), KdeSettings::default());
    }

    #[test]
    fn the_settings_file_is_found_by_the_xdg_rules() {
        let at = |c: Option<&str>, h: Option<&str>| {
            kdeglobals_path(c.map(OsStr::new), h.map(OsStr::new))
        };
        assert_eq!(
            at(Some("/cfg"), Some("/home/a")),
            Some(Path::new("/cfg/kdeglobals").to_path_buf())
        );
        assert_eq!(
            at(None, Some("/home/a")),
            Some(Path::new("/home/a/.config/kdeglobals").to_path_buf())
        );
        // A relative location is ignored.
        assert_eq!(
            at(Some("cfg"), Some("/home/a")),
            Some(Path::new("/home/a/.config/kdeglobals").to_path_buf())
        );
        assert_eq!(at(Some(""), Some("home")), None);
        assert_eq!(at(None, None), None);
    }

    #[test]
    fn a_settings_file_is_read_only_if_it_is_a_regular_file() {
        let fixture = crate::testutil::Fixture::new("kdeglobals");
        fixture.write("kdeglobals", "[KDE]\nAnimationDurationFactor=0\n");
        fixture.mkdir("dir");
        let text = read_config(&fixture.path().join("kdeglobals")).expect("read");
        assert!(parse_kdeglobals(&text).reduces_motion());
        assert_eq!(read_config(&fixture.path().join("dir")), None);
        assert_eq!(read_config(&fixture.path().join("missing")), None);
        // Only the start of a huge file is read.
        let big = "x".repeat(1 << 20);
        fixture.write("big", &big);
        let text = read_config(&fixture.path().join("big")).expect("read");
        assert_eq!(text.len() as u64, MAX_CONFIG_BYTES);
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
            None,
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
