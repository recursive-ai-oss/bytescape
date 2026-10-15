//! Which installed program or package a big folder belongs to.
//!
//! This is what the "Uninstall…" and "Installed by package `foo`" handoffs
//! need. Matching runs in the background after a scan, only for folders of at
//! least [`MIN_MATCH_BYTES`], and its answers are kept for the session. It
//! never runs on a scan thread and never delays one.
//!
//! - **Windows:** the registry's Uninstall keys (the 64-bit view, the 32-bit
//!   view and the current user's) give each program an `InstallLocation` and
//!   an `UninstallString`.
//! - **Linux:** dpkg's file lists say which package installed a folder; where
//!   rpm is used, `rpm -qf` is asked instead.
//!
//! Matching only reads. The removal command a package gets is text for the
//! user to copy; it is never run.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

#[cfg(target_os = "linux")]
mod niceness;
#[cfg(any(windows, test))]
mod programs;
mod uninstall;

#[cfg(target_os = "linux")]
mod dpkg;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(windows)]
mod registry;
#[cfg(windows)]
mod windows;

#[cfg(windows)]
pub(crate) use uninstall::quote_arguments;
pub use uninstall::{UninstallCommand, UninstallParseError, parse_uninstall_string};

/// Folders smaller than this are not matched.
pub const MIN_MATCH_BYTES: u64 = 100_000_000;

/// Most folders matched in one request.
const MAX_REQUEST: usize = 10_000;

/// A folder to match, with its size on disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub path: PathBuf,
    pub bytes: u64,
}

/// How a folder relates to the program that was found for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Relation {
    /// The folder is the program's install location.
    Exact,
    /// The folder is inside the install location. Uninstalling removes more
    /// than this folder.
    Inside,
}

/// An installed Windows program.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Program {
    pub name: String,
    pub publisher: Option<String>,
    /// The program's install location.
    pub location: PathBuf,
    pub relation: Relation,
    /// How to uninstall it. `None` when the program registered no uninstaller,
    /// or one that cannot be started safely.
    pub uninstall: Option<UninstallCommand>,
}

/// The tool that installed a package.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum PackageManager {
    Dpkg,
    Rpm,
}

/// An installed Linux package.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Package {
    pub manager: PackageManager,
    pub name: String,
    /// Files and folders inside the matched folder that the package lists.
    /// `None` where the package tool does not say.
    pub listed_entries: Option<u64>,
    /// Other packages that also list something inside the folder.
    pub other_packages: u32,
    /// A command that removes the package, such as `sudo apt remove foo`, for
    /// the user to copy. It is never run, and it is `None` unless the package
    /// name is plain enough to paste into a terminal safely.
    pub removal_command: Option<String>,
    /// An `appstream://` link that opens the package in Discover or GNOME
    /// Software, when the package ships exactly one AppStream component. See
    /// [`crate::launch::open_in_software_center`].
    pub software_center_url: Option<String>,
}

/// What owns a folder.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Owner {
    Program(Program),
    Package(Package),
}

/// What is known about a folder.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Lookup {
    /// Never asked about: not requested, or smaller than [`MIN_MATCH_BYTES`].
    NotAsked,
    /// Being matched.
    Pending,
    /// Matched, and nothing owns it (or it is shared).
    NoOwner,
    Found(Owner),
}

/// Whether a name is plain enough to be pasted into a terminal as an argument:
/// letters, digits and `+ . - _`, starting with a letter or digit.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn is_package_name(name: &str) -> bool {
    name.len() >= 2
        && name.len() <= 200
        && name.starts_with(|c: char| c.is_ascii_alphanumeric())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-' | '_'))
}

/// Where the answers come from.
pub(crate) trait Backend: Send + Sync {
    /// Looks up each folder, in order. Returns early when `cancel` is set.
    fn find(&self, folders: &[PathBuf], cancel: &AtomicBool) -> Vec<Option<Owner>>;
}

#[derive(Default)]
struct State {
    answered: HashMap<PathBuf, Option<Owner>>,
    pending: HashSet<PathBuf>,
}

struct Shared {
    state: Mutex<State>,
    /// Only one batch reads the package database at a time.
    one_at_a_time: Mutex<()>,
    cancel: AtomicBool,
}

/// Matches big folders to what installed them, in the background, and keeps
/// the answers for the life of the value.
///
/// Dropping it stops any matching in progress.
pub struct Matcher {
    shared: Arc<Shared>,
    backend: Arc<dyn Backend>,
}

impl Matcher {
    /// A matcher that asks this system.
    pub fn new() -> Self {
        Self::with_backend(system_backend())
    }

    pub(crate) fn with_backend(backend: Arc<dyn Backend>) -> Self {
        Self {
            shared: Arc::new(Shared {
                state: Mutex::new(State::default()),
                one_at_a_time: Mutex::new(()),
                cancel: AtomicBool::new(false),
            }),
            backend,
        }
    }

    /// Starts matching the folders that are big enough and not already
    /// answered, on a background thread, and returns at once.
    pub fn request(&self, candidates: &[Candidate]) {
        let mut wanted: Vec<PathBuf> = Vec::new();
        {
            let mut state = lock(&self.shared.state);
            for candidate in candidates {
                if wanted.len() >= MAX_REQUEST {
                    break;
                }
                let path = &candidate.path;
                if candidate.bytes < MIN_MATCH_BYTES
                    || !path.is_absolute()
                    || state.answered.contains_key(path)
                    || state.pending.contains(path)
                {
                    continue;
                }
                state.pending.insert(path.clone());
                wanted.push(path.clone());
            }
        }
        if wanted.is_empty() {
            return;
        }
        let shared = Arc::clone(&self.shared);
        let backend = Arc::clone(&self.backend);
        let batch = wanted.clone();
        let spawned = std::thread::Builder::new()
            .name("software-matching".into())
            .spawn(move || {
                lower_priority();
                let _turn = lock(&shared.one_at_a_time);
                let found = if shared.cancel.load(Ordering::Relaxed) {
                    Vec::new()
                } else {
                    backend.find(&batch, &shared.cancel)
                };
                let mut state = lock(&shared.state);
                for (index, path) in batch.into_iter().enumerate() {
                    state.pending.remove(&path);
                    // A batch cut short by cancelling is not an answer.
                    if !shared.cancel.load(Ordering::Relaxed) {
                        state
                            .answered
                            .insert(path, found.get(index).cloned().flatten());
                    }
                }
            });
        if spawned.is_err() {
            let mut state = lock(&self.shared.state);
            for path in &wanted {
                state.pending.remove(path);
            }
        }
    }

    /// What is known about a folder now.
    pub fn lookup(&self, path: &Path) -> Lookup {
        let state = lock(&self.shared.state);
        if let Some(answer) = state.answered.get(path) {
            return match answer {
                Some(owner) => Lookup::Found(owner.clone()),
                None => Lookup::NoOwner,
            };
        }
        if state.pending.contains(path) {
            Lookup::Pending
        } else {
            Lookup::NotAsked
        }
    }

    /// Whether nothing is being matched.
    pub fn is_idle(&self) -> bool {
        lock(&self.shared.state).pending.is_empty()
    }
}

impl Default for Matcher {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Matcher {
    fn drop(&mut self) {
        self.shared.cancel.store(true, Ordering::Relaxed);
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Matching is background work: it yields the processor to a scan.
fn lower_priority() {
    #[cfg(target_os = "linux")]
    niceness::lower();
}

#[cfg(target_os = "linux")]
fn system_backend() -> Arc<dyn Backend> {
    Arc::new(linux::System::new())
}

#[cfg(windows)]
fn system_backend() -> Arc<dyn Backend> {
    Arc::new(windows::System::new())
}

#[cfg(not(any(target_os = "linux", windows)))]
fn system_backend() -> Arc<dyn Backend> {
    struct Nothing;
    impl Backend for Nothing {
        fn find(&self, folders: &[PathBuf], _: &AtomicBool) -> Vec<Option<Owner>> {
            vec![None; folders.len()]
        }
    }
    Arc::new(Nothing)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::time::{Duration, Instant};

    /// Answers from a table, counting how often it is asked, and optionally
    /// waiting for a signal first.
    struct Table {
        owners: HashMap<PathBuf, Owner>,
        asked: Mutex<Vec<PathBuf>>,
        batches: AtomicUsize,
        gate: Option<Arc<(Mutex<bool>, std::sync::Condvar)>>,
    }

    impl Backend for Table {
        fn find(&self, folders: &[PathBuf], _: &AtomicBool) -> Vec<Option<Owner>> {
            if let Some(gate) = &self.gate {
                let (open, signal) = &**gate;
                let mut open = lock(open);
                while !*open {
                    open = signal.wait(open).unwrap();
                }
            }
            self.batches.fetch_add(1, Ordering::SeqCst);
            lock(&self.asked).extend(folders.iter().cloned());
            folders
                .iter()
                .map(|f| self.owners.get(f).cloned())
                .collect()
        }
    }

    fn package(name: &str) -> Owner {
        Owner::Package(Package {
            manager: PackageManager::Dpkg,
            name: name.into(),
            listed_entries: Some(3),
            other_packages: 0,
            removal_command: Some(format!("sudo apt remove {name}")),
            software_center_url: None,
        })
    }

    /// An absolute path on whichever system the test runs on: `/opt/big` stays
    /// as it is on Unix and becomes `C:\opt\big` on Windows, where a path
    /// without a drive is not absolute.
    fn abs(path: &str) -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(format!("C:{}", path.replace('/', r"\")))
        } else {
            PathBuf::from(path)
        }
    }

    fn candidate(path: &str, bytes: u64) -> Candidate {
        Candidate {
            path: abs(path),
            bytes,
        }
    }

    fn wait_idle(matcher: &Matcher) {
        let started = Instant::now();
        while !matcher.is_idle() && started.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(matcher.is_idle());
    }

    fn table(entries: &[(&str, &str)]) -> Arc<Table> {
        Arc::new(Table {
            owners: entries
                .iter()
                .map(|(path, name)| (abs(path), package(name)))
                .collect(),
            asked: Mutex::new(Vec::new()),
            batches: AtomicUsize::new(0),
            gate: None,
        })
    }

    #[test]
    fn only_big_folders_are_matched_and_answers_are_kept() {
        let backend = table(&[("/opt/big", "bigpkg"), ("/opt/small", "smallpkg")]);
        let matcher = Matcher::with_backend(backend.clone());
        assert_eq!(matcher.lookup(&abs("/opt/big")), Lookup::NotAsked);
        matcher.request(&[
            candidate("/opt/big", MIN_MATCH_BYTES),
            candidate("/opt/small", MIN_MATCH_BYTES - 1),
            candidate("/opt/unowned", MIN_MATCH_BYTES * 3),
            Candidate {
                path: PathBuf::from("relative/path"),
                bytes: MIN_MATCH_BYTES * 3,
            },
        ]);
        wait_idle(&matcher);
        assert_eq!(
            matcher.lookup(&abs("/opt/big")),
            Lookup::Found(package("bigpkg"))
        );
        assert_eq!(matcher.lookup(&abs("/opt/unowned")), Lookup::NoOwner);
        // Small and relative folders were never asked about.
        assert_eq!(matcher.lookup(&abs("/opt/small")), Lookup::NotAsked);
        assert_eq!(matcher.lookup(Path::new("relative/path")), Lookup::NotAsked);
        assert_eq!(
            *lock(&backend.asked),
            [abs("/opt/big"), abs("/opt/unowned")]
        );

        // Asking again for what is answered does not ask the backend again,
        // but a new folder does.
        matcher.request(&[
            candidate("/opt/big", MIN_MATCH_BYTES),
            candidate("/opt/unowned", MIN_MATCH_BYTES),
            candidate("/opt/small", MIN_MATCH_BYTES),
        ]);
        wait_idle(&matcher);
        assert_eq!(backend.batches.load(Ordering::SeqCst), 2);
        assert_eq!(lock(&backend.asked).len(), 3);
        assert_eq!(
            matcher.lookup(&abs("/opt/small")),
            Lookup::Found(package("smallpkg"))
        );
    }

    #[test]
    fn requesting_never_waits_for_the_answer() {
        let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let backend = Arc::new(Table {
            owners: HashMap::new(),
            asked: Mutex::new(Vec::new()),
            batches: AtomicUsize::new(0),
            gate: Some(gate.clone()),
        });
        let matcher = Matcher::with_backend(backend);
        let started = Instant::now();
        matcher.request(&[candidate("/opt/slow", MIN_MATCH_BYTES)]);
        // The backend is blocked, yet the call came back and says so.
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(matcher.lookup(&abs("/opt/slow")), Lookup::Pending);
        assert!(!matcher.is_idle());
        // A second request for the same folder does not queue it again.
        matcher.request(&[candidate("/opt/slow", MIN_MATCH_BYTES)]);

        *lock(&gate.0) = true;
        gate.1.notify_all();
        wait_idle(&matcher);
        assert_eq!(matcher.lookup(&abs("/opt/slow")), Lookup::NoOwner);
    }

    #[test]
    fn dropping_the_matcher_discards_a_batch_in_progress() {
        let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let backend = Arc::new(Table {
            owners: HashMap::new(),
            asked: Mutex::new(Vec::new()),
            batches: AtomicUsize::new(0),
            gate: Some(gate.clone()),
        });
        let matcher = Matcher::with_backend(backend);
        let shared = Arc::clone(&matcher.shared);
        matcher.request(&[candidate("/opt/slow", MIN_MATCH_BYTES)]);
        drop(matcher);
        *lock(&gate.0) = true;
        gate.1.notify_all();
        let started = Instant::now();
        while !lock(&shared.state).pending.is_empty() && started.elapsed() < Duration::from_secs(10)
        {
            std::thread::sleep(Duration::from_millis(5));
        }
        let state = lock(&shared.state);
        assert!(state.pending.is_empty());
        // The batch was cut short, so it left no answer behind.
        assert!(state.answered.is_empty());
    }

    #[test]
    fn a_huge_request_is_bounded() {
        let backend = table(&[]);
        let matcher = Matcher::with_backend(backend.clone());
        let many: Vec<Candidate> = (0..MAX_REQUEST + 500)
            .map(|n| candidate(&format!("/opt/f{n}"), MIN_MATCH_BYTES))
            .collect();
        matcher.request(&many);
        wait_idle(&matcher);
        assert_eq!(lock(&backend.asked).len(), MAX_REQUEST);
    }

    #[test]
    fn package_names_are_plain() {
        for good in ["foo", "libc6", "g++", "python3.12", "a-b_c", "x86"] {
            assert!(is_package_name(good), "{good}");
        }
        for bad in [
            "",
            "a",
            "-rf",
            ".hidden",
            "foo bar",
            "foo;bar",
            "$(x)",
            "a`b",
            "a|b",
            "a&b",
            "a/b",
            "a\nb",
            "é",
            "a'b",
            "a\"b",
            "foo:amd64",
        ] {
            assert!(!is_package_name(bad), "{bad:?}");
        }
        assert!(!is_package_name(&"a".repeat(201)));
    }
}
