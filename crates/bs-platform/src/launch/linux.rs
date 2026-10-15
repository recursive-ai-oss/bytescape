//! Handoffs on Linux. See the module table in [`super`] for every program
//! started here.

use super::LaunchError;
use super::plan::{self, FileManagerCall};
use super::spawn::{Spec, find_on_path};
use crate::appearance::{Bus, session_bus};
use std::ffi::{OsStr, OsString};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;

/// How long one file-manager call may take. The file manager may have to start.
const CALL_LIMIT: Duration = Duration::from_secs(8);
/// The same limit, told to `gdbus` so it gives up first and says why.
const GDBUS_TIMEOUT_SECONDS: u32 = 6;
/// How long a quick question to a tool may take.
const QUERY_LIMIT: Duration = Duration::from_secs(3);

/// What the handoffs read from the session.
pub(crate) struct Env {
    /// `PATH`.
    pub path: OsString,
    /// `DBUS_SESSION_BUS_ADDRESS`.
    pub bus_address: Option<OsString>,
    /// `XDG_RUNTIME_DIR`.
    pub runtime_dir: Option<OsString>,
    /// `XDG_CURRENT_DESKTOP`.
    pub desktop: String,
    /// Whether the user's bus socket exists.
    pub socket_exists: fn(&Path) -> bool,
    /// Whether `WAYLAND_DISPLAY` names a Wayland session.
    pub wayland: bool,
}

impl Env {
    pub fn current() -> Self {
        Self {
            path: std::env::var_os("PATH").unwrap_or_default(),
            bus_address: std::env::var_os("DBUS_SESSION_BUS_ADDRESS"),
            runtime_dir: std::env::var_os("XDG_RUNTIME_DIR"),
            desktop: std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default(),
            socket_exists: |path| path.exists(),
            wayland: std::env::var_os("WAYLAND_DISPLAY").is_some_and(|v| !v.is_empty()),
        }
    }
}

/// How a program relates to the session bus.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum BusUse {
    /// The program does not use D-Bus.
    No,
    /// The program may use D-Bus. It is refused only if the environment names
    /// a bus that is not a local socket.
    Maybe,
    /// The program is a D-Bus client. It needs a local bus, found in the
    /// environment or at the user's runtime socket.
    Always,
}

/// Finds a program and prepares to start it, applying the session-bus rule.
pub(crate) fn tool(env: &Env, name: &'static str, bus: BusUse) -> Result<Spec, LaunchError> {
    let program = find_on_path(name, &env.path).ok_or(LaunchError::ProgramMissing(name))?;
    let spec = Spec::new(program)?;
    let address = env.bus_address.as_deref().filter(|a| !a.is_empty());
    match bus {
        BusUse::No => Ok(spec),
        BusUse::Maybe => match address {
            None => Ok(spec),
            Some(address) => match session_bus(Some(address), None, &|_| false) {
                Some(_) => Ok(spec),
                None => Err(LaunchError::SessionBusNotLocal),
            },
        },
        BusUse::Always => {
            match session_bus(address, env.runtime_dir.as_deref(), &|p| {
                (env.socket_exists)(p)
            }) {
                Some(Bus::Inherited) => Ok(spec),
                Some(Bus::Pinned(address)) => Ok(spec.pin_bus(address)),
                None => Err(LaunchError::SessionBusNotLocal),
            }
        }
    }
}

/// Runs a tool and requires it to exit successfully.
fn succeeds(spec: &Spec, step: &'static str) -> Result<(), LaunchError> {
    let output = spec.run(step, Some(CALL_LIMIT))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(LaunchError::Failed {
            step,
            kind: io::ErrorKind::Other,
            code: output.status.code(),
        })
    }
}

/// The path must be absolute and name something that exists.
fn existing(path: &Path) -> Result<(), LaunchError> {
    if !path.is_absolute() {
        return Err(LaunchError::BadInput("the path must be absolute"));
    }
    std::fs::symlink_metadata(path)
        .map(|_| ())
        .map_err(|_| LaunchError::BadInput("the path does not exist"))
}

/// Asks the file manager to act on one item, through `gdbus`, else `dbus-send`.
fn file_manager(env: &Env, call: FileManagerCall, path: &Path) -> Result<(), LaunchError> {
    let uri = plan::file_uri(path.as_os_str().as_bytes())?;
    let gdbus = tool(env, "gdbus", BusUse::Always).and_then(|spec| {
        succeeds(
            &spec.args(plan::gdbus_call_args(call, &uri, GDBUS_TIMEOUT_SECONDS)),
            "ask the file manager through gdbus",
        )
    });
    let error = match gdbus {
        Ok(()) => return Ok(()),
        // Another tool would be refused for the same reason.
        Err(LaunchError::SessionBusNotLocal) => return Err(LaunchError::SessionBusNotLocal),
        Err(error) => error,
    };
    let dbus_send = tool(env, "dbus-send", BusUse::Always).and_then(|spec| {
        succeeds(
            &spec.args(plan::dbus_send_call_args(call, &uri)),
            "ask the file manager through dbus-send",
        )
    });
    // With neither tool installed, say that `gdbus` is the one to install.
    match dbus_send {
        Ok(()) => Ok(()),
        Err(LaunchError::ProgramMissing(_)) => Err(error),
        Err(other) => Err(other),
    }
}

pub(crate) fn reveal(env: &Env, path: &Path) -> Result<(), LaunchError> {
    existing(path)?;
    match file_manager(env, FileManagerCall::ShowItems, path) {
        Ok(()) => Ok(()),
        // No file manager answered, or there is no bus to ask on: open the
        // folder that holds the item. The opener refuses a bus that is not a
        // local socket itself.
        Err(_) => {
            let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
            open_path(env, parent.unwrap_or(path))
        }
    }
}

pub(crate) fn properties(env: &Env, path: &Path) -> Result<(), LaunchError> {
    existing(path)?;
    file_manager(env, FileManagerCall::ShowItemProperties, path)
}

/// Whether the file manager has `ShowItemProperties`. The answer is read once
/// and kept: asking can start the file manager.
pub(crate) fn properties_supported() -> bool {
    static ANSWER: OnceLock<bool> = OnceLock::new();
    *ANSWER.get_or_init(|| supports_properties(&Env::current()))
}

pub(crate) fn supports_properties(env: &Env) -> bool {
    let mentions = |spec: Result<Spec, LaunchError>, args: Vec<OsString>| {
        spec.and_then(|spec| {
            spec.args(args)
                .run("ask the file manager", Some(CALL_LIMIT))
        })
        .is_ok_and(|output| {
            output.status.success()
                && String::from_utf8_lossy(&output.stdout).contains("ShowItemProperties")
        })
    };
    mentions(
        tool(env, "gdbus", BusUse::Always),
        plan::gdbus_introspect_args(GDBUS_TIMEOUT_SECONDS),
    ) || mentions(
        tool(env, "dbus-send", BusUse::Always),
        plan::dbus_send_introspect_args(),
    )
}

pub(crate) fn open(env: &Env, path: &Path) -> Result<(), LaunchError> {
    existing(path)?;
    open_path(env, path)
}

/// Hands an existing absolute path to `xdg-open`. The path starts with `/`, so
/// it cannot be read as an option.
fn open_path(env: &Env, path: &Path) -> Result<(), LaunchError> {
    tool(env, "xdg-open", BusUse::Maybe)?
        .arg(path.as_os_str())
        .detach("start xdg-open")
}

/// Hands an address that was checked by the caller to `xdg-open`.
pub(crate) fn open_link(env: &Env, url: &str) -> Result<(), LaunchError> {
    tool(env, "xdg-open", BusUse::Maybe)?
        .arg(url)
        .detach("start xdg-open")
}

pub(crate) fn open_terminal(env: &Env, dir: &Path) -> Result<(), LaunchError> {
    if !dir.is_absolute() {
        return Err(LaunchError::BadInput("the folder must be absolute"));
    }
    if !std::fs::metadata(dir).is_ok_and(|m| m.is_dir()) {
        return Err(LaunchError::BadInput("the folder does not exist"));
    }
    let mut last = LaunchError::ProgramMissing("a terminal");
    for terminal in plan::terminal_order(&env.desktop) {
        let spec = match tool(env, terminal.program(), BusUse::Maybe) {
            Ok(spec) => spec,
            Err(LaunchError::ProgramMissing(_)) => continue,
            Err(other) => return Err(other),
        };
        let spec = spec
            .args(plan::terminal_args(terminal, dir.as_os_str()))
            .cwd(dir.to_path_buf());
        match spec.detach("start the terminal") {
            Ok(()) => return Ok(()),
            // A terminal that is installed but broken: try the next one.
            Err(error) => last = error,
        }
    }
    Err(last)
}

pub(crate) fn software_center_available(env: &Env) -> bool {
    let Ok(spec) = tool(env, "xdg-mime", BusUse::Maybe) else {
        return false;
    };
    let ask = spec
        .args(
            ["query", "default", "x-scheme-handler/appstream"]
                .map(OsStr::new)
                .map(OsString::from),
        )
        .run("ask for the appstream handler", Some(QUERY_LIMIT));
    ask.is_ok_and(|output| {
        output.status.success() && !String::from_utf8_lossy(&output.stdout).trim().is_empty()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::launch::spawn::testing::{recorder, script};
    use crate::testutil::Fixture;
    use std::path::PathBuf;
    use std::time::Instant;

    struct Lab {
        fx: Fixture,
    }

    impl Lab {
        fn new(label: &str) -> Self {
            let fx = Fixture::new(label);
            fx.mkdir("bin");
            fx.mkdir("work");
            Self { fx }
        }

        fn bin(&self) -> PathBuf {
            self.fx.path().join("bin")
        }

        fn out(&self, name: &str) -> PathBuf {
            self.fx.path().join(name)
        }

        /// An environment whose `PATH` holds only the fake programs, with a
        /// local session bus.
        fn env(&self) -> Env {
            Env {
                path: self.bin().into_os_string(),
                bus_address: Some(OsString::from("unix:path=/run/user/1000/bus")),
                runtime_dir: Some(OsString::from("/run/user/1000")),
                desktop: String::new(),
                socket_exists: |_| true,
                wayland: false,
            }
        }

        fn item(&self) -> PathBuf {
            self.fx.write("work/it's a file, [1].txt", "x");
            self.fx.path().join("work/it's a file, [1].txt")
        }
    }

    fn lines(path: &Path) -> Vec<String> {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn wait_for(path: &Path) {
        let started = Instant::now();
        while !path.exists() && started.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(10));
        }
        // Let the script finish writing.
        std::thread::sleep(Duration::from_millis(100));
        assert!(path.exists(), "the program never ran");
    }

    #[test]
    fn reveal_asks_the_file_manager_through_gdbus() {
        let lab = Lab::new("reveal-gdbus");
        let gdbus = lab.out("gdbus.txt");
        recorder(&lab.bin(), "gdbus", &gdbus, 0);
        let item = lab.item();
        reveal(&lab.env(), &item).unwrap();
        let got = lines(&gdbus);
        assert_eq!(got[0], "call");
        assert_eq!(got[9], "org.freedesktop.FileManager1.ShowItems");
        let uri = format!(
            "file://{}/work/it%27s%20a%20file%2C%20%5B1%5D.txt",
            lab.fx.path().display()
        );
        assert_eq!(got[10], format!("['{uri}']"));
        assert_eq!(got[11], "");
    }

    #[test]
    fn reveal_falls_back_to_dbus_send_then_to_the_parent_folder() {
        let lab = Lab::new("reveal-fallback");
        let gdbus = lab.out("gdbus.txt");
        let dbus_send = lab.out("dbus-send.txt");
        let xdg = lab.out("xdg.txt");
        recorder(&lab.bin(), "gdbus", &gdbus, 1);
        recorder(&lab.bin(), "dbus-send", &dbus_send, 0);
        recorder(&lab.bin(), "xdg-open", &xdg, 0);
        let item = lab.item();

        reveal(&lab.env(), &item).unwrap();
        assert!(gdbus.exists());
        let got = lines(&dbus_send);
        assert_eq!(got[5], "org.freedesktop.FileManager1.ShowItems");
        assert!(got[6].starts_with("array:string:file://"));
        assert!(
            !xdg.exists(),
            "the folder is opened only when the file manager fails"
        );

        // Neither D-Bus tool works: the folder that holds the item opens.
        recorder(&lab.bin(), "dbus-send", &dbus_send, 1);
        reveal(&lab.env(), &item).unwrap();
        wait_for(&xdg);
        assert_eq!(lines(&xdg)[0], format!("{}/work", lab.fx.path().display()));
    }

    #[test]
    fn reveal_with_no_tool_at_all_is_an_error() {
        let lab = Lab::new("reveal-none");
        let item = lab.item();
        assert!(matches!(
            reveal(&lab.env(), &item),
            Err(LaunchError::ProgramMissing(_))
        ));
    }

    #[test]
    fn a_path_that_is_not_absolute_or_not_there_is_refused_before_anything_starts() {
        let lab = Lab::new("reveal-bad");
        let ran = lab.out("ran.txt");
        for name in ["gdbus", "dbus-send", "xdg-open"] {
            recorder(&lab.bin(), name, &ran, 0);
        }
        let env = lab.env();
        for bad in [
            PathBuf::from("relative/file"),
            PathBuf::from("-rf"),
            lab.fx.path().join("work/missing"),
        ] {
            assert!(
                matches!(reveal(&env, &bad), Err(LaunchError::BadInput(_))),
                "{bad:?}"
            );
            assert!(matches!(
                properties(&env, &bad),
                Err(LaunchError::BadInput(_))
            ));
            assert!(matches!(open(&env, &bad), Err(LaunchError::BadInput(_))));
        }
        std::thread::sleep(Duration::from_millis(100));
        assert!(!ran.exists());
    }

    #[test]
    fn nothing_starts_when_the_session_bus_is_not_a_local_socket() {
        let lab = Lab::new("bus-tcp");
        let ran = lab.out("ran.txt");
        for name in ["gdbus", "dbus-send", "xdg-open", "konsole", "xdg-mime"] {
            recorder(&lab.bin(), name, &ran, 0);
        }
        let item = lab.item();
        let mut env = lab.env();
        for address in [
            "tcp:host=example.org,port=1234",
            "unix:path=/x;tcp:host=h,port=1",
            "nonce-tcp:host=h",
        ] {
            env.bus_address = Some(OsString::from(address));
            assert!(
                matches!(reveal(&env, &item), Err(LaunchError::SessionBusNotLocal)),
                "{address}"
            );
            assert!(matches!(
                properties(&env, &item),
                Err(LaunchError::SessionBusNotLocal)
            ));
            assert!(matches!(
                open(&env, &item),
                Err(LaunchError::SessionBusNotLocal)
            ));
            assert!(matches!(
                open_link(&env, "https://recursiveai.org/"),
                Err(LaunchError::SessionBusNotLocal)
            ));
            assert!(matches!(
                open_terminal(&env, &lab.fx.path().join("work")),
                Err(LaunchError::SessionBusNotLocal)
            ));
            assert!(!supports_properties(&env));
            assert!(!software_center_available(&env));
        }
        std::thread::sleep(Duration::from_millis(200));
        assert!(
            !ran.exists(),
            "a program was started on a bus that is not local"
        );
    }

    #[test]
    fn a_d_bus_client_needs_a_bus_but_the_opener_does_not() {
        let lab = Lab::new("bus-absent");
        let gdbus = lab.out("gdbus.txt");
        let xdg = lab.out("xdg.txt");
        recorder(&lab.bin(), "gdbus", &gdbus, 0);
        recorder(&lab.bin(), "xdg-open", &xdg, 0);
        let item = lab.item();
        let mut env = lab.env();
        env.bus_address = None;
        env.socket_exists = |_| false;
        // No address and no socket: `gdbus` is not started; the opener still
        // can be, since it can run without a bus.
        reveal(&env, &item).unwrap();
        wait_for(&xdg);
        assert!(!gdbus.exists());
        // An empty address counts as unset.
        env.bus_address = Some(OsString::new());
        open_link(&env, "https://recursiveai.org/").unwrap();
    }

    #[test]
    fn the_runtime_socket_is_named_explicitly_when_there_is_no_address() {
        let lab = Lab::new("bus-pinned");
        let seen = lab.out("seen.txt");
        script(
            &lab.bin(),
            "gdbus",
            &format!(
                "printf %s \"$DBUS_SESSION_BUS_ADDRESS\" > '{}'",
                seen.display()
            ),
        );
        let item = lab.item();
        let mut env = lab.env();
        env.bus_address = None;
        reveal(&env, &item).unwrap();
        assert_eq!(
            std::fs::read_to_string(&seen).unwrap(),
            "unix:path=/run/user/1000/bus"
        );
    }

    #[test]
    fn properties_use_the_properties_method_and_have_no_fallback_folder() {
        let lab = Lab::new("properties");
        let gdbus = lab.out("gdbus.txt");
        let xdg = lab.out("xdg.txt");
        recorder(&lab.bin(), "gdbus", &gdbus, 0);
        recorder(&lab.bin(), "xdg-open", &xdg, 0);
        let item = lab.item();
        properties(&lab.env(), &item).unwrap();
        assert_eq!(
            lines(&gdbus)[9],
            "org.freedesktop.FileManager1.ShowItemProperties"
        );
        recorder(&lab.bin(), "gdbus", &gdbus, 1);
        assert!(properties(&lab.env(), &item).is_err());
        std::thread::sleep(Duration::from_millis(100));
        assert!(!xdg.exists());
    }

    #[test]
    fn properties_are_supported_only_when_the_file_manager_lists_them() {
        let lab = Lab::new("properties-support");
        let env = lab.env();
        assert!(!supports_properties(&env), "no tool, no support");

        script(
            &lab.bin(),
            "gdbus",
            "echo 'interface org.freedesktop.FileManager1 { methods: ShowItems(); };'",
        );
        assert!(!supports_properties(&env));

        script(
            &lab.bin(),
            "gdbus",
            "echo 'ShowItems(); ShowFolders(); ShowItemProperties();'",
        );
        assert!(supports_properties(&env));

        // A failing gdbus is not enough; dbus-send is asked next.
        script(&lab.bin(), "gdbus", "exit 1");
        assert!(!supports_properties(&env));
        script(
            &lab.bin(),
            "dbus-send",
            "echo 'method return; ShowItemProperties'",
        );
        assert!(supports_properties(&env));
    }

    #[test]
    fn open_gives_the_path_to_the_opener_as_one_argument() {
        let lab = Lab::new("open");
        let xdg = lab.out("xdg.txt");
        recorder(&lab.bin(), "xdg-open", &xdg, 0);
        lab.fx.write("work/--version", "x");
        let dashed = lab.fx.path().join("work/--version");
        open(&lab.env(), &dashed).unwrap();
        wait_for(&xdg);
        // The argument is the whole absolute path, so it starts with `/`.
        assert_eq!(lines(&xdg)[0], dashed.display().to_string());
    }

    #[test]
    fn links_open_through_the_opener() {
        let lab = Lab::new("link");
        let xdg = lab.out("xdg.txt");
        recorder(&lab.bin(), "xdg-open", &xdg, 0);
        open_link(&lab.env(), "https://recursiveai.org/?a=b&c=d").unwrap();
        wait_for(&xdg);
        assert_eq!(lines(&xdg)[0], "https://recursiveai.org/?a=b&c=d");
    }

    #[test]
    fn the_terminal_follows_the_desktop_and_opens_in_the_folder() {
        let lab = Lab::new("terminal");
        let konsole = lab.out("konsole.txt");
        let gnome = lab.out("gnome.txt");
        let generic = lab.out("generic.txt");
        recorder(&lab.bin(), "konsole", &konsole, 0);
        recorder(&lab.bin(), "gnome-terminal", &gnome, 0);
        recorder(&lab.bin(), "x-terminal-emulator", &generic, 0);
        lab.fx.mkdir("work/my dir; $(x)");
        let dir = lab.fx.path().join("work/my dir; $(x)");
        let mut env = lab.env();

        env.desktop = "KDE".into();
        open_terminal(&env, &dir).unwrap();
        wait_for(&konsole);
        let got = lines(&konsole);
        assert_eq!(
            got[..2],
            ["--workdir".to_string(), dir.display().to_string()]
        );
        assert_eq!(got[2], format!("cwd={}", dir.display()));
        assert!(!gnome.exists() && !generic.exists());

        std::fs::remove_file(lab.bin().join("konsole")).unwrap();
        open_terminal(&env, &dir).unwrap();
        wait_for(&generic);
        assert_eq!(lines(&generic)[0], format!("cwd={}", dir.display()));

        std::fs::remove_file(lab.bin().join("x-terminal-emulator")).unwrap();
        env.desktop = "GNOME".into();
        open_terminal(&env, &dir).unwrap();
        wait_for(&gnome);
        assert_eq!(
            lines(&gnome)[0],
            format!("--working-directory={}", dir.display())
        );
    }

    #[test]
    fn the_terminal_needs_a_real_absolute_folder_and_some_terminal() {
        let lab = Lab::new("terminal-bad");
        let item = lab.item();
        let env = lab.env();
        assert!(matches!(
            open_terminal(&env, Path::new("relative")),
            Err(LaunchError::BadInput(_))
        ));
        assert!(matches!(
            open_terminal(&env, &lab.fx.path().join("missing")),
            Err(LaunchError::BadInput(_))
        ));
        // A file is not a folder.
        assert!(matches!(
            open_terminal(&env, &item),
            Err(LaunchError::BadInput(_))
        ));
        assert!(matches!(
            open_terminal(&env, &lab.fx.path().join("work")),
            Err(LaunchError::ProgramMissing(_))
        ));
    }

    #[test]
    fn software_center_links_need_a_handler() {
        let lab = Lab::new("software-center");
        let env = lab.env();
        assert!(!software_center_available(&env));
        script(&lab.bin(), "xdg-mime", "exit 0");
        assert!(!software_center_available(&env));
        script(&lab.bin(), "xdg-mime", "echo org.kde.discover.desktop");
        assert!(software_center_available(&env));
        script(
            &lab.bin(),
            "xdg-mime",
            "echo org.kde.discover.desktop; exit 4",
        );
        assert!(!software_center_available(&env));
    }
}
