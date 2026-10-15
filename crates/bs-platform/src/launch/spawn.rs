//! Starting a program with an argument list, an absolute path and nothing
//! inherited but what it is given. Nothing here goes through a shell.

#![allow(unsafe_code)]

use super::LaunchError;
use std::ffi::OsString;
use std::io::{self, Read};
use std::path::PathBuf;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

/// How much of a program's output is kept. Real answers are a few hundred
/// bytes; the rest is read and thrown away so the program never blocks on a
/// full pipe.
pub(crate) const MAX_CAPTURE: usize = 64 * 1024;

/// A program to start.
#[derive(Clone, Debug)]
pub(crate) struct Spec {
    program: PathBuf,
    args: Vec<OsString>,
    cwd: Option<PathBuf>,
    /// A session bus address to give the program instead of the environment's.
    bus: Option<OsString>,
    /// Windows process creation flags.
    flags: u32,
}

/// What a finished program left behind.
#[derive(Debug)]
pub(crate) struct Output {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
}

impl Spec {
    /// The program must be given by an absolute path: nothing is searched for
    /// at start time.
    pub fn new(program: PathBuf) -> Result<Self, LaunchError> {
        if !program.is_absolute() {
            return Err(LaunchError::BadInput("the program path must be absolute"));
        }
        Ok(Self {
            program,
            args: Vec::new(),
            cwd: None,
            bus: None,
            flags: 0,
        })
    }

    pub fn args(mut self, args: impl IntoIterator<Item = OsString>) -> Self {
        self.args.extend(args);
        self
    }

    pub fn arg(mut self, arg: impl Into<OsString>) -> Self {
        self.args.push(arg.into());
        self
    }

    pub fn cwd(mut self, dir: PathBuf) -> Self {
        self.cwd = Some(dir);
        self
    }

    pub fn pin_bus(mut self, address: OsString) -> Self {
        self.bus = Some(address);
        self
    }

    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn creation_flags(mut self, flags: u32) -> Self {
        self.flags = flags;
        self
    }

    /// The command with standard input, output and error closed and nothing
    /// else inherited.
    pub fn command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command
            .args(&self.args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(dir) = &self.cwd {
            command.current_dir(dir);
        }
        if let Some(address) = &self.bus {
            command.env("DBUS_SESSION_BUS_ADDRESS", address);
        }
        #[cfg(target_os = "linux")]
        close_inherited(&mut command);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(self.flags);
        }
        let _ = self.flags;
        command
    }

    /// Starts the program and leaves it running on its own. A thread collects
    /// its exit so it does not linger as a zombie.
    pub fn detach(&self, step: &'static str) -> Result<(), LaunchError> {
        let mut command = self.command();
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            // Its own process group: a terminal's Ctrl+C does not reach it.
            command.process_group(0);
        }
        let mut child = command.spawn().map_err(|e| LaunchError::io(step, &e))?;
        thread::spawn(move || {
            let _ = child.wait();
        });
        Ok(())
    }

    /// Runs the program to its end and returns its status and the start of its
    /// output. With a limit, a program that is still running at the end of it
    /// is killed.
    pub fn run(&self, step: &'static str, limit: Option<Duration>) -> Result<Output, LaunchError> {
        let mut command = self.command();
        command.stdout(Stdio::piped());
        let mut child = command.spawn().map_err(|e| LaunchError::io(step, &e))?;
        let (sender, receiver) = mpsc::channel();
        if let Some(mut stdout) = child.stdout.take() {
            thread::spawn(move || {
                let mut kept = Vec::new();
                let _ = (&mut stdout)
                    .take(MAX_CAPTURE as u64)
                    .read_to_end(&mut kept);
                let _ = sender.send(kept);
                // Keep reading so a chatty program is never stuck on a full pipe.
                let _ = io::copy(&mut stdout, &mut io::sink());
            });
        }
        let status = match limit {
            None => child.wait().map_err(|e| LaunchError::io(step, &e))?,
            Some(limit) => {
                let deadline = Instant::now() + limit;
                loop {
                    match child.try_wait() {
                        Ok(Some(status)) => break status,
                        Ok(None) => {}
                        Err(e) => {
                            let _ = child.kill();
                            let _ = child.wait();
                            return Err(LaunchError::io(step, &e));
                        }
                    }
                    if Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(LaunchError::TimedOut(step));
                    }
                    thread::sleep(Duration::from_millis(5));
                }
            }
        };
        // The output has normally ended with the program. A program that left
        // a child holding the pipe open must not hold us up.
        let stdout = receiver
            .recv_timeout(Duration::from_millis(250))
            .unwrap_or_default();
        Ok(Output { status, stdout })
    }
}

/// Marks every descriptor above standard error close-on-exec, so nothing the
/// program was not given reaches it. Kernels without `close_range` keep the
/// default: the standard library opens its own descriptors close-on-exec.
#[cfg(target_os = "linux")]
fn close_inherited(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    // SAFETY: the closure runs between fork and exec and makes one raw system
    // call, which is allowed there. It allocates nothing and takes no lock. The
    // result is ignored on purpose: an old kernel answers ENOSYS and the
    // default stays.
    unsafe {
        command.pre_exec(|| {
            let _ = libc::syscall(
                libc::SYS_close_range,
                3 as libc::c_uint,
                libc::c_uint::MAX,
                libc::CLOSE_RANGE_CLOEXEC,
            );
            Ok(())
        });
    }
}

/// Looks `name` up on `path_var` and returns the absolute path of the first
/// executable regular file. Relative entries (including the empty entry, which
/// means the current folder) are skipped, so the working folder can never
/// supply the program.
#[cfg(unix)]
pub(crate) fn find_on_path(name: &str, path_var: &std::ffi::OsStr) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    if name.is_empty() || name.contains('/') || name.starts_with('-') {
        return None;
    }
    std::env::split_paths(path_var)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(name))
        .find(|candidate| {
            std::fs::metadata(candidate)
                .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        })
}

#[cfg(all(test, target_os = "linux"))]
pub(crate) mod testing {
    //! Fake programs for tests.

    use std::io::Write;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    /// Writes an executable shell script into `dir`, and returns only once
    /// the system will run it.
    ///
    /// Tests run side by side in one process, and any of them may start a
    /// program at any moment. Starting a program copies every open descriptor
    /// into the new process until it executes, so a script that is still being
    /// written can be held open for writing by a copy in someone else's new
    /// process. The system refuses to run a file that anything holds open for
    /// writing ("text file busy"), and the copy goes away within moments, so
    /// the script is only handed out once no writer is left.
    pub fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        std::fs::create_dir_all(dir).expect("dir");
        let path = dir.join(name);
        let mut file = std::fs::File::create(&path).expect("script");
        file.write_all(format!("#!/bin/sh\n{body}\n").as_bytes())
            .expect("script");
        file.set_permissions(std::fs::Permissions::from_mode(0o755))
            .expect("mode");
        drop(file);
        wait_until_not_open_for_writing(&path);
        path
    }

    /// Waits until no process holds `path` open for writing.
    ///
    /// A read lease is granted only while nobody has the file open for
    /// writing, which is the same condition the system checks before it runs
    /// a file. The lease is given back at once.
    fn wait_until_not_open_for_writing(path: &Path) {
        let file = std::fs::File::open(path).expect("open script");
        let fd = file.as_raw_fd();
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            // SAFETY: `fd` is a descriptor this function owns and keeps open
            // for the whole call; setting a lease changes no memory.
            let taken = unsafe { libc::fcntl(fd, libc::F_SETLEASE, libc::F_RDLCK) };
            if taken == 0 {
                // SAFETY: as above; this releases the lease just taken.
                unsafe { libc::fcntl(fd, libc::F_SETLEASE, libc::F_UNLCK) };
                return;
            }
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::EAGAIN) {
                // This file system does not do leases. Give any copy a moment
                // to go away and carry on.
                std::thread::sleep(Duration::from_millis(50));
                return;
            }
            assert!(
                Instant::now() < deadline,
                "{} is still open for writing",
                path.display()
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// A script that writes its arguments, one per line, then the working
    /// folder, to `out`, and exits with `status`.
    pub fn recorder(dir: &Path, name: &str, out: &Path, status: i32) -> PathBuf {
        script(
            dir,
            name,
            &format!(
                "{{ for a in \"$@\"; do printf '%s\\n' \"$a\"; done; printf 'cwd=%s\\n' \"$PWD\"; }} > '{}'\nexit {status}",
                out.display()
            ),
        )
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::testing::*;
    use super::*;
    use crate::testutil::Fixture;
    use std::os::fd::AsRawFd;

    #[test]
    fn a_script_written_while_other_threads_start_programs_still_starts() {
        // Other threads in the test binary start programs all the time. A
        // start copies the open descriptors for a moment; if one copy holds a
        // script's write descriptor, starting that script fails with "text
        // file busy". The helper must hide that from every test.
        let fx = Fixture::new("spawn-busy");
        let true_program =
            find_on_path("true", std::ffi::OsStr::new("/usr/bin:/bin")).expect("true");
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let busy: Vec<_> = (0..4)
            .map(|_| {
                let stop = stop.clone();
                let program = true_program.clone();
                thread::spawn(move || {
                    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                        let _ = Spec::new(program.clone())
                            .unwrap()
                            .run("busy", Some(Duration::from_secs(10)));
                    }
                })
            })
            .collect();
        for round in 0..300 {
            let tool = script(fx.path(), &format!("tool{round}"), "exit 0");
            let status = Spec::new(tool)
                .unwrap()
                .run("test", Some(Duration::from_secs(10)));
            match status {
                Ok(output) => assert!(output.status.success(), "round {round}"),
                Err(error) => panic!("round {round}: {error:?}"),
            }
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        for handle in busy {
            handle.join().unwrap();
        }
    }

    #[test]
    fn a_relative_program_is_refused() {
        assert!(Spec::new(PathBuf::from("xdg-open")).is_err());
        assert!(Spec::new(PathBuf::from("./xdg-open")).is_err());
        assert!(Spec::new(PathBuf::from("/usr/bin/xdg-open")).is_ok());
    }

    #[test]
    fn path_lookup_skips_relative_entries_and_things_that_do_not_run() {
        let fx = Fixture::new("spawn-path");
        let good = fx.path().join("good");
        let other = fx.path().join("other");
        script(&good, "tool", "exit 0");
        script(&other, "tool", "exit 0");
        std::fs::write(good.join("plain"), "data").unwrap();
        std::fs::create_dir_all(good.join("dir")).unwrap();

        let path = std::env::join_paths([good.as_path(), other.as_path()]).unwrap();
        assert_eq!(find_on_path("tool", &path), Some(good.join("tool")));
        // Not executable, and a folder.
        assert_eq!(find_on_path("plain", &path), None);
        assert_eq!(find_on_path("dir", &path), None);
        assert_eq!(find_on_path("missing", &path), None);

        // A relative entry, or the empty one (the current folder), is never
        // searched, even when it would lead to the program.
        let here = std::env::current_dir().unwrap();
        let up = "../".repeat(here.components().count() - 1);
        let relative_to_good = format!("{up}{}", good.to_str().unwrap().trim_start_matches('/'));
        assert!(here.join(&relative_to_good).join("tool").exists());
        for relative in ["", ".", "good", relative_to_good.as_str()] {
            assert_eq!(
                find_on_path("tool", std::ffi::OsStr::new(relative)),
                None,
                "{relative:?}"
            );
        }

        // The name is a bare file name.
        for bad in ["", "../good/tool", "a/b", "-tool"] {
            assert_eq!(find_on_path(bad, &path), None, "{bad:?}");
        }
    }

    #[test]
    fn the_arguments_arrive_one_by_one_and_nothing_reads_a_shell_line() {
        let fx = Fixture::new("spawn-argv");
        let out = fx.path().join("argv.txt");
        let tool = recorder(&fx.path().join("bin"), "tool", &out, 0);
        let nasty = [
            "plain",
            "with space",
            "semi;colon",
            "$(touch pwned)",
            "`touch pwned`",
            "a && b",
            "--option",
            "'quoted'",
            "new\nline",
            "",
        ];
        let status = Spec::new(tool)
            .unwrap()
            .args(nasty.iter().map(OsString::from))
            .cwd(fx.path().to_path_buf())
            .run("test", Some(Duration::from_secs(10)))
            .unwrap()
            .status;
        assert!(status.success());
        let got = std::fs::read_to_string(&out).unwrap();
        let expected: String = nasty
            .iter()
            .map(|a| format!("{a}\n"))
            .chain([format!("cwd={}\n", fx.path().display())])
            .collect();
        assert_eq!(got, expected);
        assert!(!fx.path().join("pwned").exists());
    }

    #[test]
    fn output_is_returned_and_a_failing_status_is_reported() {
        let fx = Fixture::new("spawn-output");
        let tool = script(fx.path(), "tool", "echo hello; exit 3");
        let output = Spec::new(tool)
            .unwrap()
            .run("test", Some(Duration::from_secs(10)))
            .unwrap();
        assert_eq!(output.stdout, b"hello\n");
        assert_eq!(output.status.code(), Some(3));
    }

    #[test]
    fn a_program_that_hangs_is_stopped_at_the_limit() {
        let fx = Fixture::new("spawn-hang");
        let tool = script(fx.path(), "tool", "exec sleep 30");
        let started = Instant::now();
        let error = Spec::new(tool)
            .unwrap()
            .run("wait", Some(Duration::from_millis(200)))
            .unwrap_err();
        assert!(matches!(error, LaunchError::TimedOut("wait")), "{error:?}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_chatty_program_is_neither_blocked_nor_kept_whole() {
        let fx = Fixture::new("spawn-chatty");
        // About 1 MiB of output.
        let tool = script(fx.path(), "tool", "head -c 1048576 /dev/zero | tr '\\0' x");
        let output = Spec::new(tool)
            .unwrap()
            .run("test", Some(Duration::from_secs(20)))
            .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout.len(), MAX_CAPTURE);
    }

    #[test]
    fn standard_input_is_closed() {
        let fx = Fixture::new("spawn-stdin");
        let out = fx.path().join("stdin.txt");
        let tool = script(
            fx.path(),
            "tool",
            &format!("cat > '{}'; exit 0", out.display()),
        );
        let output = Spec::new(tool)
            .unwrap()
            .run("test", Some(Duration::from_secs(5)))
            .unwrap();
        assert!(output.status.success());
        assert_eq!(std::fs::read(&out).unwrap(), b"");
    }

    #[test]
    fn a_pinned_bus_address_reaches_the_program() {
        let fx = Fixture::new("spawn-bus");
        let tool = script(fx.path(), "tool", "printf %s \"$DBUS_SESSION_BUS_ADDRESS\"");
        let output = Spec::new(tool)
            .unwrap()
            .pin_bus(OsString::from("unix:path=/run/user/1/bus"))
            .run("test", Some(Duration::from_secs(5)))
            .unwrap();
        assert_eq!(output.stdout, b"unix:path=/run/user/1/bus");
    }

    fn kernel_has_close_range() -> bool {
        let release = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
        let mut parts = release.split(|c: char| !c.is_ascii_digit());
        let major: u32 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
        let minor: u32 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
        (major, minor) >= (5, 11)
    }

    #[test]
    fn a_descriptor_the_program_was_not_given_is_not_inherited() {
        if !kernel_has_close_range() {
            eprintln!("skipped: the kernel has no close_range");
            return;
        }
        let fx = Fixture::new("spawn-fds");
        // A duplicate is not close-on-exec, which stands for a descriptor
        // another library in the process forgot to mark. Its number is a high
        // one: the shell that lists its descriptors has a few of its own (its
        // script, the folder it reads), and a low number freed by another
        // test could be the same as one of those.
        let file = std::fs::File::open("/proc/self/status").unwrap();
        // A lower limit on descriptors only means a lower minimum is tried.
        let leaked = [700, 200, 100]
            .into_iter()
            .map(|minimum| {
                // SAFETY: duplicates a descriptor this test owns.
                unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD, minimum) }
            })
            .find(|duplicate| *duplicate >= 0)
            .expect("a duplicate descriptor");
        assert!(leaked >= 100);
        let tool = script(
            fx.path(),
            "tool",
            "for f in /proc/$$/fd/*; do echo \"${f##*/}\"; done",
        );
        let output = Spec::new(tool)
            .unwrap()
            .run("test", Some(Duration::from_secs(5)))
            .unwrap();
        // SAFETY: closes the descriptor made above.
        unsafe { libc::close(leaked) };
        let listed = String::from_utf8(output.stdout).unwrap();
        assert!(
            !listed.lines().any(|l| l == leaked.to_string()),
            "descriptor {leaked} was inherited: {listed:?}"
        );
    }

    #[test]
    fn a_detached_program_runs_on_its_own() {
        let fx = Fixture::new("spawn-detach");
        let out = fx.path().join("ran.txt");
        let tool = recorder(&fx.path().join("bin"), "tool", &out, 0);
        Spec::new(tool).unwrap().arg("one").detach("test").unwrap();
        let started = Instant::now();
        while !out.exists() && started.elapsed() < Duration::from_secs(10) {
            thread::sleep(Duration::from_millis(10));
        }
        thread::sleep(Duration::from_millis(100));
        assert!(std::fs::read_to_string(&out).unwrap().starts_with("one\n"));
    }

    #[test]
    fn a_program_that_cannot_start_is_an_error() {
        let fx = Fixture::new("spawn-missing");
        let error = Spec::new(fx.path().join("not-here"))
            .unwrap()
            .run("start", Some(Duration::from_secs(1)))
            .unwrap_err();
        assert!(
            matches!(error, LaunchError::Failed { step: "start", .. }),
            "{error:?}"
        );
        assert!(
            Spec::new(fx.path().join("not-here"))
                .unwrap()
                .detach("start")
                .is_err()
        );
    }
}
