//! Loading the trees: one pass over each subvolume's tree, keeping what the
//! reader needs of every inode, directory entry and file extent, and a table
//! of the disk extents with a note of who else refers to them.
//!
//! Nothing here trusts the search results more than the parsers do: every
//! item is decoded with checked offsets, the memory the tables take is
//! charged as it grows and held to the caller's limit, and a tree that cannot
//! be read to its end is dropped whole, so that no half-read directory is
//! ever presented as a whole one.

use crate::BtrfsError;
use crate::error::{Diagnostics, ProblemKind};
use crate::items::{
    self, DIR_INDEX, EXTENT_DATA, Extent, FIRST_FREE, FS_TREE_ID, INODE_ITEM, LAST_FREE, ROOT_ITEM,
    ROOT_REF, ROOT_TREE_ID, Target,
};
use crate::search::{Flow, Item, Key, TreeSource};
use std::collections::{HashMap, HashSet, VecDeque};
use std::ops::Range;

/// Nested subvolumes followed, beyond the first tree. A file system has few;
/// a hostile one can name any number of ids.
pub(crate) const MAX_TREES: usize = 65_536;
/// Subvolumes looked at to find out who else shares an extent.
pub(crate) const MAX_CENSUS_TREES: usize = 1 << 20;
/// Disk addresses named by damaged extent items that are remembered one by
/// one; past this, damage makes everything shared.
pub(crate) const MAX_DAMAGED_REFS: usize = 4096;
/// Root references looked up to place nested subvolumes, over a whole read.
/// Past this, entries are left unplaced and shown as unreadable.
pub(crate) const MAX_REF_LOOKUPS: u64 = MAX_TREES as u64;

pub(crate) const F_DAMAGED: u8 = 1;
pub(crate) const F_COMPRESSED: u8 = 2;

/// A memory limit that is charged as the tables grow.
#[derive(Debug)]
pub(crate) struct Budget {
    limit: u64,
    used: u64,
}

impl Budget {
    pub fn new(limit: u64) -> Budget {
        Budget { limit, used: 0 }
    }

    /// Charges `bytes`; `false` once the limit is passed (the charge stays).
    pub fn add(&mut self, bytes: u64) -> bool {
        self.used = self.used.saturating_add(bytes);
        self.used <= self.limit
    }

    /// Whether `bytes` more would still fit.
    fn fits(&self, bytes: u64) -> bool {
        self.used.saturating_add(bytes) <= self.limit
    }

    pub fn used(&self) -> u64 {
        self.used
    }
}

const OVER: BtrfsError =
    BtrfsError::TooLarge("the file system's trees need more memory than the limit allows");

/// One inode of one tree.
#[derive(Clone, Copy, Debug)]
pub(crate) struct InodeRec {
    pub ino: u64,
    pub size: u64,
    pub mtime: i64,
    pub nlink: u32,
    pub mode: u32,
    pub entries_start: u32,
    pub entries_len: u32,
    pub extents_start: u32,
    pub extents_len: u32,
    /// Bytes of data kept inside the tree.
    pub inline_bytes: u64,
    pub flags: u8,
}

/// One name in a directory.
#[derive(Clone, Copy, Debug)]
pub(crate) struct EntryRec {
    pub name_off: u32,
    pub name_len: u16,
    pub ty: u8,
    pub subvolume: bool,
    /// A subvolume entry that the kernel shows as an empty directory, because
    /// the root tree does not nest that subvolume here. A snapshot keeps the
    /// entries of the subvolume it was taken from, including those of
    /// subvolumes nested in it, but not the nesting itself.
    pub stub: bool,
    /// A subvolume entry that the root tree confirms: a well-formed root
    /// reference names this directory and this name. Only a confirmed entry
    /// is entered; one that is neither confirmed nor a stand-in could not be
    /// placed, and it is shown unreadable.
    pub confirmed: bool,
    pub target: u64,
}

/// A disk extent and who refers to it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ExtInfo {
    /// The first inode (by position in the inode table) that referred to it.
    pub first: u32,
    pub bytes: u64,
    /// Something other than `first` refers to it too: another inode, or
    /// anything in a subvolume the census looked at.
    pub foreign: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct TreeRec {
    pub id: u64,
    pub inodes: Range<u32>,
    /// The tree was read to its end.
    pub ok: bool,
}

/// Where the scan starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Start {
    /// The subvolume's tree id.
    pub tree: u64,
    /// The directory's inode number in that tree (256 for a subvolume's
    /// root).
    pub inode: u64,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Limits {
    pub max_memory_bytes: u64,
    /// Whether to look in the subvolumes outside the scan for references to
    /// the extents inside it.
    pub census: bool,
}

pub(crate) struct Model {
    pub trees: Vec<TreeRec>,
    pub tree_index: HashMap<u64, u32>,
    pub inodes: Vec<InodeRec>,
    pub entries: Vec<EntryRec>,
    pub names: Vec<u8>,
    /// One per file extent item that names disk blocks: an index into `exts`.
    pub extent_refs: Vec<u32>,
    pub exts: Vec<ExtInfo>,
    ext_index: HashMap<u64, u32>,
    /// Disk addresses that a damaged extent item still names: whatever
    /// refers to them is shared, since the damaged item refers to them too.
    damaged_refs: HashSet<u64>,
    pub budget: Budget,
    /// Subvolumes of the file system that the census read.
    pub census_trees: u64,
    /// The census could not read some subvolume, so what is shared is not
    /// known and every extent is taken as shared.
    pub sharing_unknown: bool,
    pub dropped_trees: u64,
    /// Searches made of the root tree to place nested subvolumes.
    ref_lookups: u64,
    /// A root tree search failed: no more are made, and the entries not yet
    /// placed stay unplaced.
    refs_failed: bool,
}

impl Model {
    fn new(limit: u64) -> Model {
        Model {
            trees: Vec::new(),
            tree_index: HashMap::new(),
            inodes: Vec::new(),
            entries: Vec::new(),
            names: Vec::new(),
            extent_refs: Vec::new(),
            exts: Vec::new(),
            ext_index: HashMap::new(),
            damaged_refs: HashSet::new(),
            budget: Budget::new(limit),
            census_trees: 0,
            sharing_unknown: false,
            dropped_trees: 0,
            ref_lookups: 0,
            refs_failed: false,
        }
    }

    pub fn name_bytes(&self, e: &EntryRec) -> &[u8] {
        let start = usize::try_from(e.name_off).unwrap_or(usize::MAX);
        let end = start.saturating_add(usize::from(e.name_len));
        self.names.get(start..end).unwrap_or(&[])
    }

    /// The position of inode `ino` of tree number `tree` in the inode table.
    pub fn find_inode(&self, tree: u32, ino: u64) -> Option<u32> {
        let rec = self.trees.get(usize::try_from(tree).ok()?)?;
        if !rec.ok {
            return None;
        }
        let range =
            usize::try_from(rec.inodes.start).ok()?..usize::try_from(rec.inodes.end).ok()?;
        let slice = self.inodes.get(range)?;
        let at = slice.binary_search_by_key(&ino, |i| i.ino).ok()?;
        rec.inodes.start.checked_add(u32::try_from(at).ok()?)
    }

    pub fn inode(&self, at: u32) -> Option<&InodeRec> {
        self.inodes.get(usize::try_from(at).ok()?)
    }

    pub fn entries_of(&self, rec: &InodeRec) -> &[EntryRec] {
        let start = usize::try_from(rec.entries_start).unwrap_or(usize::MAX);
        let end = start.saturating_add(usize::try_from(rec.entries_len).unwrap_or(usize::MAX));
        self.entries.get(start..end).unwrap_or(&[])
    }

    pub fn extents_of(&self, rec: &InodeRec) -> &[u32] {
        let start = usize::try_from(rec.extents_start).unwrap_or(usize::MAX);
        let end = start.saturating_add(usize::try_from(rec.extents_len).unwrap_or(usize::MAX));
        self.extent_refs.get(start..end).unwrap_or(&[])
    }

    pub fn ext(&self, index: u32) -> Option<&ExtInfo> {
        self.exts.get(usize::try_from(index).ok()?)
    }

    /// The memory the tables hold, for planning what the emission may add.
    pub fn bytes(&self) -> u64 {
        self.budget.used()
    }
}

/// What loading one tree found, apart from the tables.
struct TreeLoad {
    /// Subvolumes nested in this tree, in order of appearance.
    children: Vec<u64>,
    new_extents: Vec<u64>,
}

struct Rollback {
    inodes: usize,
    entries: usize,
    names: usize,
    refs: usize,
    exts: usize,
}

fn snapshot_lengths(m: &Model) -> Rollback {
    Rollback {
        inodes: m.inodes.len(),
        entries: m.entries.len(),
        names: m.names.len(),
        refs: m.extent_refs.len(),
        exts: m.exts.len(),
    }
}

/// Loads every tree reachable from `start` by nested subvolumes, and the
/// `mounted` trees the scan enters from mounts, then, if asked, takes the
/// census of the others.
///
/// `Ok(None)` when cancelled.
pub(crate) fn load(
    src: &dyn TreeSource,
    start: Start,
    mounted: &[u64],
    limits: Limits,
    cancelled: &(dyn Fn() -> bool + Sync),
    diag: &mut Diagnostics,
) -> Result<Option<Model>, BtrfsError> {
    let mut model = Model::new(limits.max_memory_bytes);
    let mut queue: VecDeque<u64> = VecDeque::new();
    queue.push_back(start.tree);
    queue.extend(mounted.iter().copied());
    let mut first = true;
    while let Some(id) = queue.pop_front() {
        if model.tree_index.contains_key(&id) {
            continue;
        }
        if model.trees.len() >= MAX_TREES {
            diag.note(ProblemKind::SubvolumeSkipped, id, 0);
            continue;
        }
        let index = u32::try_from(model.trees.len()).map_err(|_| OVER)?;
        let marks = snapshot_lengths(&model);
        let first_inode = u32::try_from(model.inodes.len()).map_err(|_| OVER)?;
        match load_tree(src, id, &mut model, cancelled, diag) {
            Ok(Some(done)) => {
                let end = u32::try_from(model.inodes.len()).map_err(|_| OVER)?;
                model.trees.push(TreeRec {
                    id,
                    inodes: first_inode..end,
                    ok: true,
                });
                model.tree_index.insert(id, index);
                queue.extend(done.children);
            }
            Ok(None) => return Ok(None),
            Err(error) => {
                if first {
                    return Err(error);
                }
                // A nested subvolume that cannot be read is dropped whole.
                if matches!(error, BtrfsError::TooLarge(_)) {
                    return Err(error);
                }
                rollback(&mut model, &marks);
                model.trees.push(TreeRec {
                    id,
                    inodes: first_inode..first_inode,
                    ok: false,
                });
                model.tree_index.insert(id, index);
                model.dropped_trees = model.dropped_trees.saturating_add(1);
                diag.note(ProblemKind::SubvolumeUnreadable, id, 0);
            }
        }
        first = false;
    }
    if model.dropped_trees > 0 {
        // What a subvolume that could not be read refers to is not known, so
        // nothing may be claimed to be used by one file alone.
        model.sharing_unknown = true;
    }
    if !limits.census {
        // Without the other subvolumes nothing can be said to be used by one
        // file alone, so nothing is.
        model.sharing_unknown = true;
    }
    if model.sharing_unknown {
        mark_all_foreign(&mut model);
    }
    if limits.census && !census(src, &mut model, cancelled, diag)? {
        return Ok(None);
    }
    Ok(Some(model))
}

fn rollback(m: &mut Model, marks: &Rollback) {
    m.inodes.truncate(marks.inodes);
    m.entries.truncate(marks.entries);
    m.names.truncate(marks.names);
    m.extent_refs.truncate(marks.refs);
    // The extents this tree introduced go; the notes it left on older ones
    // stay, which can only make something look more shared than it is.
    let gone: HashSet<u32> = (marks.exts..m.exts.len())
        .filter_map(|i| u32::try_from(i).ok())
        .collect();
    m.ext_index.retain(|_, index| !gone.contains(index));
    m.exts.truncate(marks.exts);
}

fn charge(budget: &mut Budget, bytes: usize) -> Result<(), BtrfsError> {
    if budget.add(bytes as u64) {
        Ok(())
    } else {
        Err(OVER)
    }
}

/// What a table of the hash map takes per entry at its worst: the entry, its
/// control byte, the room left for growth, and the old table that lives
/// alongside the new one while it is rebuilt.
const MAP_ENTRY_BYTES: usize = 48;

/// Makes room for `extra` more elements in `v`, charging the memory it really
/// takes. A vector that grows copies itself, and for that moment the old and
/// the new storage both exist, so the new storage must fit under the limit
/// on top of everything charged so far; the charge then settles on the new
/// capacity. Growing by half again keeps the copying amortized.
fn make_room<T>(v: &mut Vec<T>, extra: usize, budget: &mut Budget) -> Result<(), BtrfsError> {
    let needed = v.len().checked_add(extra).ok_or(OVER)?;
    let cap = v.capacity();
    if needed <= cap {
        return Ok(());
    }
    let new_cap = needed.max(cap.saturating_add(cap / 2)).max(256);
    let size = std::mem::size_of::<T>() as u64;
    let old = (cap as u64).saturating_mul(size);
    let new = (new_cap as u64).saturating_mul(size);
    if !budget.fits(new) {
        return Err(OVER);
    }
    v.try_reserve_exact(new_cap.saturating_sub(v.len()))
        .map_err(|_| OVER)?;
    budget.used = budget.used.saturating_sub(old).saturating_add(new);
    Ok(())
}

/// Reads one tree into the tables. `Ok(None)` when cancelled.
fn load_tree(
    src: &dyn TreeSource,
    tree: u64,
    m: &mut Model,
    cancelled: &(dyn Fn() -> bool + Sync),
    diag: &mut Diagnostics,
) -> Result<Option<TreeLoad>, BtrfsError> {
    let mut out = TreeLoad {
        children: Vec::new(),
        new_extents: Vec::new(),
    };
    let from = Key {
        objectid: FIRST_FREE,
        ty: 0,
        offset: 0,
    };
    let to = Key {
        objectid: LAST_FREE,
        ty: u8::MAX,
        offset: u64::MAX,
    };
    let mut failure: Option<BtrfsError> = None;
    let mut stopped = false;
    // The inode the items being read belong to.
    let mut cur: Option<u32> = None;
    let first_inode = m.inodes.len();
    src.search(tree, from, to, &mut |item: Item<'_>| {
        if cancelled() {
            stopped = true;
            return Flow::Stop;
        }
        match handle_item(m, tree, item, &mut cur, &mut out, diag) {
            Ok(()) => Flow::Continue,
            Err(e) => {
                failure = Some(e);
                Flow::Stop
            }
        }
    })?;
    if let Some(e) = failure {
        return Err(e);
    }
    if stopped {
        return Ok(None);
    }
    place_subvolumes(src, tree, first_inode, m, &mut out.children, diag);
    Ok(Some(out))
}

fn handle_item(
    m: &mut Model,
    tree: u64,
    item: Item<'_>,
    cur: &mut Option<u32>,
    out: &mut TreeLoad,
    diag: &mut Diagnostics,
) -> Result<(), BtrfsError> {
    let key = item.key;
    match key.ty {
        INODE_ITEM => {
            make_room(&mut m.inodes, 1, &mut m.budget)?;
            let at = u32::try_from(m.inodes.len()).map_err(|_| OVER)?;
            let entries_start = u32::try_from(m.entries.len()).map_err(|_| OVER)?;
            let extents_start = u32::try_from(m.extent_refs.len()).map_err(|_| OVER)?;
            let mut rec = InodeRec {
                ino: key.objectid,
                size: 0,
                mtime: 0,
                nlink: 1,
                mode: 0,
                entries_start,
                entries_len: 0,
                extents_start,
                extents_len: 0,
                inline_bytes: 0,
                flags: 0,
            };
            match items::parse_inode(item.data) {
                Some(i) => {
                    rec.size = i.size;
                    rec.mtime = i.mtime;
                    rec.nlink = i.nlink;
                    rec.mode = i.mode;
                }
                None => {
                    rec.flags |= F_DAMAGED;
                    diag.note(ProblemKind::ItemMalformed, tree, key.objectid);
                }
            }
            if rec.flags & F_DAMAGED == 0 && items::mode_kind(rec.mode).is_none() {
                rec.flags |= F_DAMAGED;
                diag.note(ProblemKind::InodeInvalid, tree, key.objectid);
            }
            m.inodes.push(rec);
            *cur = Some(at);
        }
        DIR_INDEX | EXTENT_DATA => {
            let Some(at) = *cur else {
                diag.note(ProblemKind::ItemWithoutInode, tree, key.objectid);
                return Ok(());
            };
            let here = m.inodes.get(usize::try_from(at).unwrap_or(usize::MAX));
            if here.is_none_or(|r| r.ino != key.objectid) {
                diag.note(ProblemKind::ItemWithoutInode, tree, key.objectid);
                return Ok(());
            }
            if key.ty == DIR_INDEX {
                dir_index(m, tree, at, item.data, diag)?;
            } else {
                extent_data(m, tree, at, item.data, out, diag)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Settles, for every subvolume entry of the directories just read from
/// `tree`, whether the subvolume is really nested there.
///
/// A directory entry that names a subvolume is not enough. A snapshot is a
/// copy of a subvolume's tree, so it carries the entries of the subvolumes
/// nested in the original; the kernel does not nest them in the snapshot, and
/// shows an empty directory there (`fixup_tree_root_location` and
/// `new_simple_dir` in `fs/btrfs/inode.c`). It decides by looking for a root
/// reference in the tree of trees, keyed (this tree, `ROOT_REF`, the
/// subvolume), that names this directory and this name. So does the reader:
/// without that, the snapshot's entry would claim the original's nested
/// subvolume a second time.
///
/// An entry that is neither confirmed nor refuted (the search failed, too
/// many were made, or the reference item is damaged) is not entered, wherever
/// its subvolume was loaded from: it is shown as an unreadable directory, and
/// what is shared is no longer known. A reference that breaks the kernel's
/// rules for one is damage, not a snapshot's copy, so it does not make a
/// stand-in.
fn place_subvolumes(
    src: &dyn TreeSource,
    tree: u64,
    first_inode: usize,
    m: &mut Model,
    children: &mut Vec<u64>,
    diag: &mut Diagnostics,
) {
    let mut seen: HashSet<u64> = HashSet::new();
    let mut at = first_inode;
    while let Some(rec) = m.inodes.get(at).copied() {
        at = at.saturating_add(1);
        let start = usize::try_from(rec.entries_start).unwrap_or(usize::MAX);
        let end = start.saturating_add(usize::try_from(rec.entries_len).unwrap_or(usize::MAX));
        for index in start..end.min(m.entries.len()) {
            let Some(entry) = m.entries.get(index).copied() else {
                break;
            };
            if !entry.subvolume {
                continue;
            }
            if m.refs_failed || m.ref_lookups >= MAX_REF_LOOKUPS {
                diag.note(ProblemKind::SubvolumeSkipped, entry.target, 0);
                m.sharing_unknown = true;
                continue;
            }
            m.ref_lookups = m.ref_lookups.saturating_add(1);
            let nested = nested_here(src, tree, rec.ino, m.name_bytes(&entry), entry.target);
            match nested {
                Ok(Placement::Nested) => {
                    if let Some(e) = m.entries.get_mut(index) {
                        e.confirmed = true;
                    }
                    if items::is_subvolume_id(entry.target) && seen.insert(entry.target) {
                        children.push(entry.target);
                    }
                }
                Ok(Placement::StandIn) => {
                    if let Some(e) = m.entries.get_mut(index) {
                        e.stub = true;
                    }
                }
                Ok(Placement::Malformed) => {
                    // Damage is not a snapshot's copy: what the reference
                    // should say is not known, so the entry is not placed and
                    // the subvolume's bytes may be missing from the totals.
                    diag.note(ProblemKind::ItemMalformed, tree, entry.target);
                    m.sharing_unknown = true;
                }
                Err(_) => {
                    m.refs_failed = true;
                    diag.note(ProblemKind::SubvolumeSkipped, entry.target, 0);
                    m.sharing_unknown = true;
                }
            }
        }
    }
}

/// What the root tree says about a subvolume entry.
enum Placement {
    /// A well-formed reference names this directory and this name.
    Nested,
    /// No reference, or a well-formed one that names another place: the
    /// kernel's `-ENOENT`, an empty directory.
    StandIn,
    /// An item sits at the key but breaks the kernel's rules for one.
    Malformed,
}

/// Whether the root tree nests subvolume `child` in directory `dir_ino` of
/// `tree`, under `name`.
fn nested_here(
    src: &dyn TreeSource,
    tree: u64,
    dir_ino: u64,
    name: &[u8],
    child: u64,
) -> Result<Placement, BtrfsError> {
    let key = Key {
        objectid: tree,
        ty: ROOT_REF,
        offset: child,
    };
    let mut found = Placement::StandIn;
    src.search(ROOT_TREE_ID, key, key, &mut |item: Item<'_>| {
        if item.key == key {
            found = match items::parse_root_ref(item.data) {
                Some(r) if r.dirid == dir_ino && r.name == name => Placement::Nested,
                Some(_) => Placement::StandIn,
                None => Placement::Malformed,
            };
        }
        Flow::Stop
    })?;
    Ok(found)
}

fn damage(m: &mut Model, at: u32) {
    if let Some(r) = m.inodes.get_mut(usize::try_from(at).unwrap_or(usize::MAX)) {
        r.flags |= F_DAMAGED;
    }
}

fn dir_index(
    m: &mut Model,
    tree: u64,
    at: u32,
    data: &[u8],
    diag: &mut Diagnostics,
) -> Result<(), BtrfsError> {
    for entry in items::dir_entries(data) {
        let Ok(entry) = entry else {
            diag.note(ProblemKind::ItemMalformed, tree, 0);
            damage(m, at);
            break;
        };
        make_room(&mut m.entries, 1, &mut m.budget)?;
        make_room(&mut m.names, entry.name.len(), &mut m.budget)?;
        let name_off = u32::try_from(m.names.len()).map_err(|_| OVER)?;
        let name_len = u16::try_from(entry.name.len()).unwrap_or(u16::MAX);
        let (subvolume, target) = match entry.target {
            Target::Inode(i) => (false, i),
            Target::Subvolume(t) => (true, t),
        };
        m.names
            .extend_from_slice(entry.name.get(..usize::from(name_len)).unwrap_or(&[]));
        m.entries.push(EntryRec {
            name_off,
            name_len,
            ty: entry.ty,
            subvolume,
            stub: false,
            confirmed: false,
            target,
        });
        if let Some(r) = m.inodes.get_mut(usize::try_from(at).unwrap_or(usize::MAX)) {
            r.entries_len = r.entries_len.saturating_add(1);
        }
    }
    Ok(())
}

fn extent_data(
    m: &mut Model,
    tree: u64,
    at: u32,
    data: &[u8],
    out: &mut TreeLoad,
    diag: &mut Diagnostics,
) -> Result<(), BtrfsError> {
    let ino = m
        .inodes
        .get(usize::try_from(at).unwrap_or(usize::MAX))
        .map_or(0, |r| r.ino);
    let Some(extent) = items::parse_extent(data) else {
        diag.note(ProblemKind::ItemMalformed, tree, ino);
        damage(m, at);
        // The item still refers to something, and what that is may be shared
        // with files that parse. Not knowing is not the same as exclusive.
        match items::damaged_extent_target(data) {
            Some(0) => {}
            Some(bytenr) if m.damaged_refs.len() < MAX_DAMAGED_REFS => {
                charge(&mut m.budget, MAP_ENTRY_BYTES)?;
                m.damaged_refs.insert(bytenr);
                if let Some(&index) = m.ext_index.get(&bytenr)
                    && let Some(info) = m.exts.get_mut(usize::try_from(index).unwrap_or(usize::MAX))
                {
                    info.foreign = true;
                }
            }
            _ => {
                m.sharing_unknown = true;
                mark_all_foreign(m);
            }
        }
        return Ok(());
    };
    let (disk_bytenr, disk_num_bytes, compressed) = match extent {
        Extent::Inline { bytes } => {
            if let Some(r) = m.inodes.get_mut(usize::try_from(at).unwrap_or(usize::MAX)) {
                r.inline_bytes = r.inline_bytes.saturating_add(bytes);
            }
            return Ok(());
        }
        Extent::Disk {
            disk_bytenr,
            disk_num_bytes,
            compressed,
            ..
        } => (disk_bytenr, disk_num_bytes, compressed),
    };
    if compressed && let Some(r) = m.inodes.get_mut(usize::try_from(at).unwrap_or(usize::MAX)) {
        r.flags |= F_COMPRESSED;
    }
    if disk_bytenr == 0 {
        // A hole.
        return Ok(());
    }
    let index = match m.ext_index.get(&disk_bytenr).copied() {
        Some(index) => {
            let Some(info) = m.exts.get_mut(usize::try_from(index).unwrap_or(usize::MAX)) else {
                return Ok(());
            };
            if info.bytes != disk_num_bytes {
                // Two items disagree about one extent, so which is right is
                // not known; the extent is at least referred to twice.
                info.foreign = true;
                diag.note(ProblemKind::ExtentSizeMismatch, tree, ino);
                damage(m, at);
                return Ok(());
            }
            if info.first != at {
                info.foreign = true;
            }
            index
        }
        None => {
            make_room(&mut m.exts, 1, &mut m.budget)?;
            charge(&mut m.budget, MAP_ENTRY_BYTES)?;
            let index = u32::try_from(m.exts.len()).map_err(|_| OVER)?;
            let foreign = m.sharing_unknown || m.damaged_refs.contains(&disk_bytenr);
            m.exts.push(ExtInfo {
                first: at,
                bytes: disk_num_bytes,
                foreign,
            });
            m.ext_index.insert(disk_bytenr, index);
            out.new_extents.push(disk_bytenr);
            index
        }
    };
    make_room(&mut m.extent_refs, 1, &mut m.budget)?;
    // The first extent of an inode fixes where its list starts: they are
    // contiguous because an inode's items are.
    if let Some(r) = m.inodes.get_mut(usize::try_from(at).unwrap_or(usize::MAX)) {
        if r.extents_len == 0 {
            r.extents_start = u32::try_from(m.extent_refs.len()).map_err(|_| OVER)?;
        }
        r.extents_len = r.extents_len.saturating_add(1);
    }
    m.extent_refs.push(index);
    Ok(())
}

/// Looks at the subvolumes outside the scan and marks every extent of the
/// scan that any of them refers to as shared. `Ok(false)` when cancelled.
///
/// A subvolume that cannot be read makes the sharing of everything unknown,
/// and everything is then taken as shared: the reader never claims bytes are
/// freed by a deletion that it could not check.
fn census(
    src: &dyn TreeSource,
    m: &mut Model,
    cancelled: &(dyn Fn() -> bool + Sync),
    diag: &mut Diagnostics,
) -> Result<bool, BtrfsError> {
    if m.exts.is_empty() {
        return Ok(true);
    }
    let mut ids: Vec<u64> = Vec::new();
    let from = Key {
        objectid: FS_TREE_ID,
        ty: ROOT_ITEM,
        offset: 0,
    };
    let to = Key {
        objectid: LAST_FREE,
        ty: ROOT_ITEM,
        offset: u64::MAX,
    };
    let listed = src.search(ROOT_TREE_ID, from, to, &mut |item: Item<'_>| {
        if item.key.ty == ROOT_ITEM
            && items::is_subvolume_id(item.key.objectid)
            && ids.last() != Some(&item.key.objectid)
            && ids.len() < MAX_CENSUS_TREES
        {
            ids.push(item.key.objectid);
        }
        Flow::Continue
    });
    if listed.is_err() {
        m.sharing_unknown = true;
        mark_all_foreign(m);
        return Ok(true);
    }
    for id in ids {
        if m.tree_index.contains_key(&id) {
            continue;
        }
        if cancelled() {
            return Ok(false);
        }
        let from = Key {
            objectid: FIRST_FREE,
            ty: 0,
            offset: 0,
        };
        let to = Key {
            objectid: LAST_FREE,
            ty: u8::MAX,
            offset: u64::MAX,
        };
        // Marked as they are met, so that nothing grows with the size of the
        // other subvolume.
        let Model {
            ext_index, exts, ..
        } = &mut *m;
        // An item that does not parse refers to something that is not known.
        let mut unknown = false;
        let result = src.search(id, from, to, &mut |item: Item<'_>| {
            if item.key.ty != EXTENT_DATA {
                return Flow::Continue;
            }
            let target = match items::parse_extent(item.data) {
                Some(Extent::Disk { disk_bytenr, .. }) => disk_bytenr,
                Some(Extent::Inline { .. }) => 0,
                None => {
                    diag.note(ProblemKind::ItemMalformed, id, item.key.objectid);
                    match items::damaged_extent_target(item.data) {
                        Some(bytenr) => bytenr,
                        None => {
                            unknown = true;
                            0
                        }
                    }
                }
            };
            if target != 0
                && let Some(&index) = ext_index.get(&target)
                && let Some(info) = exts.get_mut(usize::try_from(index).unwrap_or(usize::MAX))
            {
                info.foreign = true;
            }
            Flow::Continue
        });
        if unknown || result.is_err() {
            m.sharing_unknown = true;
            mark_all_foreign(m);
            return Ok(true);
        }
        m.census_trees = m.census_trees.saturating_add(1);
    }
    Ok(true)
}

fn mark_all_foreign(m: &mut Model) {
    for e in &mut m.exts {
        e.foreign = true;
    }
}
