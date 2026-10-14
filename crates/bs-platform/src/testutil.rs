//! Helpers for tests that need a scratch directory.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// A fresh directory under the system temporary directory, removed on drop.
///
/// Removal is refused for any path that is not inside the temporary
/// directory and named like a fixture, so a bug here cannot reach real data.
pub(crate) struct Fixture {
    root: PathBuf,
}

impl Fixture {
    pub fn new(label: &str) -> Self {
        let base = std::env::temp_dir();
        let name = format!(
            "bs-platform-{label}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let root = base.join(name);
        fs::create_dir_all(&root).expect("create fixture root");
        // Work with the canonical path so comparisons against resolved paths hold.
        let root = fs::canonicalize(&root).expect("canonicalize fixture root");
        Self { root }
    }

    pub fn path(&self) -> &Path {
        &self.root
    }

    /// Creates a file (and its parents) under the fixture.
    pub fn write(&self, relative: &str, contents: &str) {
        let path = self.checked(relative);
        fs::create_dir_all(path.parent().expect("has parent")).expect("create parents");
        fs::write(path, contents).expect("write fixture file");
    }

    pub fn mkdir(&self, relative: &str) {
        fs::create_dir_all(self.checked(relative)).expect("create fixture dir");
    }

    #[cfg(unix)]
    pub fn symlink(&self, target: &str, relative: &str) {
        let path = self.checked(relative);
        fs::create_dir_all(path.parent().expect("has parent")).expect("create parents");
        std::os::unix::fs::symlink(target, path).expect("create symlink");
    }

    /// A path inside the fixture. Refuses anything that climbs out.
    fn checked(&self, relative: &str) -> PathBuf {
        assert!(
            !Path::new(relative).is_absolute() && !relative.split('/').any(|part| part == ".."),
            "fixture paths stay inside the fixture: {relative}"
        );
        self.root.join(relative)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let temp = std::env::temp_dir();
        let temp = fs::canonicalize(&temp).unwrap_or(temp);
        let named_like_fixture = self
            .root
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("bs-platform-"));
        if self.root.starts_with(&temp) && named_like_fixture {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
}
