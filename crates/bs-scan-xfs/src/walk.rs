//! The directory-only walk: every directory is listed once, by several
//! threads, and what each listing held is kept for the emission to join with
//! the inode table.
//!
//! Directories are opened relative to their parent without following links,
//! so a path that changes under the walk cannot lead it somewhere else, and a
//! directory on another mount is recognized when it is opened and not entered.

use crate::XfsError;
use crate::bulkstat::BulkStat;
use crate::error::{Diagnostics, ProblemKind};
use crate::names::{EntryStat, Names, OpenedDir, RawName};
use bs_scan::{Boundary, Placement};
use bs_tree::{ReadErrorKind, ReadFailure};
use std::io;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};

pub(crate) const NONE: u32 = u32::MAX;
/// Directories nested deeper than this are listed as unreadable.
pub(crate) const MAX_DEPTH: u32 = 65_536;

const DT_UNKNOWN: u8 = 0;
const DT_DIR: u8 = 4;

/// What the inode table holds of one inode.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Stat {
    pub ino: u64,
    pub size: u64,
    pub allocated: u64,
    pub mtime: i64,
    pub nlink: u32,
    pub mode: u32,
}

impl From<BulkStat> for Stat {
    fn from(b: BulkStat) -> Stat {
        Stat {
            ino: b.ino,
            size: b.size,
            allocated: b.allocated(),
            mtime: b.mtime,
            nlink: b.nlink,
            mode: u32::from(b.mode),
        }
    }
}

impl From<EntryStat> for Stat {
    fn from(e: EntryStat) -> Stat {
        Stat {
            ino: e.ino,
            size: e.size,
            allocated: e.allocated,
            mtime: e.mtime,
            nlink: e.nlink,
            mode: e.mode,
        }
    }
}

/// The inodes of the file system, by number.
pub(crate) struct Table {
    pub rows: Vec<Stat>,
}

impl Table {
    pub fn find(&self, ino: u64) -> Option<u32> {
        let at = self.rows.binary_search_by_key(&ino, |s| s.ino).ok()?;
        u32::try_from(at).ok()
    }

    pub fn get(&self, at: u32) -> Option<&Stat> {
        self.rows.get(usize::try_from(at).ok()?)
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ListedEntry {
    pub name_off: u32,
    pub name_len: u16,
    pub d_type: u8,
    pub clean: bool,
    pub ino: u64,
    /// The inode's row in the table, or `NONE`.
    pub row: u32,
    /// The listing of the directory this entry was walked into, or `NONE`.
    pub child: u32,
}

/// How a directory turned out when the walk came to it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Status {
    /// It was listed (possibly cut short by a failure, see `failure`).
    Listed,
    /// It is a mount the scan does not enter (another file system, or one
    /// the caller left out): not entered.
    Outside,
    /// It is no longer the directory that was listed: replaced by a file or a
    /// link, or gone. Treated as an ordinary entry.
    Replaced,
    /// It was gone when the walk came to it.
    Gone,
    /// Another entry already led to this directory (a loop, or an inode
    /// number listed twice): it is listed once, at the first.
    Duplicate,
    /// It could not be opened.
    Failed(ReadFailure),
}

pub(crate) struct Listing {
    pub status: Status,
    pub entries: Vec<ListedEntry>,
    pub names: Vec<u8>,
    /// What `statx` said about entries the table did not hold: `(index of the
    /// entry, its numbers)`, ascending.
    pub extras: Vec<(u32, Stat)>,
    /// The listing failed partway; the entries before it are kept.
    pub failure: Option<ReadFailure>,
    /// For a mount of this file system that was entered: the numbers of the
    /// mounted directory itself. The entry that led to it carries the
    /// covered directory's inode, which is a different one.
    pub mounted: Option<Stat>,
}

impl Listing {
    fn empty(status: Status) -> Listing {
        Listing {
            status,
            entries: Vec::new(),
            names: Vec::new(),
            extras: Vec::new(),
            failure: None,
            mounted: None,
        }
    }

    pub fn name(&self, e: &ListedEntry) -> &[u8] {
        let start = usize::try_from(e.name_off).unwrap_or(usize::MAX);
        let end = start.saturating_add(usize::from(e.name_len));
        self.names.get(start..end).unwrap_or(&[])
    }

    pub fn extra(&self, index: usize) -> Option<&Stat> {
        let at = u32::try_from(index).ok()?;
        let i = self.extras.binary_search_by_key(&at, |(k, _)| *k).ok()?;
        self.extras.get(i).map(|(_, s)| s)
    }
}

pub(crate) fn failure_of(err: &io::Error) -> ReadFailure {
    let code = err.raw_os_error().unwrap_or(0);
    let kind = match code {
        libc::EACCES | libc::EPERM => ReadErrorKind::PermissionDenied,
        libc::ENOENT | libc::ENOTDIR | libc::ELOOP => ReadErrorKind::NotFound,
        libc::EIO | libc::ENODEV | libc::ENXIO => ReadErrorKind::Io,
        libc::EUCLEAN | libc::EBADMSG | libc::ESTALE => ReadErrorKind::Corrupt,
        libc::ENAMETOOLONG => ReadErrorKind::TooDeep,
        _ if err.kind() == io::ErrorKind::InvalidData => ReadErrorKind::Corrupt,
        _ => ReadErrorKind::Other,
    };
    ReadFailure { kind, code }
}

struct Task<D> {
    id: u32,
    /// `None` for the root, which is already open.
    parent: Option<Arc<D>>,
    name: Vec<u8>,
    expect_ino: u64,
    /// Where the directory it was found in lives.
    from: Placement,
    depth: u32,
}

struct State<D> {
    stack: Vec<Task<D>>,
    pending: usize,
    listings: Vec<Option<Listing>>,
}

/// What the walk shares between its threads.
struct Shared<'a, N: Names> {
    names: &'a N,
    boundary: &'a Boundary,
    /// The device of the scan root: the file system the inode table is of.
    device: u64,
    root_placement: Placement,
    table: &'a Table,
    next_id: AtomicU32,
    used: AtomicU64,
    limit: u64,
    over: std::sync::atomic::AtomicBool,
    out_of_ids: std::sync::atomic::AtomicBool,
    /// The directories opened so far, by inode number.
    visited: Mutex<std::collections::HashSet<u64>>,
    state: Mutex<State<N::Dir>>,
    wake: Condvar,
    cancelled: &'a (dyn Fn() -> bool + Sync),
    diag: Mutex<Diagnostics>,
}

/// What the walk produced.
pub(crate) struct Walked {
    /// Listing 0 is the root's.
    pub listings: Vec<Listing>,
    pub root: EntryStat,
    pub device: u64,
    pub fallback_stats: u64,
    pub diag: Diagnostics,
    pub memory: u64,
}

/// Memory one listed entry takes, with its share of the vectors' slack.
fn entry_cost(name_len: usize) -> u64 {
    (std::mem::size_of::<ListedEntry>() as u64)
        .saturating_add(name_len as u64)
        .saturating_mul(2)
}

/// Walks the tree below the root of `names`. `Ok(None)` when cancelled.
pub(crate) fn walk<N: Names>(
    names: &N,
    boundary: &Boundary,
    table: &Table,
    threads: usize,
    limit: u64,
    charged: u64,
    cancelled: &(dyn Fn() -> bool + Sync),
) -> Result<Option<Walked>, XfsError> {
    let root = names.root()?;
    let boundary = boundary.including(root.placement);
    let OpenedDir {
        dir,
        stat,
        device,
        placement: root_placement,
    } = root;
    let shared = Shared {
        names,
        boundary: &boundary,
        device,
        root_placement,
        table,
        next_id: AtomicU32::new(1),
        used: AtomicU64::new(charged),
        limit,
        over: std::sync::atomic::AtomicBool::new(false),
        out_of_ids: std::sync::atomic::AtomicBool::new(false),
        visited: Mutex::new(std::collections::HashSet::from([stat.ino])),
        state: Mutex::new(State {
            stack: Vec::new(),
            pending: 1,
            listings: Vec::new(),
        }),
        wake: Condvar::new(),
        cancelled,
        diag: Mutex::new(Diagnostics::default()),
    };
    let root_dir = Arc::new(dir);
    let fallback = AtomicU64::new(0);
    let threads = threads.max(1);
    std::thread::scope(|scope| {
        for worker in 0..threads {
            let shared = &shared;
            let root_dir = &root_dir;
            let fallback = &fallback;
            scope.spawn(move || {
                // The root is taken by the first worker.
                let first = (worker == 0).then(|| Task {
                    id: 0,
                    parent: None,
                    name: Vec::new(),
                    expect_ino: stat.ino,
                    from: root_placement,
                    depth: 0,
                });
                run_worker(shared, root_dir, first, fallback);
            });
        }
    });
    if (shared.cancelled)() {
        return Ok(None);
    }
    if shared.over.load(Ordering::Relaxed) {
        return Err(XfsError::TooLarge(
            "the directory listings need more memory than the limit allows",
        ));
    }
    if shared.out_of_ids.load(Ordering::Relaxed) {
        return Err(XfsError::TooLarge("more directories than can be numbered"));
    }
    let state = shared
        .state
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut listings = Vec::with_capacity(state.listings.len());
    for l in state.listings {
        // A hole means a task never finished: only after a panic, which the
        // scope would have propagated. Kept as an unreadable directory.
        listings.push(l.unwrap_or_else(|| {
            Listing::empty(Status::Failed(failure_of(&io::Error::from(
                io::ErrorKind::Other,
            ))))
        }));
    }
    let diag = shared
        .diag
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    Ok(Some(Walked {
        listings,
        root: stat,
        device,
        fallback_stats: fallback.load(Ordering::Relaxed),
        diag,
        memory: shared.used.load(Ordering::Relaxed),
    }))
}

fn run_worker<N: Names>(
    shared: &Shared<'_, N>,
    root_dir: &Arc<N::Dir>,
    first: Option<Task<N::Dir>>,
    fallback: &AtomicU64,
) {
    let mut next = first;
    loop {
        let task = match next.take() {
            Some(t) => t,
            None => {
                let mut state = shared
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                loop {
                    if (shared.cancelled)()
                        || shared.over.load(Ordering::Relaxed)
                        || shared.out_of_ids.load(Ordering::Relaxed)
                    {
                        shared.wake.notify_all();
                        return;
                    }
                    if let Some(t) = state.stack.pop() {
                        break t;
                    }
                    if state.pending == 0 {
                        shared.wake.notify_all();
                        return;
                    }
                    state = shared
                        .wake
                        .wait_timeout(state, std::time::Duration::from_millis(50))
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .0;
                }
            }
        };
        let id = task.id;
        let (listing, children) = process(shared, root_dir, task, fallback);
        let mut state = shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let at = usize::try_from(id).unwrap_or(usize::MAX);
        if state.listings.len() <= at {
            state.listings.resize_with(at.saturating_add(1), || None);
        }
        if let Some(slot) = state.listings.get_mut(at) {
            *slot = Some(listing);
        }
        state.pending = state.pending.saturating_add(children.len());
        state.pending = state.pending.saturating_sub(1);
        state.stack.extend(children);
        shared.wake.notify_all();
    }
}

fn charge<N: Names>(shared: &Shared<'_, N>, bytes: u64) -> bool {
    let now = shared
        .used
        .fetch_add(bytes, Ordering::Relaxed)
        .saturating_add(bytes);
    if now > shared.limit {
        shared.over.store(true, Ordering::Relaxed);
        return false;
    }
    true
}

/// Opens and lists one directory.
fn process<N: Names>(
    shared: &Shared<'_, N>,
    root_dir: &Arc<N::Dir>,
    task: Task<N::Dir>,
    fallback: &AtomicU64,
) -> (Listing, Vec<Task<N::Dir>>) {
    let mut here = shared.root_placement;
    let mut mounted = None;
    let dir: Arc<N::Dir> = match &task.parent {
        None => Arc::clone(root_dir),
        Some(parent) => {
            if task.depth > MAX_DEPTH {
                shared
                    .diag
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .note(ProblemKind::TooDeep, 0, task.expect_ino);
                let f = ReadFailure {
                    kind: ReadErrorKind::TooDeep,
                    code: 0,
                };
                return (Listing::empty(Status::Failed(f)), Vec::new());
            }
            match shared.names.open_child(parent, &task.name) {
                Ok(opened) => {
                    // Where the opened directory lies comes first. On a mount
                    // point the listing names the covered directory's inode
                    // while the open lands on the mounted root, so the two
                    // inode numbers differ without anything having been
                    // replaced.
                    let moved = !same_mount(task.from, opened.placement);
                    if moved {
                        // Only another mount of this very file system can be
                        // read from this inode table, and only when the caller
                        // counts it as part of the scan.
                        if !shared.boundary.allows(opened.placement)
                            || opened.device != shared.device
                        {
                            return (Listing::empty(Status::Outside), Vec::new());
                        }
                    } else if opened.stat.ino != task.expect_ino {
                        // Not the directory that was listed.
                        return (Listing::empty(Status::Replaced), Vec::new());
                    } else if !shared.boundary.allows(opened.placement) {
                        return (Listing::empty(Status::Outside), Vec::new());
                    }
                    let first = shared
                        .visited
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .insert(opened.stat.ino);
                    if !first {
                        // A mount of something the scan already reaches is
                        // left unentered, so its files count once.
                        let status = if moved {
                            Status::Outside
                        } else {
                            Status::Duplicate
                        };
                        return (Listing::empty(status), Vec::new());
                    }
                    // Each directory is counted against the memory limit.
                    if !charge(shared, 64) {
                        return (Listing::empty(Status::Listed), Vec::new());
                    }
                    here = opened.placement;
                    if moved {
                        mounted = Some(Stat::from(opened.stat));
                    }
                    Arc::new(opened.dir)
                }
                Err(e) => {
                    let status = match e.raw_os_error() {
                        Some(libc::ENOENT) => Status::Gone,
                        Some(libc::ENOTDIR | libc::ELOOP) => Status::Replaced,
                        _ => {
                            shared
                                .diag
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .note(ProblemKind::DirectoryUnreadable, 0, task.expect_ino);
                            Status::Failed(failure_of(&e))
                        }
                    };
                    return (Listing::empty(status), Vec::new());
                }
            }
        }
    };
    let mut listing = Listing::empty(Status::Listed);
    listing.mounted = mounted;
    let mut children: Vec<Task<N::Dir>> = Vec::new();
    let result = shared.names.list(&dir, &mut |raw: RawName<'_>| {
        if shared.over.load(Ordering::Relaxed) {
            return;
        }
        let index = listing.entries.len();
        if !charge(shared, entry_cost(raw.name.len())) {
            return;
        }
        let (Ok(name_off), Ok(name_len), Ok(entry_at)) = (
            u32::try_from(listing.names.len()),
            u16::try_from(raw.name.len()),
            u32::try_from(index),
        ) else {
            return;
        };
        listing.names.extend_from_slice(raw.name);
        let row = shared.table.find(raw.ino).unwrap_or(NONE);
        let mut mode = shared.table.get(row).map(|s| s.mode);
        if row == NONE && raw.clean && !raw.name.is_empty() && !raw.name.contains(&b'/') {
            // Made after the bulk read, or missed by it: ask directly.
            if let Ok(e) = shared.names.stat_entry(&dir, raw.name) {
                fallback.fetch_add(1, Ordering::Relaxed);
                mode = Some(e.mode);
                listing.extras.push((entry_at, Stat::from(e)));
            }
        }
        // Walk into what may be a directory: the listing says so, or does
        // not know, and the inode table does not say otherwise.
        let may_be_dir = matches!(raw.d_type, DT_DIR | DT_UNKNOWN)
            && mode.is_none_or(|m| m & libc::S_IFMT == libc::S_IFDIR);
        let mut child = NONE;
        if may_be_dir && raw.clean && !raw.name.is_empty() && !raw.name.contains(&b'/') {
            let id = shared.next_id.fetch_add(1, Ordering::Relaxed);
            if id == NONE {
                shared.out_of_ids.store(true, Ordering::Relaxed);
            } else {
                child = id;
                children.push(Task {
                    id,
                    parent: Some(Arc::clone(&dir)),
                    name: raw.name.to_vec(),
                    expect_ino: raw.ino,
                    from: here,
                    depth: task.depth.saturating_add(1),
                });
            }
        }
        listing.entries.push(ListedEntry {
            name_off,
            name_len,
            d_type: raw.d_type,
            clean: raw.clean,
            ino: raw.ino,
            row,
            child,
        });
    });
    if let Err(e) = result {
        listing.failure = Some(failure_of(&e));
        shared
            .diag
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .note(ProblemKind::ListingCorrupt, task.expect_ino, 0);
    }
    (listing, children)
}

/// Whether two placements are the same mount (or, without mount IDs, the
/// same device).
fn same_mount(a: Placement, b: Placement) -> bool {
    match (a.mount_id, b.mount_id) {
        (Some(x), Some(y)) => x == y,
        _ => a.device == b.device,
    }
}
