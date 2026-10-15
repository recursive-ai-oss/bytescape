//! Helpers shared by the integration tests: a fixture folder, and the helper
//! program started the way `pkexec` would start it, unprivileged, with pipes
//! on standard input and output.

#![allow(dead_code)]
// The helper never starts programs; these tests are the one place that starts
// the helper itself, to try it as a program.
#![allow(clippy::disallowed_types, clippy::disallowed_methods)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![allow(clippy::indexing_slicing, clippy::arithmetic_side_effects)]
#![allow(clippy::cast_possible_truncation)]

use bs_ipc::{Client, Direction, FrameReader, MAX_FRAME_LEN, NONCE_LEN};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

static COUNTER: AtomicU32 = AtomicU32::new(0);

pub const NONCE: [u8; NONCE_LEN] = [
    0x01, 0x23, 0x45, 0x67, 0x89, 0x01, 0x23, 0x45, 0x01, 0x23, 0x45, 0x67, 0x89, 0x01, 0x23, 0x45,
];
pub const NONCE_HEX: &str = "01234567890123450123456789012345";

/// A fresh folder under the temporary directory, removed on drop. Every path a
/// test deletes is made by [`Fixture::p`], which cannot leave the folder.
pub struct Fixture {
    root: PathBuf,
    base: PathBuf,
}

impl Fixture {
    pub fn new(label: &str) -> Self {
        let base = fs::canonicalize(std::env::temp_dir()).unwrap();
        let root = base.join(format!(
            "bs-helper-it-{label}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        Self {
            root: fs::canonicalize(&root).unwrap(),
            base,
        }
    }

    pub fn path(&self) -> &Path {
        &self.root
    }

    /// A path inside the fixture.
    pub fn p(&self, relative: &str) -> PathBuf {
        assert!(
            !relative.starts_with('/') && !relative.split('/').any(|part| part == ".."),
            "fixture paths stay inside the fixture: {relative}"
        );
        self.root.join(relative)
    }

    pub fn write(&self, relative: &str, contents: &str) {
        let path = self.p(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    pub fn mkdir(&self, relative: &str) {
        fs::create_dir_all(self.p(relative)).unwrap();
    }

    pub fn exists(&self, relative: &str) -> bool {
        fs::symlink_metadata(self.p(relative)).is_ok()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let named = self
            .root
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("bs-helper-it-"));
        if self.root.starts_with(&self.base) && named {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
}

/// The user this test runs as: the owner of its own `/proc` entry.
pub fn current_uid() -> u32 {
    use std::os::unix::fs::MetadataExt;
    fs::metadata("/proc/self").unwrap().uid()
}

/// Whether this test runs as the superuser, which says so when it does.
///
/// The helper only serves the user who asked for it through `pkexec`, and
/// never the superuser, so a test that starts it as the user it runs as has
/// nothing to try when that user is root (a container, or a namespace that
/// maps the caller to root). The tests of the helper with root's rights are
/// in `root.rs`, which starts it for an ordinary account.
pub fn refuses_the_superuser() -> bool {
    let root = current_uid() == 0;
    if root {
        println!("NOT RUN (euid 0): the helper does not act for the superuser");
    }
    root
}

pub fn helper_path() -> &'static str {
    env!("CARGO_BIN_EXE_bytescape-helper")
}

/// How to start the helper.
pub struct Launch {
    pub args: Vec<String>,
    /// The value of `PKEXEC_UID`, or none.
    pub pkexec_uid: Option<String>,
    pub stdin: Stdio,
    pub stdout: Stdio,
    pub env: Vec<(String, String)>,
}

impl Launch {
    pub fn new(mode: &str) -> Self {
        Self {
            args: vec![
                mode.into(),
                "--protocol".into(),
                "1".into(),
                "--nonce".into(),
                NONCE_HEX.into(),
            ],
            pkexec_uid: Some(current_uid().to_string()),
            stdin: Stdio::piped(),
            stdout: Stdio::piped(),
            env: Vec::new(),
        }
    }

    pub fn spawn(self) -> Helper {
        let mut command = Command::new(helper_path());
        command
            .args(&self.args)
            .env_clear()
            .stdin(self.stdin)
            .stdout(self.stdout)
            .stderr(Stdio::null());
        if let Some(uid) = &self.pkexec_uid {
            command.env("PKEXEC_UID", uid);
        }
        for (key, value) in &self.env {
            command.env(key, value);
        }
        let mut child = command.spawn().unwrap();
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        Helper {
            child,
            stdin,
            stdout,
        }
    }
}

pub struct Helper {
    pub child: Child,
    pub stdin: Option<ChildStdin>,
    pub stdout: Option<ChildStdout>,
}

impl Helper {
    /// The exit status, waiting at most `limit`; kills the helper if it is
    /// still running then and fails the test.
    pub fn exit_code(&mut self, limit: Duration) -> i32 {
        let start = Instant::now();
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status.code().unwrap_or(-1);
            }
            if start.elapsed() > limit {
                let _ = self.child.kill();
                let _ = self.child.wait();
                panic!("the helper did not exit within {limit:?}");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Everything the helper wrote, until it closes its output.
    pub fn all_output(&mut self) -> Vec<u8> {
        use std::io::Read;
        let mut out = Vec::new();
        if let Some(mut stdout) = self.stdout.take() {
            let _ = stdout.read_to_end(&mut out);
        }
        out
    }

    /// The protocol client on the helper's pipes, after a good hello.
    pub fn client(&mut self) -> Client<ChildStdout, ChildStdin> {
        let stdout = self.stdout.take().unwrap();
        let stdin = self.stdin.take().unwrap();
        Client::connect(stdout, stdin, NONCE).unwrap()
    }

    pub fn reader(&mut self) -> FrameReader<ChildStdout> {
        FrameReader::new(
            self.stdout.take().unwrap(),
            Direction::ToInterface,
            MAX_FRAME_LEN,
        )
    }
}

impl Drop for Helper {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
