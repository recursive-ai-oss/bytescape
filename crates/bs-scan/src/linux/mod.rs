//! The Linux walker: `getdents64` and `statx` relative to open directory
//! handles, on a work-stealing pool.
//!
//! Rules it keeps, each with the reason:
//!
//! * Directories are opened with `O_NOFOLLOW` relative to their parent's
//!   handle, so a directory swapped for a link while the scan runs is
//!   refused, not followed. Entries are examined with
//!   `AT_SYMLINK_NOFOLLOW | AT_NO_AUTOMOUNT`: links are counted as links and
//!   automount points are not triggered.
//! * Only the fields it uses are requested from `statx`.
//! * A directory is entered only if the caller's [`Boundary`] allows its mount.
//!   Nested btrfs subvolumes share their parent's mount ID and are entered. A
//!   mount that is not allowed is recorded as an empty directory flagged
//!   [`NodeFlags::NOT_FOLLOWED`], and its contents (which are another drive's)
//!   are not counted. Its own sizes are not counted either, because the numbers
//!   `statx` returns for a mount point describe the mounted filesystem's root,
//!   not an entry of this one.
//! * A directory that cannot be opened or listed is recorded as unreadable with
//!   the error kind, and the scan carries on. An entry that vanishes between
//!   being listed and being examined is skipped and counted as vanished.
//! * Files with several links are keyed by `(device, inode)` so the tree counts
//!   them once, and are flagged as hard links even when only one of their names
//!   lies inside the scan.
//! * Each file gets its category as it is read: the directory's place in the
//!   category tables is looked up once, from the directory's full path, and the
//!   files in it take that answer or else are told by their extension. The path
//!   is only ever used as text for that lookup; nothing is opened by it.

pub(crate) mod sys;
#[cfg(feature = "iouring-bench")]
pub(crate) mod uring;

use crate::boundary::{Boundary, Placement};
use crate::classify::{Classifier, Here, join};
use crate::dirent;
use crate::pool::{self, Worker};
use crate::scanner::{ScanContext, ScanEnd, ScanError, Scanner};
use crate::threads;
use bs_tree::{DirKey, FileKey, Meta, NodeFlags, NodeKind, ReadErrorKind, ReadFailure};
use std::cell::Cell;
use std::ffi::CStr;
use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::{Arc, Once};

/// The directory walker for Linux.
#[derive(Clone, Copy, Debug, Default)]
pub struct Walker;

impl Walker {
    /// Scans `root` to the end on the calling thread's behalf and returns the
    /// result. A convenience over [`crate::start`].
    pub fn scan_blocking(root: &Path, options: crate::ScanOptions) -> crate::ScanResult {
        crate::start(Arc::new(Walker), root.to_path_buf(), options).wait()
    }
}

/// Bytes read from a directory per `getdents64` call.
const READ_BUFFER: usize = 128 * 1024;

/// How often, in entries, a long directory checks whether to send its batch.
const FLUSH_CHECK_EVERY: usize = 256;

/// A file smaller than its logical size by at least this much is sparse or
/// compressed (smaller gaps are rounding and small-file packing).
const SPARSE_MARGIN: u64 = 4096;

use crate::OpenHook;

static OPEN_HOOK_ON: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static OPEN_HOOK: std::sync::Mutex<Option<OpenHook>> = std::sync::Mutex::new(None);

/// Installs (or removes) the hook. For tests only.
pub(crate) fn set_before_open_hook(hook: Option<OpenHook>) {
    use std::sync::atomic::Ordering;
    OPEN_HOOK_ON.store(hook.is_some(), Ordering::SeqCst);
    // A hook that panicked (a test of exactly that) poisons the lock; the
    // slot is still usable.
    let mut slot = OPEN_HOOK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *slot = hook;
}

fn run_before_open_hook(name_with_nul: &[u8]) {
    if !OPEN_HOOK_ON.load(std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    let slot = OPEN_HOOK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(hook) = slot.as_ref() {
        hook(&name_with_nul[..name_with_nul.len().saturating_sub(1)]);
    }
}

thread_local! {
    static READ_BUF: Cell<Vec<u8>> = const { Cell::new(Vec::new()) };
}

impl Scanner for Walker {
    fn name(&self) -> &'static str {
        "linux-walker"
    }

    fn scan(&self, root: &Path, ctx: &ScanContext) -> Result<ScanEnd, ScanError> {
        scan_with_variant(root, ctx, StatVariant::default())
    }
}

/// How the walker examines the entries a listing gave: one `statx` call each
/// (the only way outside the benchmark build), or through a ring.
#[cfg(feature = "iouring-bench")]
pub(crate) type StatVariant = Option<uring::Config>;
#[cfg(not(feature = "iouring-bench"))]
pub(crate) type StatVariant = Option<std::convert::Infallible>;

fn scan_with_variant(
    root: &Path,
    ctx: &ScanContext,
    variant: StatVariant,
) -> Result<ScanEnd, ScanError> {
    {
        static LIMIT: Once = Once::new();
        LIMIT.call_once(|| {
            sys::raise_open_file_limit();
        });

        // The root is the user's own choice, so links in it are resolved once
        // here. Nothing below it is ever followed.
        let canonical = std::fs::canonicalize(root).map_err(ScanError::Root)?;
        let fd = sys::open_root(&canonical).map_err(|e| {
            if e.raw_os_error() == Some(libc::ENOTDIR) {
                ScanError::NotADirectory(canonical.clone())
            } else {
                ScanError::Root(e)
            }
        })?;
        let st = sys::stat_fd(fd.as_fd()).map_err(ScanError::Root)?;
        let boundary = ctx.options().boundary.including(Placement {
            mount_id: st.mnt_id,
            device: st.dev,
        });
        let key = ctx.next_dir_key().ok_or(ScanError::Unsupported)?;

        // The root's record goes first, before any worker exists.
        let mut first = bs_tree::Batch::new();
        let meta = meta_for(&st, NodeKind::Dir, NodeFlags::EMPTY);
        first
            .dir(key, DirKey::NONE, canonical.as_os_str().as_bytes(), &meta)
            .map_err(|_| ScanError::Unsupported)?;
        ctx.count(meta.allocated, meta.logical, 0, 1);
        ctx.send(first);

        let threads = ctx.options().threads.unwrap_or_else(|| {
            let major = (st.dev >> 32) as u32;
            let minor = st.dev as u32;
            let rotational = threads::rotational_at(Path::new("/sys"), major, minor);
            let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
            threads::auto_threads(rotational, cores)
        });

        // Read once, here, before any worker exists.
        let classifier = ctx.classifier();
        let root_path: Box<[u8]> = if classifier.is_some() {
            canonical.as_os_str().as_bytes().into()
        } else {
            Box::default()
        };
        let shared = Shared {
            boundary,
            classifier,
            variant,
        };
        let roots = vec![Task {
            at: At::Open(Arc::new(fd)),
            key,
            path: root_path,
        }];
        pool::run(ctx, threads, roots, |worker, task| {
            process_dir(&shared, worker, task);
        })
    }
}

struct Shared {
    boundary: Boundary,
    /// `None` when the caller turned categories off.
    classifier: Option<Classifier>,
    #[cfg_attr(not(feature = "iouring-bench"), allow(dead_code))]
    variant: StatVariant,
}

enum At {
    /// The directory is already open (the scan root).
    Open(Arc<OwnedFd>),
    /// The directory is `name` (with its NUL) inside an open parent.
    Child {
        parent: Arc<OwnedFd>,
        name: Box<[u8]>,
    },
}

struct Task {
    at: At,
    key: DirKey,
    /// The directory's full path, for the category lookup. Empty when
    /// nothing is being classified.
    path: Box<[u8]>,
}

fn failure_of(err: &io::Error) -> ReadFailure {
    let code = err.raw_os_error().unwrap_or(0);
    let kind = match code {
        libc::EACCES | libc::EPERM => ReadErrorKind::PermissionDenied,
        // Gone, or replaced by something that is not a directory.
        libc::ENOENT | libc::ENOTDIR | libc::ELOOP => ReadErrorKind::NotFound,
        libc::EIO | libc::ENODEV | libc::ENXIO => ReadErrorKind::Io,
        libc::EUCLEAN | libc::EBADMSG | libc::ESTALE => ReadErrorKind::Corrupt,
        libc::ENAMETOOLONG => ReadErrorKind::TooDeep,
        _ => ReadErrorKind::Other,
    };
    ReadFailure { kind, code }
}

fn kind_of_mode(mode: u32) -> NodeKind {
    match mode & libc::S_IFMT {
        libc::S_IFDIR => NodeKind::Dir,
        libc::S_IFREG => NodeKind::File,
        libc::S_IFLNK => NodeKind::Symlink,
        _ => NodeKind::Other,
    }
}

fn kind_of_dtype(d_type: u8) -> NodeKind {
    match d_type {
        libc::DT_DIR => NodeKind::Dir,
        libc::DT_REG => NodeKind::File,
        libc::DT_LNK => NodeKind::Symlink,
        _ => NodeKind::Other,
    }
}

/// The record data for an examined entry.
fn meta_for(st: &sys::Stat, kind: NodeKind, mut flags: NodeFlags) -> Meta {
    let allocated = st.blocks.saturating_mul(512);
    // A directory's `st_size` describes its entry table, not data held in it,
    // so it adds nothing to the apparent size (the way `du --apparent-size`
    // counts). Its blocks do count toward size on disk.
    let logical = if kind == NodeKind::Dir { 0 } else { st.size };
    let mut link = None;
    if kind != NodeKind::Dir {
        if kind == NodeKind::Symlink {
            flags |= NodeFlags::NOT_FOLLOWED;
        }
        if kind == NodeKind::File && allocated.saturating_add(SPARSE_MARGIN) <= logical {
            flags |= NodeFlags::SPARSE_OR_COMPRESSED;
        }
        if st.nlink > 1 {
            // More than one name: flagged whether or not the others are in
            // the scan; keyed so the tree counts the bytes once.
            flags |= NodeFlags::HARD_LINK;
            link = Some(FileKey {
                volume: st.dev,
                id: u128::from(st.ino),
            });
        }
    }
    Meta {
        logical,
        allocated,
        mtime: st.mtime,
        flags,
        link,
        ..Meta::default()
    }
}

fn process_dir(shared: &Shared, worker: &mut Worker<'_, '_, Task>, task: Task) {
    let Task { at, key, path } = task;
    let dir: Arc<OwnedFd> = match at {
        At::Open(fd) => fd,
        At::Child { parent, name } => {
            run_before_open_hook(&name);
            let opened = match CStr::from_bytes_with_nul(&name) {
                Ok(c) => sys::open_dir_at(parent.as_fd(), c).map_err(|e| failure_of(&e)),
                Err(_) => Err(failure_of(&io::Error::from(io::ErrorKind::InvalidInput))),
            };
            // The parent's handle is no longer needed by this task.
            drop(parent);
            match opened.and_then(|fd| check_opened(shared, fd)) {
                Ok(fd) => Arc::new(fd),
                Err(failure) => {
                    worker.batch().unreadable(key, failure);
                    worker.tally.unreadable += 1;
                    return;
                }
            }
        }
    };

    let mut buf = READ_BUF.take();
    if buf.len() < READ_BUFFER {
        buf.resize(READ_BUFFER, 0);
    }
    let here = Here {
        place: shared
            .classifier
            .as_ref()
            .and_then(|classifier| classifier.place(&path)),
        path: &path,
    };
    let outcome = list_dir(shared, worker, &dir, key, &here, &mut buf);
    READ_BUF.set(buf);

    match outcome {
        Listing::Complete => worker.batch().listing_done(key),
        Listing::Failed(failure) => {
            worker.batch().unreadable(key, failure);
            worker.tally.unreadable += 1;
        }
        // Cancelled: leave the directory unfinished so it stays incomplete.
        Listing::Cancelled => {}
    }
}

/// Checks what was actually opened. The entry was examined by name, and a
/// mount can be placed on a directory between that and the open; the opened
/// directory must still be inside the boundary.
fn check_opened(shared: &Shared, fd: OwnedFd) -> Result<OwnedFd, ReadFailure> {
    let st = sys::stat_fd(fd.as_fd()).map_err(|e| failure_of(&e))?;
    let inside = shared.boundary.allows(Placement {
        mount_id: st.mnt_id,
        device: st.dev,
    });
    if inside && kind_of_mode(st.mode) == NodeKind::Dir {
        Ok(fd)
    } else {
        // Not what was listed any more.
        Err(ReadFailure {
            kind: ReadErrorKind::NotFound,
            code: 0,
        })
    }
}

enum Listing {
    Complete,
    Failed(ReadFailure),
    Cancelled,
}

/// Lists `dir`. The buffer must be all zeros on entry and is all zeros on
/// return: the kernel leaves the padding after each name untouched, and the
/// parser relies on it being zero to notice names that contain a NUL.
fn list_dir(
    shared: &Shared,
    worker: &mut Worker<'_, '_, Task>,
    dir: &Arc<OwnedFd>,
    key: DirKey,
    here: &Here<'_>,
    buf: &mut [u8],
) -> Listing {
    let mut used = 0usize;
    let outcome = list_dir_into(shared, worker, dir, key, here, buf, &mut used);
    if let Some(dirty) = buf.get_mut(..used) {
        dirty.fill(0);
    }
    outcome
}

fn list_dir_into(
    shared: &Shared,
    worker: &mut Worker<'_, '_, Task>,
    dir: &Arc<OwnedFd>,
    key: DirKey,
    here: &Here<'_>,
    buf: &mut [u8],
    used: &mut usize,
) -> Listing {
    #[cfg(feature = "iouring-bench")]
    if let Some(config) = shared.variant {
        return uring::list_dir_into(shared, config, worker, dir, key, here, buf, used);
    }
    list_dir_plain(shared, worker, dir, key, here, buf, used)
}

/// The listing loop: read a buffer of entries, examine each with its own
/// `statx`.
fn list_dir_plain(
    shared: &Shared,
    worker: &mut Worker<'_, '_, Task>,
    dir: &Arc<OwnedFd>,
    key: DirKey,
    here: &Here<'_>,
    buf: &mut [u8],
    used: &mut usize,
) -> Listing {
    let mut seen = 0usize;
    loop {
        if worker.cancelled() {
            return Listing::Cancelled;
        }
        if let Some(dirty) = buf.get_mut(..*used) {
            dirty.fill(0);
        }
        *used = 0;
        let filled = match sys::getdents(dir.as_fd(), buf) {
            Ok(0) => return Listing::Complete,
            Ok(n) => n,
            Err(e) => return Listing::Failed(failure_of(&e)),
        };
        *used = filled;
        for item in dirent::entries(&buf[..filled]) {
            let Ok(entry) = item else {
                // The filesystem handed back a damaged listing.
                return Listing::Failed(ReadFailure {
                    kind: ReadErrorKind::Corrupt,
                    code: 0,
                });
            };
            if entry.is_dot_or_dotdot() {
                continue;
            }
            handle_entry(shared, worker, dir, key, here, &entry);
            seen += 1;
            if seen.is_multiple_of(FLUSH_CHECK_EVERY) {
                worker.flush_if_due();
                if worker.cancelled() {
                    return Listing::Cancelled;
                }
            }
        }
    }
}

fn handle_entry(
    shared: &Shared,
    worker: &mut Worker<'_, '_, Task>,
    dir: &Arc<OwnedFd>,
    parent_key: DirKey,
    here: &Here<'_>,
    entry: &dirent::RawEntry<'_>,
) {
    handle_entry_with(shared, worker, dir, parent_key, here, entry, |name_c| {
        sys::stat_at(dir.as_fd(), name_c)
    });
}

/// Records one listed entry. `examine` says what `statx` makes of the name;
/// it is called at most once, and not at all for names that cannot be
/// examined.
fn handle_entry_with(
    shared: &Shared,
    worker: &mut Worker<'_, '_, Task>,
    dir: &Arc<OwnedFd>,
    parent_key: DirKey,
    here: &Here<'_>,
    entry: &dirent::RawEntry<'_>,
    examine: impl FnOnce(&CStr) -> io::Result<sys::Stat>,
) {
    let Ok(name_c) = CStr::from_bytes_with_nul(entry.name_with_nul) else {
        return;
    };
    let name = entry.name();
    if !entry.padding_is_clean {
        // Bytes after the name's NUL: the name holds a NUL of its own, so
        // the part we can name is a prefix that may be another entry's
        // name. Keep the entry, flagged, and do not examine it.
        let meta = Meta {
            failure: Some(ReadFailure {
                kind: ReadErrorKind::Corrupt,
                code: 0,
            }),
            ..Meta::default()
        };
        worker.tally.unreadable += 1;
        add_entry(worker, NodeKind::Other, parent_key, name, &meta);
        return;
    }
    let st = match examine(name_c) {
        Ok(st) => st,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            worker.tally.vanished += 1;
            return;
        }
        Err(e) => {
            // Listed but not examinable: keep it, flagged, with the kind the
            // listing gave.
            let kind = kind_of_dtype(entry.d_type);
            let meta = Meta {
                failure: Some(failure_of(&e)),
                ..Meta::default()
            };
            worker.tally.unreadable += 1;
            add_entry(worker, kind, parent_key, name, &meta);
            return;
        }
    };

    let kind = kind_of_mode(st.mode);
    if kind != NodeKind::Dir {
        let mut meta = meta_for(&st, kind, NodeFlags::EMPTY);
        if let Some(classifier) = &shared.classifier {
            meta.category = classifier.category(here.place, kind, name);
        }
        worker.tally.files += 1;
        worker.tally.add_bytes(meta.allocated, meta.logical);
        worker.batch().file(kind, parent_key, name, &meta).ok();
        return;
    }

    let inside = shared.boundary.allows(Placement {
        mount_id: st.mnt_id,
        device: st.dev,
    });
    let Some(child_key) = worker.ctx().next_dir_key() else {
        // Out of directory keys: it cannot be described as a directory, so say
        // plainly that it was not read.
        let meta = Meta {
            failure: Some(ReadFailure {
                kind: ReadErrorKind::Other,
                code: 0,
            }),
            ..meta_for(&st, NodeKind::Other, NodeFlags::NOT_FOLLOWED)
        };
        worker.tally.unreadable += 1;
        worker
            .batch()
            .file(NodeKind::Other, parent_key, name, &meta)
            .ok();
        return;
    };
    worker.tally.dirs += 1;
    if inside {
        let meta = meta_for(&st, NodeKind::Dir, NodeFlags::EMPTY);
        worker.tally.add_bytes(meta.allocated, meta.logical);
        worker.batch().dir(child_key, parent_key, name, &meta).ok();
        let path = if shared.classifier.is_some() {
            join(here.path, name, b'/')
        } else {
            Box::default()
        };
        worker.spawn(Task {
            at: At::Child {
                parent: Arc::clone(dir),
                name: entry.name_with_nul.into(),
            },
            key: child_key,
            path,
        });
    } else {
        // Another drive's mount. The numbers describe its root, not an entry
        // of this filesystem, so none are counted.
        let meta = Meta {
            mtime: st.mtime,
            flags: NodeFlags::NOT_FOLLOWED,
            ..Meta::default()
        };
        let batch = worker.batch();
        batch.dir(child_key, parent_key, name, &meta).ok();
        batch.listing_done(child_key);
    }
}

/// Adds an entry that could not be examined. A directory needs a key; if none
/// is left it is recorded as a plain unreadable entry.
fn add_entry(
    worker: &mut Worker<'_, '_, Task>,
    kind: NodeKind,
    parent_key: DirKey,
    name: &[u8],
    meta: &Meta,
) {
    if kind == NodeKind::Dir {
        if let Some(child_key) = worker.ctx().next_dir_key() {
            worker.tally.dirs += 1;
            worker.batch().dir(child_key, parent_key, name, meta).ok();
            return;
        }
        worker
            .batch()
            .file(NodeKind::Other, parent_key, name, meta)
            .ok();
        return;
    }
    worker.tally.files += 1;
    worker.batch().file(kind, parent_key, name, meta).ok();
}
