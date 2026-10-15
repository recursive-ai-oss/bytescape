//! Loading the file system's metadata into memory: every inode in use, and
//! the entries of every directory, in the order the disk holds them.
//!
//! The disk is read in two sweeps. First the used part of every group's inode
//! table, in large sequential reads, which also finds the directories. Then
//! the directories' blocks, nearest first, by several threads at once: a
//! directory is one small read somewhere in its group, and what makes such
//! reads fast is having many in flight. The tree is built from the result
//! afterwards without touching the disk again.
//!
//! Nothing here trusts the disk: sizes of what is allocated are derived from
//! the device, reads are chunked and bounded, directory blocks are charged to
//! a budget that a consistent file system cannot exceed, and a structure that
//! does not parse marks only its own directory.

use crate::blocks::{BlockIo, MapFailure, Run, extent_runs, first_block_hint, legacy_runs};
use crate::dir::{BlockCorrupt, Event, inline_tail, walk_block};
use crate::error::{Diagnostics, Ext4Error, ProblemKind};
use crate::gd::{Group, GroupState, read_groups};
use crate::inode::{
    FL_EXTENTS, FL_INLINE_DATA, InodeRec, Parsed, REC_BAD, REC_UNREADABLE, i_block, parse_inode,
};
use crate::le::{to_usize, u32_at};
use crate::sb::Geometry;
use crate::source::ByteSource;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

/// Bytes read from an inode table or a directory at a time.
const CHUNK_BYTES: usize = 1 << 20;

/// The most threads that read directories, whatever the caller asks for.
const MAX_THREADS: usize = 128;

/// Directories handed to a reading thread at a time.
const DIRS_PER_TASK: usize = 32;

/// Why a directory could not be listed completely.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DirFail {
    None,
    /// The structures are damaged or inconsistent.
    Corrupt,
    /// The device failed to deliver a block.
    Io,
}

/// Where a directory's entries lie in the entry table.
#[derive(Clone, Copy, Debug)]
pub(crate) struct DirSlot {
    pub ino: u32,
    pub first: u32,
    pub count: u32,
    /// The inode its `..` entry names, or 0 if it has none.
    pub dotdot: u32,
    pub fail: DirFail,
}

/// One entry of a directory.
#[derive(Clone, Copy, Debug)]
pub(crate) struct EntryRec {
    pub child: u32,
    pub name_off: u32,
    pub name_len: u8,
    pub file_type: u8,
}

/// What `locate` finds for an inode number.
pub(crate) enum Located {
    OutOfRange,
    /// Not in use, or never initialized.
    Free,
    /// In use, but the inode could not be read or is nonsense.
    Unreadable {
        io: bool,
    },
    /// In use; the second field is its index in the table of records.
    Used(InodeRec, usize),
}

/// A directory found in the inode scan, waiting for its blocks to be read.
struct PendingDir {
    ino: u32,
    flags: u32,
    size: u64,
    i_block: [u8; 60],
    tail: Option<Vec<u8>>,
}

/// The directories found so far, and the memory they take. A directory costs
/// its pending record here, its slot in the finished model and its inline
/// tail; a disk can hold as many of them as it has inodes, so they are
/// charged to the memory limit like the other tables.
struct PendingDirs {
    dirs: Vec<PendingDir>,
    bytes: u64,
    limit: u64,
}

impl PendingDirs {
    fn new(limit: u64) -> PendingDirs {
        PendingDirs {
            dirs: Vec::new(),
            bytes: 0,
            limit,
        }
    }

    /// Adds a directory. `false` when that takes the total past the limit.
    fn push(&mut self, dir: PendingDir) -> bool {
        let cost = std::mem::size_of::<PendingDir>()
            .saturating_add(std::mem::size_of::<DirSlot>())
            .saturating_add(dir.tail.as_ref().map_or(0, Vec::len));
        self.bytes = self
            .bytes
            .saturating_add(u64::try_from(cost).unwrap_or(u64::MAX));
        if self.bytes > self.limit {
            return false;
        }
        self.dirs.push(dir);
        true
    }
}

/// The loaded file system.
pub(crate) struct Model {
    pub geo: Geometry,
    pub groups: Vec<Group>,
    slot_base: Vec<u32>,
    pub recs: Vec<InodeRec>,
    pub dirs: Vec<DirSlot>,
    pub entries: Vec<EntryRec>,
    pub names: Vec<u8>,
    /// Directory and map blocks read.
    pub blocks_read: u64,
    /// Bytes of inode tables read.
    pub inode_bytes_read: u64,
}

impl Model {
    /// Where the record of inode `ino` is, if the inode table holds it.
    pub fn locate(&self, ino: u32) -> Located {
        let Some(index) = ino.checked_sub(1) else {
            return Located::OutOfRange;
        };
        if ino > self.geo.inodes_count {
            return Located::OutOfRange;
        }
        let ipg = self.geo.inodes_per_group;
        let group =
            to_usize(u64::from(index.checked_div(ipg).unwrap_or(u32::MAX))).unwrap_or(usize::MAX);
        let within = index.checked_rem(ipg).unwrap_or(u32::MAX);
        let (Some(g), Some(base)) = (self.groups.get(group), self.slot_base.get(group)) else {
            return Located::OutOfRange;
        };
        match g.state {
            GroupState::Damaged => return Located::Unreadable { io: false },
            GroupState::Uninit => return Located::Free,
            GroupState::Ok => {}
        }
        if within >= g.used_inodes {
            return Located::Free;
        }
        let slot = to_usize(u64::from(base.saturating_add(within))).unwrap_or(usize::MAX);
        let Some(rec) = self.recs.get(slot) else {
            return Located::OutOfRange;
        };
        if rec.state & REC_UNREADABLE != 0 {
            return Located::Unreadable { io: true };
        }
        if rec.state & REC_BAD != 0 {
            return Located::Unreadable { io: false };
        }
        if !rec.is_in_use() {
            return Located::Free;
        }
        Located::Used(*rec, slot)
    }

    /// Index of the first record of group `group` in the table of records.
    pub fn slot_base_of(&self, group: usize) -> u32 {
        self.slot_base.get(group).copied().unwrap_or(u32::MAX)
    }

    /// The listing of directory `ino`.
    pub fn dir_slot(&self, ino: u32) -> Option<&DirSlot> {
        let at = self.dirs.binary_search_by_key(&ino, |d| d.ino).ok()?;
        self.dirs.get(at)
    }

    pub fn entries_of(&self, slot: &DirSlot) -> &[EntryRec] {
        let start = to_usize(u64::from(slot.first)).unwrap_or(usize::MAX);
        let end = start.saturating_add(to_usize(u64::from(slot.count)).unwrap_or(0));
        self.entries.get(start..end).unwrap_or(&[])
    }

    pub fn name_of(&self, entry: &EntryRec) -> &[u8] {
        let start = to_usize(u64::from(entry.name_off)).unwrap_or(usize::MAX);
        let end = start.saturating_add(usize::from(entry.name_len));
        self.names.get(start..end).unwrap_or(&[])
    }
}

/// Limits on what a read may take.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Limits {
    /// Memory the tables may take; exceeded means `TooLarge`.
    pub max_memory_bytes: u64,
    /// Threads reading directory blocks (at least one).
    pub threads: usize,
}

/// Reads everything. Returns `Ok(None)` when `cancel` fired.
pub(crate) fn load<S: ByteSource + ?Sized>(
    src: &S,
    geo: Geometry,
    limits: Limits,
    cancel: &(dyn Fn() -> bool + Sync),
    diag: &mut Diagnostics,
) -> Result<Option<Model>, Ext4Error> {
    let groups = read_groups(src, &geo, diag)?;

    // How many inode records will be kept, and where each group's start.
    let mut slot_base = Vec::with_capacity(groups.len());
    let mut total: u64 = 0;
    for g in &groups {
        slot_base.push(u32::try_from(total).map_err(|_| Ext4Error::TooLarge("inodes"))?);
        total = total.saturating_add(u64::from(g.used_inodes));
    }
    let record_bytes = total.saturating_mul(std::mem::size_of::<InodeRec>() as u64);
    if record_bytes > limits.max_memory_bytes {
        return Err(Ext4Error::TooLarge("inode records exceed the memory limit"));
    }
    let mut recs: Vec<InodeRec> = Vec::new();
    recs.try_reserve_exact(to_usize(total).ok_or(Ext4Error::TooLarge("inodes"))?)
        .map_err(|_| Ext4Error::TooLarge("cannot allocate the inode records"))?;

    let mut model = Model {
        geo,
        groups,
        slot_base,
        recs,
        dirs: Vec::new(),
        entries: Vec::new(),
        names: Vec::new(),
        blocks_read: 0,
        inode_bytes_read: 0,
    };

    // Sweep one: the inode tables, group by group.
    let inode_size =
        to_usize(u64::from(model.geo.inode_size)).ok_or(Ext4Error::TooLarge("inode"))?;
    let chunk_inodes = CHUNK_BYTES.checked_div(inode_size).unwrap_or(1).max(1);
    let mut buf: Vec<u8> = Vec::new();
    let mut pending = PendingDirs::new(limits.max_memory_bytes.saturating_sub(record_bytes));
    for g in 0..model.groups.len() {
        if cancel() {
            return Ok(None);
        }
        let Some(group) = model.groups.get(g).copied() else {
            break;
        };
        if group.state != GroupState::Ok {
            continue;
        }
        let used = to_usize(u64::from(group.used_inodes)).unwrap_or(0);
        let table_offset = group
            .inode_table
            .checked_mul(u64::from(model.geo.block_size))
            .ok_or(Ext4Error::TooLarge("inode table offset"))?;
        let mut done = 0usize;
        while done < used {
            if cancel() {
                return Ok(None);
            }
            let n = chunk_inodes.min(used.saturating_sub(done));
            let bytes = n.saturating_mul(inode_size);
            let offset = table_offset
                .checked_add(u64::try_from(done.saturating_mul(inode_size)).unwrap_or(u64::MAX))
                .ok_or(Ext4Error::TooLarge("inode table offset"))?;
            buf.clear();
            buf.resize(bytes, 0);
            match src.read_at(offset, &mut buf) {
                Ok(()) => {
                    model.inode_bytes_read = model.inode_bytes_read.saturating_add(bytes as u64);
                    for (j, raw) in buf.chunks_exact(inode_size).enumerate() {
                        let index = done.saturating_add(j);
                        let ino = (g as u64)
                            .saturating_mul(u64::from(model.geo.inodes_per_group))
                            .saturating_add(index as u64)
                            .saturating_add(1);
                        model.take_inode(raw, u32::try_from(ino).unwrap_or(0), &mut pending, diag);
                    }
                    if pending.bytes > pending.limit {
                        return Err(Ext4Error::TooLarge(
                            "the directories exceed the memory limit",
                        ));
                    }
                }
                Err(_) => {
                    diag.note(ProblemKind::InodeTableUnreadable, 0, g as u64);
                    for _ in 0..n {
                        model.recs.push(InodeRec {
                            state: REC_UNREADABLE,
                            ..InodeRec::default()
                        });
                    }
                }
            }
            done = done.saturating_add(n);
        }
    }

    // Sweep two: the directories' blocks, in the order they lie on the disk.
    pending.dirs.sort_by_key(|p| first_block_hint(&p.i_block));
    let table_budget = limits
        .max_memory_bytes
        .saturating_sub(record_bytes)
        .saturating_sub(pending.bytes);
    read_directories(
        src,
        &mut model,
        &pending.dirs,
        table_budget,
        limits.threads,
        cancel,
        diag,
    )?;
    if cancel() {
        return Ok(None);
    }
    Ok(Some(model))
}

/// What one reading thread produced for a run of directories. Offsets in
/// `slots` and `entries` are relative to this chunk's own tables.
#[derive(Default)]
struct ChunkOut {
    slots: Vec<DirSlot>,
    entries: Vec<EntryRec>,
    names: Vec<u8>,
    diag: Diagnostics,
    blocks_read: u64,
}

/// What the reading threads share.
struct Shared<'a, S: ?Sized> {
    src: &'a S,
    geo: &'a Geometry,
    budget: AtomicU64,
    /// Bytes of entries and names held by all chunks so far.
    table_bytes: AtomicU64,
    table_limit: u64,
    over_limit: AtomicBool,
    next_task: AtomicUsize,
    results: Mutex<Vec<(usize, ChunkOut)>>,
    cancel: &'a (dyn Fn() -> bool + Sync),
}

fn read_directories<S: ByteSource + ?Sized>(
    src: &S,
    model: &mut Model,
    pending: &[PendingDir],
    table_limit: u64,
    threads: usize,
    cancel: &(dyn Fn() -> bool + Sync),
    diag: &mut Diagnostics,
) -> Result<(), Ext4Error> {
    let shared = Shared {
        src,
        geo: &model.geo,
        budget: AtomicU64::new(model.geo.blocks_count),
        table_bytes: AtomicU64::new(0),
        table_limit,
        over_limit: AtomicBool::new(false),
        next_task: AtomicUsize::new(0),
        results: Mutex::new(Vec::new()),
        cancel,
    };
    let tasks: Vec<&[PendingDir]> = pending.chunks(DIRS_PER_TASK).collect();
    let workers = threads.clamp(1, tasks.len().clamp(1, MAX_THREADS));
    std::thread::scope(|scope| {
        for _ in 1..workers {
            // A thread that cannot be started just leaves the work to the
            // others.
            let _ = std::thread::Builder::new()
                .name("ext4-dirs".into())
                .spawn_scoped(scope, || work(&shared, &tasks));
        }
        work(&shared, &tasks);
    });
    if shared.over_limit.load(Ordering::Relaxed) {
        return Err(Ext4Error::TooLarge(
            "directory tables exceed the memory limit",
        ));
    }
    let mut results = shared
        .results
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // Merge in the order of the work, whichever thread did it, so that the
    // result does not depend on scheduling.
    results.sort_by_key(|(i, _)| *i);
    for (_, out) in results {
        let entry_base = u32::try_from(model.entries.len())
            .map_err(|_| Ext4Error::TooLarge("directory entries"))?;
        let name_base = u32::try_from(model.names.len())
            .map_err(|_| Ext4Error::TooLarge("directory entry names"))?;
        let entries_added = u32::try_from(out.entries.len())
            .map_err(|_| Ext4Error::TooLarge("directory entries"))?;
        let names_added = u32::try_from(out.names.len())
            .map_err(|_| Ext4Error::TooLarge("directory entry names"))?;
        if entry_base.checked_add(entries_added).is_none()
            || name_base.checked_add(names_added).is_none()
        {
            return Err(Ext4Error::TooLarge("directory entries"));
        }
        model.names.extend_from_slice(&out.names);
        model.entries.extend(out.entries.iter().map(|e| EntryRec {
            name_off: e.name_off.saturating_add(name_base),
            ..*e
        }));
        model.dirs.extend(out.slots.iter().map(|s| DirSlot {
            first: s.first.saturating_add(entry_base),
            ..*s
        }));
        diag.absorb(&out.diag);
        model.blocks_read = model.blocks_read.saturating_add(out.blocks_read);
    }
    model.dirs.sort_by_key(|d| d.ino);
    Ok(())
}

/// One thread's share: takes the next run of directories until none is left.
fn work<S: ByteSource + ?Sized>(shared: &Shared<'_, S>, tasks: &[&[PendingDir]]) {
    let Some(mut io) = BlockIo::new(
        shared.src,
        shared.geo.block_size,
        shared.geo.blocks_count,
        &shared.budget,
    ) else {
        return;
    };
    let mut buf = Vec::new();
    loop {
        if (shared.cancel)() || shared.over_limit.load(Ordering::Relaxed) {
            return;
        }
        let i = shared.next_task.fetch_add(1, Ordering::Relaxed);
        let Some(task) = tasks.get(i) else {
            return;
        };
        let mut out = ChunkOut::default();
        let before = io.blocks_read;
        for dir in *task {
            let slot = read_dir(shared, &mut io, dir, &mut buf, &mut out);
            out.slots.push(slot);
            if shared.over_limit.load(Ordering::Relaxed) {
                return;
            }
        }
        out.blocks_read = io.blocks_read.saturating_sub(before);
        shared
            .results
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((i, out));
    }
}

impl Model {
    fn take_inode(
        &mut self,
        raw: &[u8],
        ino: u32,
        pending: &mut PendingDirs,
        diag: &mut Diagnostics,
    ) {
        match parse_inode(raw, &self.geo) {
            Parsed::Free => self.recs.push(InodeRec::default()),
            Parsed::Bad => {
                diag.note(ProblemKind::InodeInvalid, ino, 0);
                self.recs.push(InodeRec {
                    state: REC_BAD,
                    ..InodeRec::default()
                });
            }
            Parsed::Used(rec) => {
                if rec.is_dir() {
                    let flags = u32_at(raw, 0x20).unwrap_or(0);
                    let mut block = [0u8; 60];
                    if let Some(src) = i_block(raw) {
                        block.copy_from_slice(src);
                    }
                    let tail = if flags & FL_INLINE_DATA != 0 {
                        inline_tail(raw).map(<[u8]>::to_vec)
                    } else {
                        None
                    };
                    // Past the limit the directory is dropped and the sweep
                    // ends with `TooLarge` when the chunk is done.
                    pending.push(PendingDir {
                        ino,
                        flags,
                        size: rec.size,
                        i_block: block,
                        tail,
                    });
                }
                self.recs.push(rec);
            }
        }
    }
}

/// What one directory read has found so far.
struct DirState<'a> {
    diag: &'a mut Diagnostics,
    ino: u32,
    fail: DirFail,
    dotdot: u32,
    /// Set when the entry table is full; the rest of the directory is lost.
    stop: bool,
}

impl DirState<'_> {
    fn note(&mut self, kind: ProblemKind, detail: u64, fail: DirFail) {
        self.diag.note(kind, self.ino, detail);
        self.fail = merge_fail(self.fail, fail);
    }

    fn map_failure(&mut self, f: MapFailure) {
        let fail = if f.io { DirFail::Io } else { DirFail::Corrupt };
        self.note(f.kind, f.detail, fail);
    }
}

fn merge_fail(a: DirFail, b: DirFail) -> DirFail {
    match (a, b) {
        (DirFail::Corrupt, _) | (_, DirFail::Corrupt) => DirFail::Corrupt,
        (DirFail::Io, _) | (_, DirFail::Io) => DirFail::Io,
        _ => DirFail::None,
    }
}

/// Reads one directory's blocks and appends its entries to `out`.
fn read_dir<S: ByteSource + ?Sized>(
    shared: &Shared<'_, S>,
    io: &mut BlockIo<'_, S>,
    dir: &PendingDir,
    buf: &mut Vec<u8>,
    out: &mut ChunkOut,
) -> DirSlot {
    let first = u32::try_from(out.entries.len()).unwrap_or(u32::MAX);
    let block_size = io.block_size();
    let has_types = shared.geo.has_filetype();
    let mut diag = Diagnostics::default();
    let mut state = DirState {
        diag: &mut diag,
        ino: dir.ino,
        fail: DirFail::None,
        dotdot: 0,
        stop: false,
    };

    if dir.flags & FL_INLINE_DATA != 0 {
        // The first four bytes of i_block name the parent; the rest of it and
        // the system.data attribute hold entries without dots.
        state.dotdot = u32_at(&dir.i_block, 0).unwrap_or(0);
        let areas: [Option<&[u8]>; 2] = [dir.i_block.get(4..), dir.tail.as_deref()];
        for area in areas.into_iter().flatten() {
            add_block(shared, out, area, block_size, false, has_types, &mut state);
        }
    } else {
        let wanted = dir.size.div_ceil(u64::from(shared.geo.block_size));
        let limit = wanted.min(io.blocks_count());
        let (runs, failure) = if dir.flags & FL_EXTENTS != 0 {
            extent_runs(io, &dir.i_block, limit)
        } else {
            legacy_runs(io, &dir.i_block, limit)
        };
        if let Some(f) = failure {
            state.map_failure(f);
        }
        for run in &runs {
            if state.stop {
                break;
            }
            read_run(shared, io, out, run, buf, has_types, &mut state);
        }
    }

    let slot = DirSlot {
        ino: dir.ino,
        first,
        count: u32::try_from(out.entries.len())
            .unwrap_or(u32::MAX)
            .saturating_sub(first),
        dotdot: state.dotdot,
        fail: state.fail,
    };
    out.diag.absorb(&diag);
    slot
}

/// Reads the blocks of one run and parses their entries.
fn read_run<S: ByteSource + ?Sized>(
    shared: &Shared<'_, S>,
    io: &mut BlockIo<'_, S>,
    out: &mut ChunkOut,
    run: &Run,
    buf: &mut Vec<u8>,
    has_types: bool,
    state: &mut DirState<'_>,
) {
    let block_size = io.block_size();
    let per_chunk =
        u64::try_from(CHUNK_BYTES.checked_div(block_size).unwrap_or(1).max(1)).unwrap_or(1);
    let mut done = 0u64;
    while done < run.len && !state.stop {
        let n = per_chunk.min(run.len.saturating_sub(done));
        let first = run.pblk.saturating_add(done);
        // One bad chunk does not hide the next.
        if let Err(f) = io.read(first, n, buf) {
            state.map_failure(f);
            done = done.saturating_add(n);
            continue;
        }
        for (i, block) in buf.chunks_exact(block_size).enumerate() {
            let lblk = run.lblk.saturating_add(done).saturating_add(i as u64);
            add_block(shared, out, block, block_size, lblk == 0, has_types, state);
            if state.stop {
                break;
            }
        }
        done = done.saturating_add(n);
    }
}

/// Parses one block (or inline area) and appends its entries.
fn add_block<S: ?Sized>(
    shared: &Shared<'_, S>,
    out: &mut ChunkOut,
    block: &[u8],
    block_size: usize,
    first_block: bool,
    has_types: bool,
    state: &mut DirState<'_>,
) {
    let entries = &mut out.entries;
    let names = &mut out.names;
    let before = (entries.len(), names.len());
    let mut bad_names: Vec<u32> = Vec::new();
    let mut full = false;
    let mut dotdot = state.dotdot;
    let walked = walk_block(
        block,
        block_size,
        first_block,
        has_types,
        |event| match event {
            Event::DotDot(inode) => {
                dotdot = inode;
                true
            }
            Event::BadName(inode) => {
                bad_names.push(inode);
                true
            }
            Event::Entry(d) => {
                let index = u32::try_from(entries.len()).ok();
                let offset = u32::try_from(names.len()).ok();
                let name_len = u8::try_from(d.name.len()).ok();
                match (index, offset, name_len) {
                    (Some(index), Some(offset), Some(name_len))
                        if index < u32::MAX
                            && offset.checked_add(u32::from(name_len)).is_some() =>
                    {
                        names.extend_from_slice(d.name);
                        entries.push(EntryRec {
                            child: d.inode,
                            name_off: offset,
                            name_len,
                            file_type: d.file_type,
                        });
                        true
                    }
                    _ => {
                        full = true;
                        false
                    }
                }
            }
        },
    );
    state.dotdot = dotdot;
    // Account for what was added, for the memory limit.
    let added = entries
        .len()
        .saturating_sub(before.0)
        .saturating_mul(std::mem::size_of::<EntryRec>())
        .saturating_add(names.len().saturating_sub(before.1));
    let total = shared
        .table_bytes
        .fetch_add(u64::try_from(added).unwrap_or(u64::MAX), Ordering::Relaxed)
        .saturating_add(u64::try_from(added).unwrap_or(u64::MAX));
    if total > shared.table_limit {
        shared.over_limit.store(true, Ordering::Relaxed);
        state.stop = true;
    }
    for inode in bad_names {
        state.note(
            ProblemKind::DirentBadName,
            u64::from(inode),
            DirFail::Corrupt,
        );
    }
    if full {
        state.note(ProblemKind::TooManyEntries, 0, DirFail::Corrupt);
        state.stop = true;
    }
    if let Err(BlockCorrupt { offset }) = walked {
        state.note(
            ProblemKind::DirectoryBlockCorrupt,
            u64::try_from(offset).unwrap_or(u64::MAX),
            DirFail::Corrupt,
        );
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::cast_possible_truncation,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects
    )]
    use super::*;

    fn dir(tail: Option<Vec<u8>>) -> PendingDir {
        PendingDir {
            ino: 12,
            flags: 0,
            size: 0,
            i_block: [0; 60],
            tail,
        }
    }

    #[test]
    fn pending_directories_are_charged_including_their_tails() {
        let each = std::mem::size_of::<PendingDir>() + std::mem::size_of::<DirSlot>();
        let mut pending = PendingDirs::new(u64::try_from(each * 2 + 100).unwrap());
        assert!(pending.push(dir(None)));
        assert_eq!(pending.bytes, each as u64);
        assert!(pending.push(dir(Some(vec![0; 50]))));
        assert_eq!(pending.bytes, (each * 2 + 50) as u64);
        // 51 more bytes of tail than the limit has room for.
        assert!(!pending.push(dir(Some(vec![0; 1]))));
        assert!(pending.bytes > pending.limit);
        assert_eq!(pending.dirs.len(), 2, "the refused one is not kept");
    }
}
