//! Turning the walk and the inode table into scanner batches: the traversal
//! of the listings from the root with an explicit stack, each entry joined
//! with its inode's numbers.
//!
//! The sizes are those the walker reports: the logical size is the file's
//! size (nothing for a directory), the allocated size the blocks the inode
//! holds in bytes, hard links are keyed by device and inode number and counted
//! once by the tree, and links are flagged and never followed.

use crate::error::{Diagnostics, ProblemKind};
use crate::walk::{ListedEntry, Listing, NONE, Stat, Status, Table, Walked};
use crate::{Unaccounted, XfsError};
use bs_scan::{Classifier, MAX_PATH_BYTES, ScanContext, ScanEnd, join_path};
use bs_tree::{
    Batch, Category, DirKey, FileKey, Meta, NodeFlags, NodeKind, ReadErrorKind, ReadFailure,
};
use std::rc::Rc;

/// A file smaller on disk than its length by at least this much is sparse.
const SPARSE_MARGIN: u64 = 4096;
/// What the scan's own tree costs per entry besides the name's bytes.
pub(crate) const TREE_BYTES_PER_ENTRY: u64 = 96;

const FLUSH_RECORDS: usize = 4096;
const FLUSH_NAME_BYTES: usize = 256 * 1024;

const CODE_CORRUPT: i32 = 117;

const DT_UNKNOWN: u8 = 0;
const DT_FIFO: u8 = 1;
const DT_CHR: u8 = 2;
const DT_DIR: u8 = 4;
const DT_BLK: u8 = 6;
const DT_REG: u8 = 8;
const DT_LNK: u8 = 10;
const DT_SOCK: u8 = 12;

struct Frame {
    /// The directory's inode number.
    ino: u64,
    listing: u32,
    key: DirKey,
    path: Rc<[u8]>,
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

fn corrupt() -> ReadFailure {
    ReadFailure {
        kind: ReadErrorKind::Corrupt,
        code: CODE_CORRUPT,
    }
}

fn kind_of_mode(mode: u32) -> Option<NodeKind> {
    match mode & libc::S_IFMT {
        libc::S_IFREG => Some(NodeKind::File),
        libc::S_IFDIR => Some(NodeKind::Dir),
        libc::S_IFLNK => Some(NodeKind::Symlink),
        libc::S_IFCHR | libc::S_IFBLK | libc::S_IFIFO | libc::S_IFSOCK => Some(NodeKind::Other),
        _ => None,
    }
}

/// Whether the type a listing gives agrees with the inode's mode. A listing
/// that does not know agrees with anything.
fn d_type_agrees(d_type: u8, mode: u32) -> bool {
    let want = match mode & libc::S_IFMT {
        libc::S_IFREG => DT_REG,
        libc::S_IFDIR => DT_DIR,
        libc::S_IFLNK => DT_LNK,
        libc::S_IFCHR => DT_CHR,
        libc::S_IFBLK => DT_BLK,
        libc::S_IFIFO => DT_FIFO,
        libc::S_IFSOCK => DT_SOCK,
        _ => return false,
    };
    d_type == DT_UNKNOWN || d_type == want
}

pub(crate) struct Emitter<'a> {
    walked: &'a Walked,
    table: &'a Table,
    ctx: &'a ScanContext,
    classifier: Option<Classifier>,
    reached: Vec<bool>,
    path_room: u64,
    path_live: u64,
    batch: Batch,
    tally: Tally,
    pub diag: Diagnostics,
    pub unreadable_entries: u64,
    pub vanished_entries: u64,
    pub cancelled: bool,
}

impl<'a> Emitter<'a> {
    pub fn new(
        walked: &'a Walked,
        table: &'a Table,
        ctx: &'a ScanContext,
        limit: u64,
    ) -> Result<Emitter<'a>, XfsError> {
        let entries: u64 = walked.listings.iter().map(|l| l.entries.len() as u64).sum();
        let names: u64 = walked.listings.iter().map(|l| l.names.len() as u64).sum();
        let needed = walked
            .memory
            .saturating_add(entries.saturating_mul(TREE_BYTES_PER_ENTRY))
            .saturating_add(names)
            .saturating_add(table.rows.len() as u64);
        if needed > limit {
            return Err(XfsError::TooLarge(
                "the tree and the walk need more memory than the limit allows",
            ));
        }
        Ok(Emitter {
            walked,
            table,
            ctx,
            classifier: ctx.classifier(),
            reached: vec![false; table.rows.len()],
            path_room: limit.saturating_sub(needed),
            path_live: 0,
            batch: Batch::new(),
            tally: Tally::default(),
            diag: Diagnostics::default(),
            unreadable_entries: 0,
            vanished_entries: 0,
            cancelled: false,
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

    fn close(&mut self, key: DirKey, failure: Option<ReadFailure>) {
        if let Some(f) = failure {
            self.batch.unreadable(key, f);
            bump(&mut self.tally.unreadable);
            bump(&mut self.unreadable_entries);
        } else {
            self.batch.listing_done(key);
        }
    }

    /// The numbers of an entry: the inode table's, or what a direct lookup
    /// said for an entry the table did not hold.
    fn stat_of(&self, listing: &Listing, index: usize, e: &ListedEntry) -> Option<Stat> {
        if e.row != NONE {
            return self.table.get(e.row).copied();
        }
        listing.extra(index).copied()
    }

    pub fn run(&mut self, root_name: &[u8]) -> Result<ScanEnd, XfsError> {
        let walked = self.walked;
        let root_stat = Stat::from(walked.root);
        let key = self
            .ctx
            .next_dir_key()
            .ok_or(XfsError::BadRoot("no directory keys"))?;
        if kind_of_mode(root_stat.mode) != Some(NodeKind::Dir) {
            return Err(XfsError::BadRoot("the start is not a directory"));
        }
        let meta = dir_meta(&root_stat);
        self.batch
            .dir(key, DirKey::NONE, root_name, &meta)
            .map_err(|_| XfsError::TooLarge("root name"))?;
        self.tally.dirs = 1;
        self.tally.allocated = meta.allocated;
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
            ino: root_stat.ino,
            listing: 0,
            key,
            path: root_path,
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
            // First entry on top, so the walk follows the listing's order.
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

    fn list(&mut self, frame: &Frame) -> Vec<Frame> {
        let walked = self.walked;
        let mut children = Vec::new();
        let Some(listing) = walked
            .listings
            .get(usize::try_from(frame.listing).unwrap_or(usize::MAX))
        else {
            self.close(frame.key, Some(corrupt()));
            return children;
        };
        let place = self.classifier.as_ref().and_then(|c| c.place(&frame.path));
        let mut broken = false;
        for (index, entry) in listing.entries.iter().enumerate() {
            if !self.entry(frame, place, listing, index, entry, &mut children) {
                broken = true;
            }
            if !self.flush_if_due() {
                self.cancelled = true;
                break;
            }
        }
        let failure = listing.failure.or_else(|| broken.then(corrupt));
        self.close(frame.key, failure);
        children
    }

    /// Emits one entry. `false` when it had to be dropped, so that its
    /// directory cannot claim to be complete.
    fn entry(
        &mut self,
        frame: &Frame,
        place: Option<Category>,
        listing: &Listing,
        index: usize,
        entry: &ListedEntry,
        children: &mut Vec<Frame>,
    ) -> bool {
        let name = listing.name(entry);
        let dir_ino = frame.ino;
        if !entry.clean || name.is_empty() || name.contains(&b'/') {
            self.diag
                .note(ProblemKind::EntryBadName, dir_ino, entry.ino);
            return false;
        }
        // The child listing says what the walk found when it came to the
        // entry; an entry that was replaced is an ordinary entry.
        let child = (entry.child != NONE)
            .then(|| {
                self.walked
                    .listings
                    .get(usize::try_from(entry.child).unwrap_or(usize::MAX))
            })
            .flatten();
        let stat = self.stat_of(listing, index, entry);
        if stat.is_none() && child.is_some_and(|c| c.status == Status::Outside) {
            // A mount the scan does not enter, whose covered directory the
            // inode table does not describe: still shown, as not followed.
            if let Some(key) = self.ctx.next_dir_key() {
                let meta = Meta {
                    flags: NodeFlags::NOT_FOLLOWED,
                    ..Meta::default()
                };
                bump(&mut self.tally.dirs);
                self.batch.dir(key, frame.key, name, &meta).ok();
                self.batch.listing_done(key);
                return true;
            }
        }
        let Some(st) = stat else {
            // Removed between the walk and the lookup.
            self.diag
                .note(ProblemKind::EntryToMissingInode, dir_ino, entry.ino);
            bump(&mut self.vanished_entries);
            return true;
        };
        if entry.row != NONE
            && let Some(seen) = self
                .reached
                .get_mut(usize::try_from(entry.row).unwrap_or(usize::MAX))
        {
            *seen = true;
        }
        let Some(kind) = kind_of_mode(st.mode) else {
            self.diag
                .note(ProblemKind::InodeInvalid, dir_ino, entry.ino);
            let meta = Meta {
                mtime: st.mtime,
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
        match child.map(|c| c.status) {
            Some(Status::Gone) => {
                // Removed between the listing and the walk.
                self.diag
                    .note(ProblemKind::EntryToMissingInode, dir_ino, entry.ino);
                bump(&mut self.vanished_entries);
                return true;
            }
            Some(Status::Duplicate) => {
                self.diag
                    .note(ProblemKind::DirectoryLinkedTwice, dir_ino, entry.ino);
                return false;
            }
            _ => {}
        }
        let replaced = child.is_some_and(|c| c.status == Status::Replaced);
        if !replaced && !d_type_agrees(entry.d_type, st.mode) {
            self.diag
                .note(ProblemKind::EntryTypeMismatch, dir_ino, entry.ino);
            return false;
        }
        if kind == NodeKind::Dir {
            let Some(child) = child.filter(|c| c.status != Status::Replaced) else {
                // A directory the walk did not come to (a name it could not
                // open by): its numbers are all there is.
                self.diag
                    .note(ProblemKind::EntryTypeMismatch, dir_ino, entry.ino);
                return false;
            };
            // A mount entered under its own inode: its numbers, not those of
            // the directory it covers.
            let st = match child.mounted {
                Some(mounted) => {
                    if let Some(seen) = self
                        .table
                        .find(mounted.ino)
                        .and_then(|row| self.reached.get_mut(usize::try_from(row).ok()?))
                    {
                        *seen = true;
                    }
                    mounted
                }
                None => st,
            };
            return self.dir_entry(frame, st, name, entry.child, child, children);
        }
        self.file_entry(frame, place, st, kind, name);
        true
    }

    fn dir_entry(
        &mut self,
        frame: &Frame,
        st: Stat,
        name: &[u8],
        listing_id: u32,
        child: &Listing,
        children: &mut Vec<Frame>,
    ) -> bool {
        let Some(key) = self.ctx.next_dir_key() else {
            let meta = Meta {
                failure: Some(ReadFailure {
                    kind: ReadErrorKind::Other,
                    code: 0,
                }),
                mtime: st.mtime,
                ..Meta::default()
            };
            bump(&mut self.tally.unreadable);
            bump(&mut self.unreadable_entries);
            bump(&mut self.tally.files);
            self.batch
                .file(NodeKind::Other, frame.key, name, &meta)
                .ok();
            return true;
        };
        match child.status {
            Status::Outside => {
                // Another drive's mount. The numbers describe its root, not
                // an entry of this file system, so none are counted.
                let meta = Meta {
                    mtime: st.mtime,
                    flags: NodeFlags::NOT_FOLLOWED,
                    ..Meta::default()
                };
                bump(&mut self.tally.dirs);
                self.batch.dir(key, frame.key, name, &meta).ok();
                self.batch.listing_done(key);
            }
            Status::Failed(f) => {
                let meta = dir_meta(&st);
                bump(&mut self.tally.dirs);
                self.tally.allocated = self.tally.allocated.saturating_add(meta.allocated);
                self.batch.dir(key, frame.key, name, &meta).ok();
                self.close(key, Some(f));
            }
            Status::Listed | Status::Replaced | Status::Gone | Status::Duplicate => {
                let meta = dir_meta(&st);
                bump(&mut self.tally.dirs);
                self.tally.allocated = self.tally.allocated.saturating_add(meta.allocated);
                self.batch.dir(key, frame.key, name, &meta).ok();
                let mut own_path = 0u64;
                let path: Rc<[u8]> = if self.classifier.is_some() {
                    // The path of every directory waiting its turn is held in
                    // memory; past what the limit leaves, a directory shares
                    // its parent's path and only its category lookups lose
                    // precision.
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
                    ino: st.ino,
                    listing: listing_id,
                    key,
                    path,
                    own_path,
                });
            }
        }
        true
    }

    fn file_entry(
        &mut self,
        frame: &Frame,
        place: Option<Category>,
        st: Stat,
        kind: NodeKind,
        name: &[u8],
    ) {
        let mut flags = NodeFlags::EMPTY;
        let mut link = None;
        if kind == NodeKind::Symlink {
            flags |= NodeFlags::NOT_FOLLOWED;
        }
        if kind == NodeKind::File && st.allocated.saturating_add(SPARSE_MARGIN) <= st.size {
            flags |= NodeFlags::SPARSE_OR_COMPRESSED;
        }
        if st.nlink > 1 {
            // More than one name: flagged whether or not the others are in
            // the scan; keyed so the tree counts the bytes once.
            flags |= NodeFlags::HARD_LINK;
            link = Some(FileKey {
                volume: self.walked.device,
                id: u128::from(st.ino),
            });
        }
        let mut meta = Meta {
            logical: st.size,
            allocated: st.allocated,
            mtime: st.mtime,
            flags,
            link,
            ..Meta::default()
        };
        if let Some(classifier) = &self.classifier {
            meta.category = classifier.category(place, kind, name);
        }
        bump(&mut self.tally.files);
        self.tally.allocated = self.tally.allocated.saturating_add(st.allocated);
        self.tally.logical = self.tally.logical.saturating_add(st.size);
        self.batch.file(kind, frame.key, name, &meta).ok();
    }

    /// Counts the inodes of the table that no entry of the scan reached,
    /// and their bytes (directories included).
    pub fn account_for_the_rest(&self) -> (Unaccounted, u64) {
        let mut out = Unaccounted::default();
        for (i, row) in self.table.rows.iter().enumerate() {
            if self.reached.get(i).copied().unwrap_or(false) || row.ino == self.walked.root.ino {
                continue;
            }
            out.inodes = out.inodes.saturating_add(1);
            out.allocated = out.allocated.saturating_add(row.allocated);
        }
        (out, self.table.rows.len() as u64)
    }
}

/// A directory's own entry: its blocks count toward size on disk, its length
/// adds nothing to the apparent size.
fn dir_meta(st: &Stat) -> Meta {
    Meta {
        allocated: st.allocated,
        mtime: st.mtime,
        ..Meta::default()
    }
}
