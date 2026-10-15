//! Turning the loaded tables into scanner batches: the walk from the start
//! directory with an explicit stack, the sizes of every file (each disk extent
//! counted once, at the first file that is emitted with it), and what is
//! shared.
//!
//! # How sizes are counted
//!
//! * **Logical size** is the inode's size. A directory counts none, as the
//!   walker's does.
//! * **Allocated size** of a file is the sum of the sizes on disk of the
//!   distinct extents its file extent items name and nobody emitted before it
//!   has counted. A compressed extent counts its compressed size, a
//!   preallocated one its reservation, a hole nothing. Data kept inside the
//!   tree's own blocks (small files, link targets) counts the bytes stored,
//!   not a rounded-up block as the kernel's `st_blocks` does. Because the whole
//!   extent is counted,
//!   a file that refers to only part of an extent (after an overwrite in the
//!   middle of it) counts more than the walker, which asks the kernel for the
//!   bytes the file refers to; the extent stays on disk as long as any part is
//!   used.
//! * A file whose extents are all counted by others has allocated size zero.
//!   It is flagged shared.
//!
//! # What deleting frees
//!
//! A shared file carries `exclusive`: the bytes of the extents that nothing
//! but this file refers to. They are what deleting the file frees. The rest of
//! its bytes stay on disk while another file, snapshot or reflink uses them.
//! "Nothing but this file" is judged from every file extent item of every
//! subvolume of the file system, when the census is on (the default), and only
//! from the subvolumes of the scan when it is off. It is approximate where:
//!
//! * a subvolume is deleted but not yet cleaned up still counts as a user;
//! * data changes after the trees were read;
//! * files that share data with each other inside a folder that is deleted
//!   together are each taken as sharing with an outsider, so the folder's
//!   figure can be short. It is never long.

use crate::error::{Diagnostics, ProblemKind};
use crate::items::{self, ModeKind};
use crate::model::{EntryRec, F_COMPRESSED, F_DAMAGED, InodeRec, Model, Start};
use crate::mounts::{Graft, Grafts};
use crate::{BtrfsError, Compression, Sharing, Unaccounted};
use bs_scan::{Classifier, MAX_PATH_BYTES, ScanContext, ScanEnd, join_path};
use bs_tree::{
    Batch, Category, DirKey, FileKey, Meta, NodeFlags, NodeKind, ReadErrorKind, ReadFailure,
};
use std::rc::Rc;

/// Directories nested deeper than this are listed as unreadable. A path that
/// long cannot be named, so nothing legitimate is lost.
pub(crate) const MAX_DEPTH: u32 = 65_536;

/// A file this much smaller on disk than its length is sparse or compressed;
/// smaller gaps are rounding.
const SPARSE_MARGIN: u64 = 4096;

/// What the scan's own tree costs per entry besides the name's bytes.
const TREE_BYTES_PER_ENTRY: u64 = 96;

/// Code for damaged structures; there is no OS call behind it.
const CODE_CORRUPT: i32 = 117;
/// `EIO`.
const CODE_IO: i32 = 5;

const FLUSH_RECORDS: usize = 4096;
const FLUSH_NAME_BYTES: usize = 256 * 1024;

struct Frame {
    tree: u32,
    inode: u32,
    key: DirKey,
    path: Rc<[u8]>,
    depth: u32,
    /// Bytes of path this frame allocated (0 when it shares its parent's).
    own_path: u64,
}

#[derive(Default)]
struct Tally {
    allocated: u64,
    logical: u64,
    files: u64,
    dirs: u64,
    unreadable: u64,
}

fn bump(counter: &mut u64) {
    *counter = counter.saturating_add(1);
}

fn failure(kind: ReadErrorKind, code: i32) -> ReadFailure {
    ReadFailure { kind, code }
}

fn corrupt() -> ReadFailure {
    failure(ReadErrorKind::Corrupt, CODE_CORRUPT)
}

/// The figures of one file.
struct FileSizes {
    /// Bytes of extents counted at this file.
    counted: u64,
    /// Bytes of all its distinct extents, counted here or not.
    physical: u64,
    /// Bytes of extents that nothing else refers to.
    exclusive: u64,
    shared: bool,
}

pub(crate) struct Emitter<'a> {
    model: &'a Model,
    grafts: &'a Grafts,
    ctx: &'a ScanContext,
    classifier: Option<Classifier>,
    reached: Vec<u64>,
    claimed: Vec<u64>,
    path_room: u64,
    path_live: u64,
    batch: Batch,
    tally: Tally,
    scratch: Vec<u32>,
    pub diag: Diagnostics,
    pub unreadable_entries: u64,
    pub vanished_entries: u64,
    pub cancelled: bool,
    pub sharing: Sharing,
    pub compression: Compression,
}

fn bitset(bits: usize) -> Result<Vec<u64>, BtrfsError> {
    let words = bits.div_ceil(64);
    let mut v = Vec::new();
    v.try_reserve_exact(words)
        .map_err(|_| BtrfsError::TooLarge("cannot allocate the visited set"))?;
    v.resize(words, 0u64);
    Ok(v)
}

fn test_and_set(set: &mut [u64], at: usize) -> bool {
    match set.get_mut(at / 64) {
        Some(w) => {
            let bit = 1u64 << (at % 64);
            let was = *w & bit != 0;
            *w |= bit;
            was
        }
        None => true,
    }
}

fn has_bit(set: &[u64], at: usize) -> bool {
    set.get(at / 64).is_some_and(|w| (*w >> (at % 64)) & 1 == 1)
}

impl<'a> Emitter<'a> {
    pub fn new(
        model: &'a Model,
        grafts: &'a Grafts,
        ctx: &'a ScanContext,
        limit: u64,
    ) -> Result<Emitter<'a>, BtrfsError> {
        let inodes = model.inodes.len() as u64;
        let dirs = model
            .inodes
            .iter()
            .filter(|i| items::mode_kind(i.mode) == Some(ModeKind::Directory))
            .count() as u64;
        let sets = (inodes / 8)
            .saturating_add((model.exts.len() as u64) / 8)
            .saturating_add(32);
        let stack = dirs.saturating_mul((std::mem::size_of::<Frame>() as u64).saturating_add(8));
        let tree = inodes
            .saturating_mul(TREE_BYTES_PER_ENTRY)
            .saturating_add(model.names.len() as u64);
        let needed = model
            .bytes()
            .saturating_add(sets)
            .saturating_add(stack)
            .saturating_add(tree);
        if needed > limit {
            return Err(BtrfsError::TooLarge(
                "the tree and the walk need more memory than the limit allows",
            ));
        }
        Ok(Emitter {
            model,
            grafts,
            ctx,
            classifier: ctx.classifier(),
            reached: bitset(model.inodes.len())?,
            claimed: bitset(model.exts.len())?,
            path_room: limit.saturating_sub(needed),
            path_live: 0,
            batch: Batch::new(),
            tally: Tally::default(),
            scratch: Vec::new(),
            diag: Diagnostics::default(),
            unreadable_entries: 0,
            vanished_entries: 0,
            cancelled: false,
            sharing: Sharing::default(),
            compression: Compression::default(),
        })
    }

    fn flush(&mut self) -> bool {
        let t = std::mem::take(&mut self.tally);
        self.ctx.count(t.allocated, t.logical, t.files, t.dirs);
        self.ctx.count_problems(t.unreadable, 0);
        let batch = std::mem::take(&mut self.batch);
        self.ctx.send(batch)
    }

    fn flush_if_due(&mut self) -> bool {
        if self.batch.len() >= FLUSH_RECORDS || self.batch.name_bytes() >= FLUSH_NAME_BYTES {
            return self.flush();
        }
        true
    }

    fn close(&mut self, key: DirKey, broken: bool) {
        if broken {
            self.batch
                .unreadable(key, failure(ReadErrorKind::Corrupt, CODE_CORRUPT));
            bump(&mut self.tally.unreadable);
            bump(&mut self.unreadable_entries);
        } else {
            self.batch.listing_done(key);
        }
    }

    fn tree_id(&self, tree: u32) -> u64 {
        self.model
            .trees
            .get(usize::try_from(tree).unwrap_or(usize::MAX))
            .map_or(0, |t| t.id)
    }

    /// Walks the tree and sends it. The start directory goes first.
    pub fn run(&mut self, start: Start, root_name: &[u8]) -> Result<ScanEnd, BtrfsError> {
        let model = self.model;
        let at = model.find_inode(0, start.inode).ok_or(BtrfsError::BadRoot(
            "the start directory is not in the tree",
        ))?;
        let rec = *model.inode(at).ok_or(BtrfsError::BadRoot(
            "the start directory is not in the tree",
        ))?;
        if items::mode_kind(rec.mode) != Some(ModeKind::Directory) {
            return Err(BtrfsError::BadRoot("the start is not a directory"));
        }
        let key = self
            .ctx
            .next_dir_key()
            .ok_or(BtrfsError::BadRoot("no directory keys"))?;
        test_and_set(&mut self.reached, usize::try_from(at).unwrap_or(usize::MAX));
        let meta = Meta {
            mtime: rec.mtime,
            ..Meta::default()
        };
        self.batch
            .dir(key, DirKey::NONE, root_name, &meta)
            .map_err(|_| BtrfsError::TooLarge("root name"))?;
        self.tally.dirs = 1;
        if !self.flush() {
            self.cancelled = true;
            return Ok(ScanEnd::Cancelled);
        }
        let root_path: Rc<[u8]> = if self.classifier.is_some() {
            Rc::from(root_name)
        } else {
            Rc::from(&b""[..])
        };
        let mut stack = vec![Frame {
            tree: 0,
            inode: at,
            key,
            path: root_path,
            depth: 0,
            own_path: 0,
        }];
        while let Some(frame) = stack.pop() {
            if self.ctx.is_cancelled() {
                self.cancelled = true;
                self.flush();
                return Ok(ScanEnd::Cancelled);
            }
            let children = self.list(&frame);
            self.path_live = self.path_live.saturating_sub(frame.own_path);
            // First entry on top, so the walk follows the tree's order.
            stack.extend(children.into_iter().rev());
            if self.cancelled || !self.flush_if_due() {
                self.cancelled = true;
                return Ok(ScanEnd::Cancelled);
            }
        }
        if !self.flush() {
            self.cancelled = true;
            return Ok(ScanEnd::Cancelled);
        }
        Ok(ScanEnd::Completed)
    }

    /// Lists one directory and returns the subdirectories to list next.
    fn list(&mut self, frame: &Frame) -> Vec<Frame> {
        let model = self.model;
        let mut children = Vec::new();
        let Some(rec) = model.inode(frame.inode) else {
            self.close(frame.key, true);
            return children;
        };
        let place = self.classifier.as_ref().and_then(|c| c.place(&frame.path));
        let mut broken = rec.flags & F_DAMAGED != 0;
        for entry in model.entries_of(rec) {
            if !self.entry(frame, place, entry, &mut children) {
                broken = true;
            }
            if !self.flush_if_due() {
                self.cancelled = true;
                break;
            }
        }
        self.close(frame.key, broken);
        children
    }

    /// Emits one directory entry. `false` when the entry had to be dropped, so
    /// that its directory cannot claim to be complete.
    fn entry(
        &mut self,
        frame: &Frame,
        place: Option<Category>,
        entry: &EntryRec,
        children: &mut Vec<Frame>,
    ) -> bool {
        let model = self.model;
        let name = model.name_bytes(entry);
        let dir_ino = model.inode(frame.inode).map_or(0, |r| r.ino);
        let tree_id = self.tree_id(frame.tree);
        if name.is_empty() || name.iter().any(|&b| b == 0 || b == b'/') {
            self.diag.note(ProblemKind::DirentBadName, tree_id, dir_ino);
            return false;
        }
        if entry.subvolume {
            return self.subvolume_entry(frame, entry, name, children);
        }
        let Some(at) = model.find_inode(frame.tree, entry.target) else {
            // Created or removed between the pages of the search.
            self.diag
                .note(ProblemKind::DirentToMissingInode, tree_id, entry.target);
            bump(&mut self.vanished_entries);
            return true;
        };
        let Some(rec) = model.inode(at) else {
            return true;
        };
        let reached_before = test_and_set(&mut self.reached, usize::try_from(at).unwrap_or(0));
        let Some(kind) = items::mode_kind(rec.mode) else {
            // Already noted when the inode was read. It is shown, as what it
            // cannot be told to be, and flagged unreadable: its bytes and, for
            // a directory, what is below it are missing from the total.
            let meta = Meta {
                mtime: rec.mtime,
                failure: Some(corrupt()),
                ..Meta::default()
            };
            bump(&mut self.tally.files);
            bump(&mut self.tally.unreadable);
            bump(&mut self.unreadable_entries);
            self.batch
                .file(NodeKind::Other, frame.key, name, &meta)
                .ok();
            return true;
        };
        if !items::type_agrees(entry.ty, kind, rec.mode) {
            self.diag
                .note(ProblemKind::DirentTypeMismatch, tree_id, entry.target);
            return false;
        }
        if kind == ModeKind::Directory {
            if reached_before {
                self.diag
                    .note(ProblemKind::DirectoryLinkedTwice, tree_id, entry.target);
                return false;
            }
            self.dir_entry(frame, frame.tree, at, rec, name, children);
            return true;
        }
        self.file_entry(frame, place, rec, kind, name, reached_before, tree_id);
        true
    }

    fn subvolume_entry(
        &mut self,
        frame: &Frame,
        entry: &EntryRec,
        name: &[u8],
        children: &mut Vec<Frame>,
    ) -> bool {
        let model = self.model;
        let unreadable = |this: &mut Self, f: ReadFailure| {
            let Some(key) = this.ctx.next_dir_key() else {
                return;
            };
            let meta = Meta {
                failure: Some(f),
                ..Meta::default()
            };
            bump(&mut this.tally.dirs);
            bump(&mut this.tally.unreadable);
            bump(&mut this.unreadable_entries);
            this.batch.dir(key, frame.key, name, &meta).ok();
            this.batch.unreadable(key, f);
        };
        let Some(&tree) = model.tree_index.get(&entry.target) else {
            // Beyond the number of subvolumes followed.
            unreadable(self, failure(ReadErrorKind::Other, 0));
            return true;
        };
        let Some(at) = model.find_inode(tree, items::FIRST_FREE) else {
            // A tree that failed, or one with no root directory.
            unreadable(self, failure(ReadErrorKind::Io, CODE_IO));
            return true;
        };
        let Some(rec) = model.inode(at) else {
            return true;
        };
        if test_and_set(&mut self.reached, usize::try_from(at).unwrap_or(0)) {
            // A second way into a subvolume.
            self.diag
                .note(ProblemKind::SubvolumeSkipped, entry.target, 0);
            return false;
        }
        if items::mode_kind(rec.mode) != Some(ModeKind::Directory) {
            self.diag
                .note(ProblemKind::InodeInvalid, entry.target, items::FIRST_FREE);
            unreadable(self, corrupt());
            return true;
        }
        self.dir_entry(frame, tree, at, rec, name, children);
        true
    }

    /// A directory that cannot be given a key: it is shown as an unreadable
    /// entry.
    fn out_of_keys(&mut self, frame: &Frame, name: &[u8], mtime: i64) {
        let meta = Meta {
            failure: Some(failure(ReadErrorKind::Other, 0)),
            mtime,
            ..Meta::default()
        };
        bump(&mut self.tally.unreadable);
        bump(&mut self.unreadable_entries);
        bump(&mut self.tally.files);
        self.batch
            .file(NodeKind::Other, frame.key, name, &meta)
            .ok();
    }

    /// A directory entry that is not entered: a mount of something else.
    fn not_followed(&mut self, frame: &Frame, name: &[u8], mtime: i64) {
        let Some(key) = self.ctx.next_dir_key() else {
            self.out_of_keys(frame, name, mtime);
            return;
        };
        let meta = Meta {
            mtime,
            flags: NodeFlags::NOT_FOLLOWED,
            ..Meta::default()
        };
        bump(&mut self.tally.dirs);
        self.batch.dir(key, frame.key, name, &meta).ok();
        self.batch.listing_done(key);
    }

    fn dir_entry(
        &mut self,
        frame: &Frame,
        tree: u32,
        at: u32,
        rec: &InodeRec,
        name: &[u8],
        children: &mut Vec<Frame>,
    ) {
        let model = self.model;
        match self.grafts.get(at) {
            None => {}
            Some(Graft::NotFollowed) => {
                // The mount hides this folder; what the trees hold in it is
                // not what a walk would find there.
                self.not_followed(frame, name, rec.mtime);
                return;
            }
            Some(Graft::Unreadable) => {
                let Some(key) = self.ctx.next_dir_key() else {
                    self.out_of_keys(frame, name, rec.mtime);
                    return;
                };
                let f = failure(ReadErrorKind::Io, CODE_IO);
                let meta = Meta {
                    mtime: rec.mtime,
                    failure: Some(f),
                    ..Meta::default()
                };
                bump(&mut self.tally.dirs);
                bump(&mut self.tally.unreadable);
                bump(&mut self.unreadable_entries);
                self.batch.dir(key, frame.key, name, &meta).ok();
                self.batch.unreadable(key, f);
                return;
            }
            Some(Graft::Enter {
                tree: mounted,
                at: root,
            }) => {
                // The mounted directory takes the covered folder's place,
                // with its own numbers.
                let Some(mounted_rec) = model.inode(root) else {
                    self.not_followed(frame, name, rec.mtime);
                    return;
                };
                if test_and_set(&mut self.reached, usize::try_from(root).unwrap_or(0)) {
                    // Already reached another way (a bind mount of a folder
                    // the scan covers anyway): its files count once.
                    self.diag.note(
                        ProblemKind::DirectoryLinkedTwice,
                        self.tree_id(mounted),
                        mounted_rec.ino,
                    );
                    self.not_followed(frame, name, mounted_rec.mtime);
                    return;
                }
                self.enter_dir(frame, mounted, root, mounted_rec, name, children);
                return;
            }
        }
        self.enter_dir(frame, tree, at, rec, name, children);
    }

    fn enter_dir(
        &mut self,
        frame: &Frame,
        tree: u32,
        at: u32,
        rec: &InodeRec,
        name: &[u8],
        children: &mut Vec<Frame>,
    ) {
        let Some(key) = self.ctx.next_dir_key() else {
            // Out of keys: it cannot be described as a directory.
            self.out_of_keys(frame, name, rec.mtime);
            return;
        };
        let depth = frame.depth.saturating_add(1);
        let mut meta = Meta {
            mtime: rec.mtime,
            ..Meta::default()
        };
        if depth > MAX_DEPTH {
            self.diag
                .note(ProblemKind::TooDeep, self.tree_id(tree), rec.ino);
            meta.failure = Some(failure(ReadErrorKind::TooDeep, 0));
            bump(&mut self.tally.unreadable);
            bump(&mut self.unreadable_entries);
        }
        bump(&mut self.tally.dirs);
        self.batch.dir(key, frame.key, name, &meta).ok();
        if depth > MAX_DEPTH {
            return;
        }
        let mut own_path = 0u64;
        let path: Rc<[u8]> = if self.classifier.is_some() {
            // The path of every directory waiting its turn is held in memory;
            // past what the limit leaves, a directory shares its parent's
            // path and only its category lookups lose precision.
            let want = frame
                .path
                .len()
                .saturating_add(1)
                .saturating_add(name.len())
                .min(MAX_PATH_BYTES.saturating_add(usize::from(u8::MAX) + 1))
                as u64;
            if self.path_live.saturating_add(want) <= self.path_room {
                self.path_live = self.path_live.saturating_add(want);
                own_path = want;
                Rc::from(join_path(&frame.path, name, b'/'))
            } else {
                Rc::clone(&frame.path)
            }
        } else {
            Rc::clone(&frame.path)
        };
        children.push(Frame {
            tree,
            inode: at,
            key,
            path,
            depth,
            own_path,
        });
    }

    /// The sizes of a file, claiming its extents.
    fn sizes_of(&mut self, rec: &InodeRec) -> FileSizes {
        let model = self.model;
        self.scratch.clear();
        self.scratch.extend_from_slice(model.extents_of(rec));
        self.scratch.sort_unstable();
        self.scratch.dedup();
        // Data kept in the tree is this file's alone.
        let mut out = FileSizes {
            counted: rec.inline_bytes,
            physical: rec.inline_bytes,
            exclusive: rec.inline_bytes,
            shared: false,
        };
        for &index in &self.scratch {
            let Some(info) = model.ext(index) else {
                continue;
            };
            out.physical = out.physical.saturating_add(info.bytes);
            if !test_and_set(
                &mut self.claimed,
                usize::try_from(index).unwrap_or(usize::MAX),
            ) {
                out.counted = out.counted.saturating_add(info.bytes);
            }
            if info.foreign {
                out.shared = true;
            } else {
                out.exclusive = out.exclusive.saturating_add(info.bytes);
            }
        }
        out
    }

    #[allow(clippy::too_many_arguments)]
    fn file_entry(
        &mut self,
        frame: &Frame,
        place: Option<Category>,
        rec: &InodeRec,
        kind: ModeKind,
        name: &[u8],
        seen_before: bool,
        tree_id: u64,
    ) {
        let node_kind = match kind {
            ModeKind::Regular => NodeKind::File,
            ModeKind::Symlink => NodeKind::Symlink,
            _ => NodeKind::Other,
        };
        let sizes = self.sizes_of(rec);
        let logical = rec.size;
        let mut flags = NodeFlags::EMPTY;
        let mut link = None;
        if rec.nlink > 1 {
            // More than one name: flagged whether or not the others are in
            // the scan; keyed so the tree counts the bytes once.
            flags |= NodeFlags::HARD_LINK;
            link = Some(FileKey {
                volume: tree_id,
                id: u128::from(rec.ino),
            });
        }
        if kind == ModeKind::Symlink {
            flags |= NodeFlags::NOT_FOLLOWED;
        }
        if sizes.shared {
            flags |= NodeFlags::SHARED;
        }
        if kind == ModeKind::Regular
            && (rec.flags & F_COMPRESSED != 0
                || sizes.physical.saturating_add(SPARSE_MARGIN) <= logical)
        {
            flags |= NodeFlags::SPARSE_OR_COMPRESSED;
        }
        let mut meta = Meta {
            logical,
            allocated: sizes.counted,
            mtime: rec.mtime,
            flags,
            link,
            exclusive: sizes.exclusive.min(sizes.counted),
            ..Meta::default()
        };
        if rec.flags & F_DAMAGED != 0 {
            meta.failure = Some(corrupt());
            bump(&mut self.tally.unreadable);
            bump(&mut self.unreadable_entries);
        }
        if let Some(classifier) = &self.classifier {
            meta.category = classifier.category(place, node_kind, name);
        }
        if !seen_before {
            if sizes.shared {
                self.sharing.files = self.sharing.files.saturating_add(1);
                self.sharing.allocated = self.sharing.allocated.saturating_add(sizes.counted);
                self.sharing.exclusive = self.sharing.exclusive.saturating_add(meta.exclusive);
            }
            if rec.flags & F_COMPRESSED != 0 {
                self.compression.files = self.compression.files.saturating_add(1);
                self.compression.logical = self.compression.logical.saturating_add(logical);
                self.compression.on_disk = self.compression.on_disk.saturating_add(sizes.physical);
            }
        }
        // Every name is a file in the walker's counts; the tree sorts out
        // which of them carries the bytes.
        bump(&mut self.tally.files);
        self.tally.allocated = self.tally.allocated.saturating_add(sizes.counted);
        self.tally.logical = self.tally.logical.saturating_add(logical);
        // Names are at most 255 bytes, so a batch cannot be too full for one.
        self.batch.file(node_kind, frame.key, name, &meta).ok();
    }

    /// Counts what is in the trees but not in the tree built: inodes no
    /// directory reaches (files removed while still open, other parts of the
    /// file system when the scan starts below its root) and the bytes of
    /// their extents that nobody emitted has counted.
    pub fn account_for_the_rest(&mut self) -> (Unaccounted, u64) {
        let model = self.model;
        let mut out = Unaccounted::default();
        for (i, rec) in model.inodes.iter().enumerate() {
            if has_bit(&self.reached, i) {
                continue;
            }
            out.inodes = out.inodes.saturating_add(1);
            for &index in model.extents_of(rec) {
                let Some(info) = model.ext(index) else {
                    continue;
                };
                if !test_and_set(
                    &mut self.claimed,
                    usize::try_from(index).unwrap_or(usize::MAX),
                ) {
                    out.allocated = out.allocated.saturating_add(info.bytes);
                }
            }
        }
        (out, model.inodes.len() as u64)
    }
}
