//! The Windows walker: `GetFileInformationByHandleEx` on open directory
//! handles, on the same work-stealing pool as the Linux walker.
//!
//! The extended directory-information class returns, for every entry in one
//! call and without opening it: size, allocated size, attributes, reparse tag
//! and the 128-bit file ID. Filesystems that reject it get the older class
//! (64-bit IDs), and those that reject that get `FindFirstFileExW`, which
//! reports neither allocation nor IDs; allocated sizes are then estimated and
//! counted in [`crate::ScanProgress::estimated`], and hard links cannot be
//! detected.
//!
//! # What is guaranteed about links
//!
//! * **Descent never resolves a path.** Every directory below the scan root
//!   is opened relative to the handle of the directory it was listed in, by
//!   its one name (`NtCreateFile` with a root directory), and the handle is
//!   held for as long as its children are pending. A directory above the
//!   target that is swapped for a junction or link after listing therefore
//!   changes nothing: the scan keeps walking the original, because it holds
//!   the original. A target that is swapped between listing and opening is
//!   opened as the link itself (`FILE_OPEN_REPARSE_POINT`), recognised by its
//!   attributes, and recorded as unreadable instead of entered.
//! * **Every opened directory is checked** against the root's volume and for
//!   the reparse-point attribute, so a mount point or junction that slipped
//!   in is refused even if the open succeeded.
//! * **The scan root** is the one path the walker resolves, once, at the
//!   start: it is the user's own choice and links in it are followed to where
//!   it really is.
//! * **The one exception to "no path" is the last-resort listing.** On a
//!   filesystem that rejects both handle-based listing classes,
//!   `FindFirstFileExW` needs a path. The walker takes the path from the
//!   handle it holds (so it names the object it holds as of that moment),
//!   lists it, and then checks that the path still names that object. A swap
//!   in that short window could make the listing come from another directory,
//!   so its names and sizes could be counted under the wrong parent; it
//!   still cannot make the walker enter anything, because children are opened
//!   relative to the held handle by name, as always. The sizes counted that
//!   way are estimates in any case (see above).
//!
//! # Other rules
//!
//! * A reparse point (symbolic link, junction, mount point, anything else) is
//!   never entered. Its entry is recorded flagged
//!   [`NodeFlags::NOT_FOLLOWED`]. A mount point is a reparse point, so no other
//!   volume is ever entered; each opened directory's volume serial is checked
//!   against the root's as well.
//! * A cloud placeholder (`RECALL_ON_DATA_ACCESS`, `RECALL_ON_OPEN`, `OFFLINE`,
//!   or a cloud reparse tag) is never opened, and a placeholder directory is
//!   not listed, since listing it can download its contents. Files are counted
//!   from the directory information only; a placeholder directory is recorded
//!   as unread, so its parents show a lower bound.
//! * A directory that cannot be opened or listed is recorded as unreadable
//!   with the error kind, and the scan carries on.
//! * A listed name that is not a single path component (it holds a NUL or a
//!   separator) is recorded as damaged and never opened.
//! * Every file with a file ID is keyed so the tree can count hard links once
//!   (the listing has no link count). Names are converted from UTF-16 to WTF-8
//!   without loss.

pub(crate) mod sys;

use crate::OpenHook;
use crate::pool::{self, Worker};
use crate::scanner::{ScanContext, ScanEnd, ScanError, Scanner};
use crate::threads;
use crate::wininfo::{self, EntryClass, Layout};
use bs_tree::{DirKey, FileKey, Meta, NodeFlags, NodeKind, ReadErrorKind, ReadFailure};
use std::io;
use std::os::windows::io::OwnedHandle;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use windows_sys::Win32::Foundation as f;

/// The directory walker for Windows.
#[derive(Clone, Copy, Debug, Default)]
pub struct Walker;

impl Walker {
    /// Scans `root` to the end and returns the result. A convenience over
    /// [`crate::start`].
    pub fn scan_blocking(root: &Path, options: crate::ScanOptions) -> crate::ScanResult {
        crate::start(Arc::new(Walker), root.to_path_buf(), options).wait()
    }
}

/// Bytes read from a directory per call.
const LIST_BUFFER: usize = 64 * 1024;
/// A file smaller than its logical size by at least this much is sparse or
/// compressed.
const SPARSE_MARGIN: u64 = 4096;
/// Cluster size assumed when a listing gives no allocation size.
const ESTIMATE_UNIT: u64 = 4096;

static OPEN_HOOK_ON: AtomicBool = AtomicBool::new(false);
static OPEN_HOOK: std::sync::Mutex<Option<OpenHook>> = std::sync::Mutex::new(None);

/// Installs (or removes) the hook. For tests only.
pub(crate) fn set_before_open_hook(hook: Option<OpenHook>) {
    OPEN_HOOK_ON.store(hook.is_some(), Ordering::SeqCst);
    // A hook that panicked poisons the lock; the slot is still usable.
    let mut slot = OPEN_HOOK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *slot = hook;
}

/// Calls the hook, if one is installed, with the name as WTF-8.
fn run_before_open_hook(name: &[u16]) {
    if !OPEN_HOOK_ON.load(Ordering::Relaxed) {
        return;
    }
    let slot = OPEN_HOOK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(hook) = slot.as_ref() {
        let mut bytes = Vec::new();
        wininfo::wtf8_from_utf16(name.iter().copied(), &mut bytes);
        hook(&bytes);
    }
}

enum At {
    /// The directory is already open (the scan root).
    Open(Arc<OwnedHandle>),
    /// The directory is `name` inside the open directory `parent`.
    Child {
        parent: Arc<OwnedHandle>,
        name: Box<[u16]>,
    },
}

struct Task {
    at: At,
    key: DirKey,
}

struct Shared {
    volume_serial: u64,
}

impl Scanner for Walker {
    fn name(&self) -> &'static str {
        "windows-walker"
    }

    fn scan(&self, root: &Path, ctx: &ScanContext) -> Result<ScanEnd, ScanError> {
        // The root is the user's own choice, so links in it are resolved once
        // here. Nothing below it is ever followed.
        let canonical = std::fs::canonicalize(root).map_err(ScanError::Root)?;
        let mut path = sys::wide(&canonical);
        path.pop();
        let mut with_nul = path.clone();
        with_nul.push(0);
        let handle = sys::open_dir(&with_nul).map_err(ScanError::Root)?;
        let info = sys::handle_info(&handle).map_err(ScanError::Root)?;
        if info.attributes & wininfo::attr::DIRECTORY == 0 {
            return Err(ScanError::NotADirectory(canonical));
        }
        let key = ctx.next_dir_key().ok_or(ScanError::Unsupported)?;

        let mut first = bs_tree::Batch::new();
        let mut name = Vec::new();
        wininfo::wtf8_from_utf16(path.iter().copied(), &mut name);
        let meta = Meta {
            mtime: wininfo::filetime_to_unix(info.last_write_ticks),
            ..Meta::default()
        };
        first
            .dir(key, DirKey::NONE, &name, &meta)
            .map_err(|_| ScanError::Unsupported)?;
        ctx.count(0, 0, 0, 1);
        ctx.send(first);

        let threads = ctx.options().threads.unwrap_or_else(|| {
            // The storage type is not queried on Windows yet: assume flash.
            let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
            threads::auto_threads(None, cores)
        });
        let shared = Shared {
            volume_serial: info.volume_serial,
        };
        // The root's handle is the one the walk starts from; it is not
        // opened a second time by path.
        let roots = vec![Task {
            at: At::Open(Arc::new(handle)),
            key,
        }];
        pool::run(ctx, threads, roots, |worker, task| {
            process_dir(&shared, worker, task);
        })
    }
}

fn failure_of(err: &io::Error) -> ReadFailure {
    let code = err.raw_os_error().unwrap_or(0);
    let kind = match code as u32 {
        f::ERROR_ACCESS_DENIED | f::ERROR_SHARING_VIOLATION => ReadErrorKind::PermissionDenied,
        f::ERROR_FILE_NOT_FOUND | f::ERROR_PATH_NOT_FOUND | f::ERROR_DIRECTORY => {
            ReadErrorKind::NotFound
        }
        f::ERROR_FILENAME_EXCED_RANGE | f::ERROR_BAD_PATHNAME => ReadErrorKind::TooDeep,
        // Disk errors, bad blocks, a damaged volume.
        23 | 1117 | 1392 | 1393 => ReadErrorKind::Corrupt,
        _ => ReadErrorKind::Other,
    };
    ReadFailure { kind, code }
}

/// What the walker needs from one listed entry, whichever call produced it.
struct Item {
    name: Vec<u8>,
    name_units: Vec<u16>,
    attributes: u32,
    reparse_tag: u32,
    logical: u64,
    allocated: u64,
    estimated: bool,
    mtime: i64,
    file_id: Option<u128>,
}

fn is_dot(units: &[u16]) -> bool {
    matches!(units, [0x2e] | [0x2e, 0x2e])
}

/// What a directory that was opened but is not what was listed is recorded
/// as: it changed since the listing.
fn changed_since_listed() -> ReadFailure {
    ReadFailure {
        kind: ReadErrorKind::NotFound,
        code: 0,
    }
}

fn process_dir(shared: &Shared, worker: &mut Worker<'_, '_, Task>, task: Task) {
    let Task { at, key } = task;
    let handle: Arc<OwnedHandle> = match at {
        At::Open(handle) => handle,
        At::Child { parent, name } => {
            run_before_open_hook(&name);
            let opened = sys::open_dir_at(&parent, &name);
            // The parent's handle is no longer needed by this task.
            drop(parent);
            match opened {
                Ok(handle) => Arc::new(handle),
                Err(e) => {
                    worker.batch().unreadable(key, failure_of(&e));
                    worker.tally.unreadable += 1;
                    return;
                }
            }
        }
    };
    // What was opened must be an ordinary directory of the scanned volume. A
    // directory that became a link since it was listed is opened as the link
    // itself, and is refused here.
    match sys::handle_info(&handle) {
        Ok(info)
            if info.attributes & wininfo::attr::DIRECTORY != 0
                && info.attributes & wininfo::attr::REPARSE_POINT == 0
                && info.volume_serial == shared.volume_serial => {}
        Ok(_) => {
            worker.batch().unreadable(key, changed_since_listed());
            worker.tally.unreadable += 1;
            return;
        }
        Err(e) => {
            worker.batch().unreadable(key, failure_of(&e));
            worker.tally.unreadable += 1;
            return;
        }
    }

    match list_dir(shared, worker, &handle, key) {
        Listing::Complete => worker.batch().listing_done(key),
        Listing::Failed(failure) => {
            worker.batch().unreadable(key, failure);
            worker.tally.unreadable += 1;
        }
        Listing::Cancelled => {}
    }
}

enum Listing {
    Complete,
    Failed(ReadFailure),
    Cancelled,
}

fn list_dir(
    shared: &Shared,
    worker: &mut Worker<'_, '_, Task>,
    handle: &Arc<OwnedHandle>,
    key: DirKey,
) -> Listing {
    let mut buf = sys::ListBuffer::new(LIST_BUFFER);
    let mut layout = Layout::Extended;
    let mut restart = true;
    let mut seen = 0usize;
    loop {
        if worker.cancelled() {
            return Listing::Cancelled;
        }
        match sys::list(handle, layout, restart, &mut buf) {
            sys::Listed::End => return Listing::Complete,
            sys::Listed::Failed(e) => return Listing::Failed(failure_of(&e)),
            sys::Listed::Unsupported => match layout {
                Layout::Extended => {
                    layout = Layout::Both;
                    continue;
                }
                Layout::Both => return list_with_find(shared, worker, handle, key),
            },
            sys::Listed::Records => {}
        }
        restart = false;
        let bytes = buf.bytes();
        for record in wininfo::records(bytes, layout) {
            let Ok(rec) = record else {
                return Listing::Failed(ReadFailure {
                    kind: ReadErrorKind::Corrupt,
                    code: 0,
                });
            };
            let units: Vec<u16> = wininfo::name_units(bytes, &rec).collect();
            if is_dot(&units) {
                continue;
            }
            let mut name = Vec::with_capacity(units.len());
            wininfo::wtf8_from_utf16(units.iter().copied(), &mut name);
            let item = Item {
                name,
                name_units: units,
                attributes: rec.attributes,
                reparse_tag: rec.reparse_tag,
                logical: rec.logical,
                allocated: rec.allocated,
                estimated: false,
                mtime: rec.mtime,
                file_id: rec.file_id,
            };
            handle_item(shared, worker, handle, key, item);
            seen += 1;
            if seen.is_multiple_of(256) {
                worker.flush_if_due();
            }
        }
    }
}

/// The last resort for filesystems that reject both handle-based classes.
///
/// `FindFirstFileExW` takes a path. It is taken from the handle (so it names
/// the object this walker holds), the listing is collected in full, and only
/// then, once the path is confirmed to still name the held object, are the
/// entries used. See the module documentation for what a swap in between can
/// and cannot do.
fn list_with_find(
    shared: &Shared,
    worker: &mut Worker<'_, '_, Task>,
    handle: &Arc<OwnedHandle>,
    key: DirKey,
) -> Listing {
    let path = match sys::final_path(handle) {
        Ok(path) => path,
        Err(e) => return Listing::Failed(failure_of(&e)),
    };
    let found = match sys::find_all(&path) {
        Ok(found) => found,
        Err(e) => return Listing::Failed(failure_of(&e)),
    };
    if !path_still_names(&path, handle) {
        // The listing may be of something else; use none of it.
        return Listing::Failed(changed_since_listed());
    }
    for entry in found {
        if is_dot(&entry.name) {
            continue;
        }
        let mut name = Vec::with_capacity(entry.name.len());
        wininfo::wtf8_from_utf16(entry.name.iter().copied(), &mut name);
        let item = Item {
            name,
            name_units: entry.name,
            attributes: entry.attributes,
            reparse_tag: entry.reparse_tag,
            logical: entry.logical,
            allocated: entry
                .logical
                .div_ceil(ESTIMATE_UNIT)
                .saturating_mul(ESTIMATE_UNIT),
            estimated: true,
            mtime: wininfo::filetime_to_unix(entry.last_write_ticks),
            file_id: None,
        };
        handle_item(shared, worker, handle, key, item);
        if worker.cancelled() {
            return Listing::Cancelled;
        }
    }
    Listing::Complete
}

/// Whether `path`, opened afresh without following a final link, is the same
/// object as the held `handle` (same volume and file index, and not a link).
fn path_still_names(path: &[u16], handle: &OwnedHandle) -> bool {
    let mut with_nul = path.to_vec();
    with_nul.push(0);
    let Ok(again) = sys::open_dir(&with_nul) else {
        return false;
    };
    let (Ok(now), Ok(held)) = (sys::handle_info(&again), sys::handle_info(handle)) else {
        return false;
    };
    now.attributes & wininfo::attr::REPARSE_POINT == 0
        && now.volume_serial == held.volume_serial
        && now.file_index == held.file_index
}

fn handle_item(
    shared: &Shared,
    worker: &mut Worker<'_, '_, Task>,
    dir: &Arc<OwnedHandle>,
    parent_key: DirKey,
    item: Item,
) {
    if !wininfo::is_plain_component(&item.name_units) {
        // Opening this by name could reach somewhere else, and no real
        // directory listing holds such a name: keep it as damaged.
        let meta = Meta {
            failure: Some(ReadFailure {
                kind: ReadErrorKind::Corrupt,
                code: 0,
            }),
            ..Meta::default()
        };
        worker.tally.unreadable += 1;
        worker.tally.files += 1;
        worker
            .batch()
            .file(NodeKind::Other, parent_key, &item.name, &meta)
            .ok();
        return;
    }
    let class = wininfo::classify(item.attributes, item.reparse_tag);
    let flags = wininfo::entry_flags(&class, item.estimated);
    let mut meta = Meta {
        logical: item.logical,
        allocated: item.allocated,
        mtime: item.mtime,
        flags,
        ..Meta::default()
    };
    if item.estimated {
        worker.ctx().count_estimated(1);
    }

    match class.class {
        EntryClass::Dir => {
            let Some(child_key) = worker.ctx().next_dir_key() else {
                meta.failure = Some(ReadFailure {
                    kind: ReadErrorKind::Other,
                    code: 0,
                });
                worker.tally.unreadable += 1;
                worker
                    .batch()
                    .file(NodeKind::Other, parent_key, &item.name, &meta)
                    .ok();
                return;
            };
            worker.tally.dirs += 1;
            worker.tally.add_bytes(meta.allocated, meta.logical);
            if class.descend {
                worker
                    .batch()
                    .dir(child_key, parent_key, &item.name, &meta)
                    .ok();
                worker.spawn(Task {
                    at: At::Child {
                        parent: Arc::clone(dir),
                        name: item.name_units.into_boxed_slice(),
                    },
                    key: child_key,
                });
            } else {
                // A placeholder folder: not listed, so what is in it is not
                // known. Recorded as unread.
                meta.failure = Some(ReadFailure {
                    kind: ReadErrorKind::Other,
                    code: 0,
                });
                worker.tally.unreadable += 1;
                worker
                    .batch()
                    .dir(child_key, parent_key, &item.name, &meta)
                    .ok();
            }
        }
        EntryClass::File | EntryClass::Symlink | EntryClass::Other => {
            let kind = match class.class {
                EntryClass::Symlink => NodeKind::Symlink,
                EntryClass::Other => NodeKind::Other,
                _ => NodeKind::File,
            };
            if kind == NodeKind::File
                && !class.placeholder
                && (item.attributes & (wininfo::attr::SPARSE_FILE | wininfo::attr::COMPRESSED) != 0
                    || meta.allocated.saturating_add(SPARSE_MARGIN) <= meta.logical)
            {
                meta.flags |= NodeFlags::SPARSE_OR_COMPRESSED;
            }
            if let Some(id) = item.file_id {
                meta.link = Some(FileKey {
                    volume: shared.volume_serial,
                    id,
                });
            }
            worker.tally.files += 1;
            worker.tally.add_bytes(meta.allocated, meta.logical);
            worker
                .batch()
                .file(kind, parent_key, &item.name, &meta)
                .ok();
        }
    }
}
