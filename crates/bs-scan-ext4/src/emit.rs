//! Turning the loaded file system into scanner batches: walks the directory
//! tree from the root with an explicit stack, checks each entry against the
//! inode it names, and reports everything it cannot vouch for.

use crate::error::{Diagnostics, Ext4Error, ProblemKind};
use crate::inode::{InodeRec, S_IFDIR, S_IFLNK, S_IFMT, S_IFREG};
use crate::reader::{DirFail, DirSlot, EntryRec, Located, Model};
use crate::sb::ROOT_INODE;
use crate::{Hidden, Unaccounted};
use bs_scan::{Classifier, ScanContext, ScanEnd, join_path};
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

/// `EUCLEAN`: "structure needs cleaning", what the kernel reports for a
/// damaged directory.
const CODE_CORRUPT: i32 = 117;
/// `EIO`.
const CODE_IO: i32 = 5;

const FLUSH_RECORDS: usize = 4096;
const FLUSH_NAME_BYTES: usize = 256 * 1024;

/// Paths of mount points below the file system's root, as a tree of names.
pub(crate) struct SkipSet {
    nodes: Vec<SkipNode>,
}

struct SkipNode {
    children: Vec<(Box<[u8]>, u32)>,
    mount: bool,
}

impl SkipSet {
    pub fn new(paths: &[Vec<u8>]) -> SkipSet {
        let mut set = SkipSet {
            nodes: vec![SkipNode {
                children: Vec::new(),
                mount: false,
            }],
        };
        for path in paths {
            let mut at = 0u32;
            let mut any = false;
            for part in path.split(|&b| b == b'/').filter(|p| !p.is_empty()) {
                any = true;
                at = set.child_or_insert(at, part);
            }
            if any && let Some(node) = set.nodes.get_mut(at as usize) {
                node.mount = true;
            }
        }
        set
    }

    fn child_or_insert(&mut self, at: u32, name: &[u8]) -> u32 {
        if let Some(found) = self.child(at, name) {
            return found;
        }
        let new = u32::try_from(self.nodes.len()).unwrap_or(u32::MAX);
        self.nodes.push(SkipNode {
            children: Vec::new(),
            mount: false,
        });
        if let Some(node) = self.nodes.get_mut(at as usize) {
            node.children.push((name.into(), new));
        }
        new
    }

    pub fn child(&self, at: u32, name: &[u8]) -> Option<u32> {
        let node = self.nodes.get(at as usize)?;
        node.children
            .iter()
            .find(|(n, _)| &**n == name)
            .map(|&(_, id)| id)
    }

    pub fn is_mount(&self, at: u32) -> bool {
        self.nodes.get(at as usize).is_some_and(|n| n.mount)
    }
}

enum PathSpec {
    Full(Rc<[u8]>),
    Child {
        parent: Rc<[u8]>,
        name_off: u32,
        name_len: u8,
    },
}

struct Frame {
    ino: u32,
    key: DirKey,
    path: PathSpec,
    trie: Option<u32>,
    depth: u32,
    /// Below a mount point of another file system: walked to account for
    /// what lies under it, but not reported.
    shadow: bool,
}

#[derive(Default)]
struct Tally {
    allocated: u64,
    logical: u64,
    files: u64,
    dirs: u64,
    unreadable: u64,
    vanished: u64,
}

pub(crate) struct Emitter<'a> {
    model: &'a Model,
    ctx: &'a ScanContext,
    classifier: Option<Classifier>,
    skip: SkipSet,
    reached: Vec<u64>,
    /// Records whose own bytes went into the tree. Narrower than `reached`:
    /// what lies under a mount point of another file system is walked, and
    /// so reached, but only added to the hidden totals.
    counted: Vec<u64>,
    batch: Batch,
    tally: Tally,
    volume: u64,
    pub diag: Diagnostics,
    pub hidden: Hidden,
    pub unreadable_entries: u64,
    pub vanished_entries: u64,
    pub cancelled: bool,
    /// Set while a directory is being listed when one of its entries was
    /// dropped and something may be missing because of it.
    listing_lost: bool,
}

fn bump(counter: &mut u64) {
    *counter = counter.saturating_add(1);
}

fn kind_of_mode(mode: u16) -> NodeKind {
    match mode & S_IFMT {
        S_IFDIR => NodeKind::Dir,
        S_IFREG => NodeKind::File,
        S_IFLNK => NodeKind::Symlink,
        _ => NodeKind::Other,
    }
}

/// The kind a directory entry's type byte announces, if it announces one.
fn kind_of_file_type(file_type: u8) -> Option<NodeKind> {
    match file_type {
        1 => Some(NodeKind::File),
        2 => Some(NodeKind::Dir),
        3..=6 => Some(NodeKind::Other),
        7 => Some(NodeKind::Symlink),
        _ => None,
    }
}

fn failure(io: bool) -> ReadFailure {
    if io {
        ReadFailure {
            kind: ReadErrorKind::Io,
            code: CODE_IO,
        }
    } else {
        ReadFailure {
            kind: ReadErrorKind::Corrupt,
            code: CODE_CORRUPT,
        }
    }
}

/// The record data for an inode in use, as the walker would report it.
fn meta_for(rec: &InodeRec, kind: NodeKind, ino: u32, volume: u64) -> Meta {
    let allocated = rec.blocks512.saturating_mul(512);
    // A directory's size describes its entry table, not data held in it.
    let logical = if kind == NodeKind::Dir { 0 } else { rec.size };
    let mut flags = NodeFlags::EMPTY;
    let mut link = None;
    if kind != NodeKind::Dir {
        if kind == NodeKind::Symlink {
            flags |= NodeFlags::NOT_FOLLOWED;
        }
        if kind == NodeKind::File && allocated.saturating_add(SPARSE_MARGIN) <= logical {
            flags |= NodeFlags::SPARSE_OR_COMPRESSED;
        }
        if rec.links > 1 {
            flags |= NodeFlags::HARD_LINK;
            link = Some(FileKey {
                volume,
                id: u128::from(ino),
            });
        }
    }
    Meta {
        logical,
        allocated,
        mtime: rec.mtime,
        flags,
        link,
        ..Meta::default()
    }
}

impl<'a> Emitter<'a> {
    pub fn new(
        model: &'a Model,
        ctx: &'a ScanContext,
        skip: SkipSet,
        volume: u64,
    ) -> Result<Emitter<'a>, Ext4Error> {
        let words = model.recs.len().div_ceil(64);
        let mut reached = Vec::new();
        reached
            .try_reserve_exact(words)
            .map_err(|_| Ext4Error::TooLarge("cannot allocate the visited set"))?;
        reached.resize(words, 0u64);
        let mut counted = Vec::new();
        counted
            .try_reserve_exact(words)
            .map_err(|_| Ext4Error::TooLarge("cannot allocate the visited set"))?;
        counted.resize(words, 0u64);
        let classifier = ctx.classifier();
        Ok(Emitter {
            model,
            ctx,
            classifier,
            skip,
            reached,
            counted,
            batch: Batch::new(),
            tally: Tally::default(),
            volume,
            diag: Diagnostics::default(),
            hidden: Hidden::default(),
            unreadable_entries: 0,
            vanished_entries: 0,
            cancelled: false,
            listing_lost: false,
        })
    }

    /// Marks a record as reached; says whether it already was.
    fn test_and_set(&mut self, slot: usize) -> bool {
        let (word, bit) = (slot / 64, slot % 64);
        match self.reached.get_mut(word) {
            Some(w) => {
                let was = (*w >> bit) & 1 == 1;
                *w |= 1u64 << bit;
                was
            }
            None => true,
        }
    }

    /// Notes that the record's bytes are in the tree.
    fn mark_counted(&mut self, slot: usize) {
        if let Some(w) = self.counted.get_mut(slot / 64) {
            *w |= 1u64 << (slot % 64);
        }
    }

    fn is_counted(&self, slot: usize) -> bool {
        self.counted
            .get(slot / 64)
            .is_some_and(|w| (*w >> (slot % 64)) & 1 == 1)
    }

    fn is_reached(&self, slot: usize) -> bool {
        self.reached
            .get(slot / 64)
            .is_some_and(|w| (*w >> (slot % 64)) & 1 == 1)
    }

    /// An entry of the directory being listed was dropped. If the inode it
    /// names is in use and nothing has counted it yet, whatever it leads to
    /// (a whole subtree, for a directory) is missing from the tree, so the
    /// listing cannot be called complete. An inode that was already reached
    /// by another name is counted, and nothing is lost. One seen only under a
    /// mount point of another file system is not: its bytes are in the hidden
    /// totals, not in the tree. An inode reached only later is flagged all
    /// the same: the flag errs towards "at least".
    fn lose(&mut self, frame: &Frame, slot: Option<usize>) {
        if frame.shadow {
            return;
        }
        if slot.is_some_and(|s| self.is_counted(s)) {
            return;
        }
        self.listing_lost = true;
    }

    /// Sends the batch and the counts. `false` when the receiver is gone.
    fn flush(&mut self) -> bool {
        let t = std::mem::take(&mut self.tally);
        self.ctx.count(t.allocated, t.logical, t.files, t.dirs);
        self.ctx.count_problems(t.unreadable, t.vanished);
        let batch = std::mem::take(&mut self.batch);
        self.ctx.send(batch)
    }

    fn flush_if_due(&mut self) -> bool {
        if self.batch.len() >= FLUSH_RECORDS || self.batch.name_bytes() >= FLUSH_NAME_BYTES {
            return self.flush();
        }
        true
    }

    /// Walks the tree and sends it. The root record goes first.
    pub fn run(&mut self, root_name: &[u8]) -> Result<ScanEnd, Ext4Error> {
        let Located::Used(root, root_slot) = self.model.locate(ROOT_INODE) else {
            return Err(Ext4Error::BadRoot("the root inode is not in use"));
        };
        if !root.is_dir() {
            return Err(Ext4Error::BadRoot("the root inode is not a directory"));
        }
        let key = self
            .ctx
            .next_dir_key()
            .ok_or(Ext4Error::BadRoot("no directory keys"))?;
        self.test_and_set(root_slot);
        self.mark_counted(root_slot);
        let meta = meta_for(&root, NodeKind::Dir, ROOT_INODE, self.volume);
        self.batch
            .dir(key, DirKey::NONE, root_name, &meta)
            .map_err(|_| Ext4Error::TooLarge("root name"))?;
        self.tally.allocated = meta.allocated;
        self.tally.dirs = 1;
        // The root's record goes out alone, before anything refers to it.
        if !self.flush() {
            self.cancelled = true;
            return Ok(ScanEnd::Cancelled);
        }

        let empty: Rc<[u8]> = Rc::from(&b""[..]);
        let root_path: Rc<[u8]> = if self.classifier.is_some() {
            Rc::from(root_name)
        } else {
            Rc::clone(&empty)
        };
        let mut stack = vec![Frame {
            ino: ROOT_INODE,
            key,
            path: PathSpec::Full(root_path),
            trie: Some(0),
            depth: 0,
            shadow: false,
        }];
        while let Some(frame) = stack.pop() {
            if self.ctx.is_cancelled() {
                self.cancelled = true;
                self.flush();
                return Ok(ScanEnd::Cancelled);
            }
            let children = self.list(frame, &empty);
            // First entry on top, so the walk follows the disk's order.
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
    fn list(&mut self, frame: Frame, empty: &Rc<[u8]>) -> Vec<Frame> {
        let model = self.model;
        let mut children = Vec::new();
        let full: Rc<[u8]> = if self.classifier.is_none() {
            Rc::clone(empty)
        } else {
            match &frame.path {
                PathSpec::Full(p) => Rc::clone(p),
                PathSpec::Child {
                    parent,
                    name_off,
                    name_len,
                } => {
                    let start = usize::try_from(*name_off).unwrap_or(usize::MAX);
                    let end = start.saturating_add(usize::from(*name_len));
                    let name = model.names.get(start..end).unwrap_or(&[]);
                    Rc::from(join_path(parent, name, b'/'))
                }
            }
        };
        let place = if frame.shadow {
            None
        } else {
            self.classifier.as_ref().and_then(|c| c.place(&full))
        };
        let slot: Option<DirSlot> = model.dir_slot(frame.ino).copied();
        let mut fail = slot.map_or(DirFail::Corrupt, |s| s.fail);
        self.listing_lost = false;
        if let Some(slot) = &slot {
            for entry in model.entries_of(slot) {
                self.entry(&frame, &full, place, entry, &mut children);
                if !frame.shadow && !self.flush_if_due() {
                    self.cancelled = true;
                    break;
                }
            }
        }
        if self.listing_lost && fail == DirFail::None {
            // An entry was dropped and the inode behind it is in use and not
            // counted anywhere else, so bytes may be missing from this
            // directory. It must not be reported as completely listed.
            fail = DirFail::Corrupt;
        }
        if !frame.shadow {
            match fail {
                DirFail::None => self.batch.listing_done(frame.key),
                DirFail::Corrupt | DirFail::Io => {
                    self.batch
                        .unreadable(frame.key, failure(fail == DirFail::Io));
                    bump(&mut self.tally.unreadable);
                    bump(&mut self.unreadable_entries);
                }
            }
        }
        children
    }

    fn entry(
        &mut self,
        frame: &Frame,
        full: &Rc<[u8]>,
        place: Option<Category>,
        entry: &EntryRec,
        children: &mut Vec<Frame>,
    ) {
        let model = self.model;
        let name = model.name_of(entry);
        let child = entry.child;
        let (rec, slot) = match model.locate(child) {
            Located::OutOfRange => {
                self.diag
                    .note(ProblemKind::DirentBadInode, frame.ino, u64::from(child));
                // Nothing says what the entry stood for.
                self.lose(frame, None);
                return;
            }
            Located::Free => {
                // Freed since the inode tables were read, or damage; either
                // way there is nothing to report.
                self.diag
                    .note(ProblemKind::DirentToFreeInode, frame.ino, u64::from(child));
                bump(&mut self.vanished_entries);
                if !frame.shadow {
                    bump(&mut self.tally.vanished);
                }
                return;
            }
            Located::Unreadable { io } => {
                if !frame.shadow {
                    self.unreadable_entry(frame, name, entry, io);
                }
                return;
            }
            Located::Used(rec, slot) => (rec, slot),
        };
        if child < model.geo.first_ino {
            self.diag.note(
                ProblemKind::DirentToReservedInode,
                frame.ino,
                u64::from(child),
            );
            self.lose(frame, Some(slot));
            return;
        }
        let kind = kind_of_mode(rec.mode);
        if kind_of_file_type(entry.file_type).is_some_and(|announced| announced != kind) {
            // The entry and the inode disagree about what this is: the inode
            // was freed and reused, or the directory is damaged.
            self.diag
                .note(ProblemKind::DirentTypeMismatch, frame.ino, u64::from(child));
            self.lose(frame, Some(slot));
            return;
        }

        if kind == NodeKind::Dir {
            self.dir_entry(frame, full, entry, &rec, slot, children);
        } else {
            self.file_entry(frame, place, name, &rec, slot, child, kind);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn file_entry(
        &mut self,
        frame: &Frame,
        place: Option<Category>,
        name: &[u8],
        rec: &InodeRec,
        slot: usize,
        ino: u32,
        kind: NodeKind,
    ) {
        let first_sight = !self.test_and_set(slot);
        if frame.shadow {
            if first_sight {
                self.hidden.files = self.hidden.files.saturating_add(1);
                self.hidden.allocated = self
                    .hidden
                    .allocated
                    .saturating_add(rec.blocks512.saturating_mul(512));
                self.hidden.logical = self.hidden.logical.saturating_add(rec.size);
            }
            return;
        }
        self.mark_counted(slot);
        let mut meta = meta_for(rec, kind, ino, self.volume);
        if let Some(classifier) = &self.classifier {
            meta.category = classifier.category(place, kind, name);
        }
        bump(&mut self.tally.files);
        self.tally.allocated = self.tally.allocated.saturating_add(meta.allocated);
        self.tally.logical = self.tally.logical.saturating_add(meta.logical);
        // Names are at most 255 bytes, so the batch cannot be too full for one.
        self.batch.file(kind, frame.key, name, &meta).ok();
    }

    fn dir_entry(
        &mut self,
        frame: &Frame,
        full: &Rc<[u8]>,
        entry: &EntryRec,
        rec: &InodeRec,
        slot: usize,
        children: &mut Vec<Frame>,
    ) {
        let ino = entry.child;
        let name = self.model.name_of(entry);
        if self.is_reached(slot) {
            // A second way into a directory: a loop, or a hard-linked
            // directory. The first way stands.
            self.diag
                .note(ProblemKind::DirectoryLinkedTwice, frame.ino, u64::from(ino));
            return;
        }
        let Some(dir) = self.model.dir_slot(ino) else {
            self.diag
                .note(ProblemKind::InodeInvalid, frame.ino, u64::from(ino));
            if !frame.shadow {
                self.unreadable_entry(frame, name, entry, false);
            }
            return;
        };
        if dir.dotdot == 0 {
            self.diag
                .note(ProblemKind::DirectoryNoParent, frame.ino, u64::from(ino));
        } else if dir.dotdot != frame.ino {
            // Its own `..` says it belongs elsewhere. Either this entry is
            // stale (a rename in progress) or the directory is damaged.
            self.diag.note(
                ProblemKind::DirectoryParentMismatch,
                frame.ino,
                u64::from(ino),
            );
            self.lose(frame, Some(slot));
            return;
        }
        let depth = frame.depth.saturating_add(1);
        let child_path = |parent: &Rc<[u8]>| PathSpec::Child {
            parent: Rc::clone(parent),
            name_off: entry.name_off,
            name_len: entry.name_len,
        };
        let own_bytes = rec.blocks512.saturating_mul(512);
        if frame.shadow {
            self.test_and_set(slot);
            self.hidden.dirs = self.hidden.dirs.saturating_add(1);
            self.hidden.allocated = self.hidden.allocated.saturating_add(own_bytes);
            if depth <= MAX_DEPTH {
                children.push(Frame {
                    ino,
                    key: frame.key,
                    path: child_path(full),
                    trie: None,
                    depth,
                    shadow: true,
                });
            }
            return;
        }
        let Some(key) = self.ctx.next_dir_key() else {
            // Out of keys: it cannot be described as a directory.
            self.test_and_set(slot);
            self.mark_counted(slot);
            let meta = Meta {
                failure: Some(ReadFailure {
                    kind: ReadErrorKind::Other,
                    code: 0,
                }),
                ..meta_for(rec, NodeKind::Other, ino, self.volume)
            };
            bump(&mut self.tally.unreadable);
            bump(&mut self.unreadable_entries);
            self.batch
                .file(NodeKind::Other, frame.key, name, &meta)
                .ok();
            return;
        };
        self.test_and_set(slot);
        bump(&mut self.tally.dirs);
        let trie = frame.trie.and_then(|t| self.skip.child(t, name));
        if trie.is_some_and(|t| self.skip.is_mount(t)) {
            // Another file system is mounted here. What this file system
            // holds under the mount point is hidden by it, as it is from the
            // walker, so the entry is reported empty and its contents are
            // only counted.
            let meta = Meta {
                mtime: rec.mtime,
                flags: NodeFlags::NOT_FOLLOWED,
                ..Meta::default()
            };
            self.batch.dir(key, frame.key, name, &meta).ok();
            self.batch.listing_done(key);
            self.hidden.dirs = self.hidden.dirs.saturating_add(1);
            self.hidden.allocated = self.hidden.allocated.saturating_add(own_bytes);
            children.push(Frame {
                ino,
                key,
                path: child_path(full),
                trie: None,
                depth,
                shadow: true,
            });
            return;
        }
        self.mark_counted(slot);
        let mut meta = meta_for(rec, NodeKind::Dir, ino, self.volume);
        if depth > MAX_DEPTH {
            self.diag
                .note(ProblemKind::TooDeep, frame.ino, u64::from(ino));
            meta.failure = Some(ReadFailure {
                kind: ReadErrorKind::TooDeep,
                code: 0,
            });
            bump(&mut self.tally.unreadable);
            bump(&mut self.unreadable_entries);
        }
        self.tally.allocated = self.tally.allocated.saturating_add(meta.allocated);
        self.batch.dir(key, frame.key, name, &meta).ok();
        if depth <= MAX_DEPTH {
            children.push(Frame {
                ino,
                key,
                path: child_path(full),
                trie,
                depth,
                shadow: false,
            });
        }
    }

    /// An entry whose inode could not be read: kept, flagged, with what the
    /// listing says it is.
    fn unreadable_entry(&mut self, frame: &Frame, name: &[u8], entry: &EntryRec, io: bool) {
        let mut kind = kind_of_file_type(entry.file_type).unwrap_or(NodeKind::Other);
        let meta = Meta {
            failure: Some(failure(io)),
            ..Meta::default()
        };
        bump(&mut self.tally.unreadable);
        bump(&mut self.unreadable_entries);
        if kind == NodeKind::Dir {
            if let Some(key) = self.ctx.next_dir_key() {
                bump(&mut self.tally.dirs);
                self.batch.dir(key, frame.key, name, &meta).ok();
                return;
            }
            kind = NodeKind::Other;
        }
        bump(&mut self.tally.files);
        self.batch.file(kind, frame.key, name, &meta).ok();
    }

    /// Counts what is on disk but was not reached from the root, and the
    /// reserved inodes (journal, quota files, resize inode). Also returns
    /// the number of inodes in use.
    pub fn account_for_the_rest(&self) -> (Unaccounted, Unaccounted, u64) {
        let model = self.model;
        let mut unreachable = Unaccounted::default();
        let mut reserved = Unaccounted::default();
        let mut in_use = 0u64;
        let ipg = u64::from(model.geo.inodes_per_group);
        for (g, group) in model.groups.iter().enumerate() {
            let base = u64::from(model.slot_base_of(g));
            for i in 0..u64::from(group.used_inodes) {
                let slot = usize::try_from(base.saturating_add(i)).unwrap_or(usize::MAX);
                let Some(rec) = model.recs.get(slot) else {
                    continue;
                };
                if !rec.is_in_use() {
                    continue;
                }
                in_use = in_use.saturating_add(1);
                let ino = (g as u64)
                    .saturating_mul(ipg)
                    .saturating_add(i)
                    .saturating_add(1);
                let bytes = rec.blocks512.saturating_mul(512);
                if ino < u64::from(model.geo.first_ino) {
                    if ino != u64::from(ROOT_INODE) {
                        reserved.inodes = reserved.inodes.saturating_add(1);
                        reserved.allocated = reserved.allocated.saturating_add(bytes);
                    }
                } else if !self.is_reached(slot) {
                    unreachable.inodes = unreachable.inodes.saturating_add(1);
                    unreachable.allocated = unreachable.allocated.saturating_add(bytes);
                }
            }
        }
        (unreachable, reserved, in_use)
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

    #[test]
    fn skip_set_matches_whole_paths() {
        let set = SkipSet::new(&[b"proc".to_vec(), b"/mnt/data/".to_vec(), Vec::new()]);
        let proc = set.child(0, b"proc").unwrap();
        assert!(set.is_mount(proc));
        let mnt = set.child(0, b"mnt").unwrap();
        assert!(!set.is_mount(mnt));
        assert!(set.is_mount(set.child(mnt, b"data").unwrap()));
        assert!(set.child(0, b"data").is_none());
        // The empty path names the root, which is never a mount to skip.
        assert!(!set.is_mount(0));
    }

    #[test]
    fn types_and_modes() {
        assert_eq!(kind_of_mode(0x81A4), NodeKind::File);
        assert_eq!(kind_of_mode(0x41ED), NodeKind::Dir);
        assert_eq!(kind_of_mode(0xA1FF), NodeKind::Symlink);
        assert_eq!(kind_of_mode(0x61B0), NodeKind::Other);
        assert_eq!(kind_of_file_type(0), None);
        assert_eq!(kind_of_file_type(0xDE), None);
        assert_eq!(kind_of_file_type(2), Some(NodeKind::Dir));
    }
}
