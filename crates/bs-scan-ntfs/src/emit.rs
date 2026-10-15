//! Turning the parsed table into scanner batches. NTFS keeps no list of a
//! directory's children in the table that this reader trusts; each file
//! names its parent, so the directories are rebuilt from those names, the
//! walk starts at the root (record 5) with an explicit stack, and a file the
//! walk never reaches is counted instead of dropped.

use crate::Accounting;
use crate::budget::TREE_BYTES_PER_ENTRY;
use crate::error::{Diagnostics, NtfsError, ProblemKind};
use crate::le::to_usize;
use crate::mft::Model;
use crate::parse::{F_HAS_EA, F_RESIDENT_DATA, FileRec, NONE, NS_DOS, drop_dos_aliases};
use crate::record::split_ref;
use crate::{Extras, Unaccounted};
use bs_scan::winrules::{self, EntryClass, attr};
use bs_scan::{Classifier, MAX_PATH_BYTES, ScanContext, ScanEnd, join_path};
use bs_tree::{
    Batch, Category, DirKey, FileKey, Meta, NodeFlags, NodeKind, ReadErrorKind, ReadFailure,
};
use std::rc::Rc;

/// The root directory's record number.
pub(crate) const ROOT_RECORD: u32 = 5;

/// Directories nested deeper than this are listed as unreadable. A path that
/// long cannot be named, so nothing legitimate is lost.
pub(crate) const MAX_DEPTH: u32 = 65_536;

/// A file this much smaller on disk than its length is sparse or compressed;
/// smaller gaps are rounding.
const SPARSE_MARGIN: u64 = 4096;

/// `ERROR_FILE_CORRUPT`-like code for damaged structures; there is no OS call
/// behind it, so the number only tells the kind apart.
const CODE_CORRUPT: i32 = 1392;
/// `ERROR_IO_DEVICE`.
const CODE_IO: i32 = 1117;

const FLUSH_RECORDS: usize = 4096;
const FLUSH_NAME_BYTES: usize = 256 * 1024;

#[derive(Clone, Copy)]
struct Edge {
    file: u32,
    name: u32,
}

/// Directories and their entries, from the names.
struct Graph {
    /// `start[d]..start[d + 1]` are the entries of directory `d`.
    start: Vec<u32>,
    edges: Vec<Edge>,
    /// How many live names each file has.
    names: Vec<u16>,
}

/// The names of a file after the DOS aliases of names in the same directory
/// are taken out. A file never has more than the reader keeps (the parser
/// and the join of extension records cap them), and the filter is
/// O(n log n) in any case.
fn live_names(model: &Model, file: &FileRec, out: &mut Vec<u32>) {
    out.clear();
    let mut at = file.name_head;
    while at != NONE && out.len() <= model.names.len() {
        let Some(n) = model
            .names
            .get(to_usize(u64::from(at)).unwrap_or(usize::MAX))
        else {
            break;
        };
        out.push(at);
        at = n.next;
    }
    // A DOS name is the short alias of a long name in the same directory;
    // without one it is the file's only name there.
    drop_dos_aliases(out, |&i| {
        model
            .names
            .get(to_usize(u64::from(i)).unwrap_or(usize::MAX))
            .map_or((u64::MAX, false), |n| {
                (split_ref(n.parent).0, n.ns == NS_DOS)
            })
    });
}

fn build_graph(model: &Model, diag: &mut Diagnostics) -> Result<Graph, NtfsError> {
    let n = model.files.len();
    let oom = |_| NtfsError::TooLarge("cannot allocate the directory index");
    let mut start: Vec<u32> = Vec::new();
    start.try_reserve_exact(n.saturating_add(1)).map_err(oom)?;
    start.resize(n.saturating_add(1), 0);
    let mut names_of: Vec<u16> = Vec::new();
    names_of.try_reserve_exact(n).map_err(oom)?;
    names_of.resize(n, 0);

    let mut scratch = Vec::new();
    let mut total = 0u64;
    // Pass 1: count entries per directory.
    let valid_parent = |model: &Model, child: u32, parent_ref: u64| -> Option<usize> {
        let (p, seq) = split_ref(parent_ref);
        let pi = to_usize(p)?;
        let parent = model.files.get(pi)?;
        let ok = parent.present()
            && parent.is_dir()
            && (seq == 0 || seq == parent.seq)
            && p != u64::from(child);
        ok.then_some(pi)
    };
    for (i, f) in model.files.iter().enumerate() {
        if !f.present() {
            continue;
        }
        let Ok(child) = u32::try_from(i) else { break };
        if child == ROOT_RECORD {
            continue;
        }
        live_names(model, f, &mut scratch);
        if let Some(c) = names_of.get_mut(i) {
            *c = u16::try_from(scratch.len()).unwrap_or(u16::MAX);
        }
        for &nm in &scratch {
            let Some(name) = model
                .names
                .get(to_usize(u64::from(nm)).unwrap_or(usize::MAX))
            else {
                continue;
            };
            match valid_parent(model, child, name.parent) {
                Some(p) => {
                    if let Some(s) = start.get_mut(p.saturating_add(1)) {
                        *s = s.saturating_add(1);
                    }
                    total = total.saturating_add(1);
                }
                None => diag.note(ProblemKind::ParentStale, u64::from(child), name.parent),
            }
        }
    }
    if total > u64::from(u32::MAX) {
        return Err(NtfsError::TooLarge(
            "more directory entries than can be numbered",
        ));
    }
    // Prefix sums.
    let mut running = 0u32;
    for s in &mut start {
        running = running.saturating_add(*s);
        *s = running;
    }
    let mut edges: Vec<Edge> = Vec::new();
    edges
        .try_reserve_exact(to_usize(total).unwrap_or(0))
        .map_err(oom)?;
    edges.resize(
        to_usize(total).unwrap_or(0),
        Edge {
            file: NONE,
            name: NONE,
        },
    );
    // Pass 2: fill, with a cursor per directory.
    let mut cursor: Vec<u32> = Vec::new();
    cursor.extend(start.iter().take(n).copied());
    for (i, f) in model.files.iter().enumerate() {
        if !f.present() {
            continue;
        }
        let Ok(child) = u32::try_from(i) else { break };
        if child == ROOT_RECORD {
            continue;
        }
        live_names(model, f, &mut scratch);
        for &nm in &scratch {
            let Some(name) = model
                .names
                .get(to_usize(u64::from(nm)).unwrap_or(usize::MAX))
            else {
                continue;
            };
            let Some(p) = valid_parent(model, child, name.parent) else {
                continue;
            };
            let Some(slot) = cursor.get_mut(p) else {
                continue;
            };
            let at = to_usize(u64::from(*slot)).unwrap_or(usize::MAX);
            *slot = slot.saturating_add(1);
            if let Some(e) = edges.get_mut(at) {
                *e = Edge {
                    file: child,
                    name: nm,
                };
            }
        }
    }
    Ok(Graph {
        start,
        edges,
        names: names_of,
    })
}

/// The attributes the classification looks at. The bit 0x40000 is "recall on
/// open" to the system's interface, but on disk it also marks a file that has
/// extended attributes, which the interface does not report as a recall.
/// Without a recall reparse point, a file with extended attributes is just
/// that.
fn effective_attrs(f: &FileRec) -> u32 {
    let mut attrs = f.attrs | if f.is_dir() { attr::DIRECTORY } else { 0 };
    if f.c.flags & F_HAS_EA != 0 {
        attrs &= !attr::RECALL_ON_OPEN;
    }
    attrs
}

/// What a file-like entry is and the flags it carries: a link or mount point
/// that was not followed, a cloud placeholder whose size comes from its
/// record alone, a stream that is sparse or compressed. One place for every
/// way an entry reaches the tree, so a file placed outside any folder is
/// described like any other.
fn kind_and_flags(f: &FileRec, attrs: u32, class: &winrules::Classified) -> (NodeKind, NodeFlags) {
    let kind = match class.class {
        EntryClass::Symlink => NodeKind::Symlink,
        EntryClass::Other => NodeKind::Other,
        _ => NodeKind::File,
    };
    let mut flags = NodeFlags::EMPTY;
    if class.placeholder {
        flags |= NodeFlags::CLOUD_PLACEHOLDER;
    }
    if class.not_followed {
        flags |= NodeFlags::NOT_FOLLOWED;
    }
    if kind == NodeKind::File
        && !class.placeholder
        && (attrs & (attr::SPARSE_FILE | attr::COMPRESSED) != 0
            || f.c.alloc.saturating_add(SPARSE_MARGIN) <= f.c.logical)
    {
        flags |= NodeFlags::SPARSE_OR_COMPRESSED;
    }
    (kind, flags)
}

enum PathSpec {
    Full(Rc<[u8]>),
}

struct Frame {
    file: u32,
    key: DirKey,
    path: PathSpec,
    depth: u32,
    /// Bytes of path this frame allocated (0 when it shares its parent's).
    own_path: u64,
}

/// The name of the folder that holds files no directory reaches. The name
/// tells a person what it is, nothing more: a real directory may carry the
/// same name (POSIX names on NTFS may contain `<`), so nothing may identify
/// the folder by it. The folder carries [`NodeFlags::SYNTHETIC`], which is
/// what the interface and the delete flows test.
pub(crate) const ORPHANS_NAME: &[u8] = b"<files outside any folder>";

#[derive(Default)]
struct Tally {
    allocated: u64,
    logical: u64,
    files: u64,
    dirs: u64,
    unreadable: u64,
}

pub(crate) struct Emitter<'a> {
    model: &'a Model,
    ctx: &'a ScanContext,
    classifier: Option<Classifier>,
    graph: Graph,
    reached: Vec<u64>,
    /// Records below a directory the walk did not enter (a link or mount
    /// point): left out by design, never adopted.
    shadowed: Vec<u64>,
    /// What the category lookups' paths may still take.
    path_room: u64,
    path_live: u64,
    /// The root's listing is closed last, after the files nobody reaches are
    /// placed, so it carries what they say.
    root_broken: bool,
    root_io: bool,
    batch: Batch,
    tally: Tally,
    sep: u8,
    accounting: Accounting,
    volume_key: u64,
    pub diag: Diagnostics,
    pub extras: Extras,
    pub unreadable_entries: u64,
    pub not_entered: u64,
    pub cancelled: bool,
    /// Files placed in the folder of files outside any directory.
    pub adopted: Unaccounted,
}

fn bump(counter: &mut u64) {
    *counter = counter.saturating_add(1);
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

impl<'a> Emitter<'a> {
    pub fn new(
        model: &'a Model,
        ctx: &'a ScanContext,
        accounting: Accounting,
        sep: u8,
        limit: u64,
        diag: &mut Diagnostics,
    ) -> Result<Emitter<'a>, NtfsError> {
        // The emission holds, beside the model: the directory index, two
        // bit sets, the walk's stack, and the tree the batches build. All of
        // it is bounded by the model, so it is checked before any of it is
        // made.
        let n = model.files.len() as u64;
        let names = model.names.len() as u64;
        let present = model.files.iter().filter(|f| f.present()).count() as u64;
        let dirs = model
            .files
            .iter()
            .filter(|f| f.present() && f.is_dir())
            .count() as u64;
        // `start`, the per-file name counts, and the fill cursor; the edges.
        let graph_bytes = n
            .saturating_mul(4 + 2 + 4)
            .saturating_add(names.saturating_mul(std::mem::size_of::<Edge>() as u64));
        let sets_bytes = (n / 8).saturating_mul(2).saturating_add(16);
        let stack_bytes =
            dirs.saturating_mul((std::mem::size_of::<Frame>() as u64).saturating_add(8));
        // Every name becomes an entry, and so does every file that is
        // adopted.
        let tree_bytes = names
            .saturating_add(present)
            .saturating_mul(TREE_BYTES_PER_ENTRY)
            .saturating_add(model.pool.len() as u64);
        let needed = model
            .bytes()
            .saturating_add(graph_bytes)
            .saturating_add(sets_bytes)
            .saturating_add(stack_bytes)
            .saturating_add(tree_bytes);
        if needed > limit {
            return Err(NtfsError::TooLarge(
                "the directories and the tree need more memory than the limit allows",
            ));
        }
        let graph = build_graph(model, diag)?;
        let words = model.files.len().div_ceil(64);
        let bitset = || -> Result<Vec<u64>, NtfsError> {
            let mut v = Vec::new();
            v.try_reserve_exact(words)
                .map_err(|_| NtfsError::TooLarge("cannot allocate the visited set"))?;
            v.resize(words, 0u64);
            Ok(v)
        };
        let reached = bitset()?;
        let shadowed = bitset()?;
        let classifier = ctx.classifier();
        Ok(Emitter {
            model,
            ctx,
            classifier,
            graph,
            reached,
            shadowed,
            path_room: limit.saturating_sub(needed),
            path_live: 0,
            root_broken: false,
            root_io: false,
            batch: Batch::new(),
            tally: Tally::default(),
            sep,
            accounting,
            volume_key: model.geo.serial & 0xFFFF_FFFF,
            diag: Diagnostics::default(),
            extras: Extras::default(),
            unreadable_entries: 0,
            not_entered: 0,
            cancelled: false,
            adopted: Unaccounted::default(),
        })
    }

    /// Marks a record as reached; says whether it already was.
    fn test_and_set(&mut self, slot: u32) -> bool {
        let slot = to_usize(u64::from(slot)).unwrap_or(usize::MAX);
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

    fn is_reached(&self, slot: usize) -> bool {
        self.reached
            .get(slot / 64)
            .is_some_and(|w| (*w >> (slot % 64)) & 1 == 1)
    }

    /// Sends the batch and the counts. `false` when the receiver is gone.
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

    /// The sizes this entry is reported with.
    fn sizes(&self, f: &FileRec, is_dir: bool) -> (u64, u64) {
        match self.accounting {
            Accounting::Full => (
                f.c.logical.saturating_add(f.c.ads_logical),
                f.c.alloc
                    .saturating_add(f.c.ads_alloc)
                    .saturating_add(f.c.other_alloc),
            ),
            Accounting::Listing if is_dir => (0, 0),
            // A listing gives data that lives in the file record the length
            // rounded up to eight bytes as its allocation; the clusters of
            // everything else.
            Accounting::Listing if f.c.flags & F_RESIDENT_DATA != 0 => {
                (f.c.logical, f.c.logical.saturating_add(7) & !7)
            }
            Accounting::Listing => (f.c.logical, f.c.alloc),
        }
    }

    /// What the extras of Full over Listing add up to, for a reached file.
    fn note_extras(&mut self, f: &FileRec) {
        let e = &mut self.extras;
        e.streams = e.streams.saturating_add(u64::from(f.c.ads_streams));
        e.stream_logical = e.stream_logical.saturating_add(f.c.ads_logical);
        e.stream_allocated = e.stream_allocated.saturating_add(f.c.ads_alloc);
        e.attribute_allocated = e.attribute_allocated.saturating_add(f.c.other_alloc);
        if f.is_dir() {
            e.directory_allocated = e
                .directory_allocated
                .saturating_add(f.c.other_alloc)
                .saturating_add(f.c.alloc);
        }
    }

    /// Walks the tree and sends it. The root record goes first.
    pub fn run(&mut self, root_name: &[u8]) -> Result<ScanEnd, NtfsError> {
        let model = self.model;
        let root = model
            .file(ROOT_RECORD)
            .filter(|f| f.present() && f.is_dir())
            .ok_or(NtfsError::BadRoot("record 5 is not a directory in use"))?;
        let key = self
            .ctx
            .next_dir_key()
            .ok_or(NtfsError::BadRoot("no directory keys"))?;
        self.test_and_set(ROOT_RECORD);
        let (logical, allocated) = self.sizes(root, true);
        let meta = Meta {
            logical,
            allocated,
            mtime: root.mtime,
            ..Meta::default()
        };
        self.batch
            .dir(key, DirKey::NONE, root_name, &meta)
            .map_err(|_| NtfsError::TooLarge("root name"))?;
        self.tally.allocated = allocated;
        self.tally.logical = logical;
        self.tally.dirs = 1;
        self.note_extras(root);
        // The root's record goes out alone, before anything refers to it.
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
            file: ROOT_RECORD,
            key,
            path: PathSpec::Full(root_path),
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
            // First entry on top, so the walk follows the table's order.
            stack.extend(children.into_iter().rev());
            if self.cancelled || !self.flush_if_due() {
                self.cancelled = true;
                return Ok(ScanEnd::Cancelled);
            }
        }
        self.finish_root(key);
        if self.cancelled || !self.flush_if_due() {
            self.cancelled = true;
            return Ok(ScanEnd::Cancelled);
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
        let PathSpec::Full(full) = &frame.path;
        let place = self.classifier.as_ref().and_then(|c| c.place(full));
        let me = to_usize(u64::from(frame.file)).unwrap_or(usize::MAX);
        let from = self.graph.start.get(me).copied().unwrap_or(0);
        let to = self
            .graph
            .start
            .get(me.saturating_add(1))
            .copied()
            .unwrap_or(0);
        for at in from..to {
            let Some(edge) = self
                .graph
                .edges
                .get(to_usize(u64::from(at)).unwrap_or(usize::MAX))
                .copied()
            else {
                continue;
            };
            self.entry(frame, full, place, edge, &mut children);
            if !self.flush_if_due() {
                self.cancelled = true;
                break;
            }
        }
        let own = model.file(frame.file);
        let mut broken = own.is_some_and(FileRec::damaged);
        let mut io = false;
        if frame.file == ROOT_RECORD && model.lost > 0 {
            // Records that could not be read may have been anywhere, so the
            // root cannot claim to be complete.
            broken = true;
            io = self.diag_io(model);
        }
        if frame.file == ROOT_RECORD {
            // Closed last, by `finish_root`.
            self.root_broken = broken;
            self.root_io = io;
            return children;
        }
        self.close(frame.key, broken, io);
        children
    }

    /// Reports a directory as listed, or as unreadable.
    fn close(&mut self, key: DirKey, broken: bool, io: bool) {
        if broken {
            self.batch.unreadable(key, failure(io));
            bump(&mut self.tally.unreadable);
            bump(&mut self.unreadable_entries);
        } else {
            self.batch.listing_done(key);
        }
    }

    /// Places the files no directory reaches, then closes the root. The
    /// root is unreadable when the table had records that could not be read,
    /// because they may have been anywhere, and when files had to be placed
    /// outside a folder of their own that could not be made.
    fn finish_root(&mut self, root: DirKey) {
        let placed = self.adopt_orphans(root);
        let (broken, io) = (self.root_broken || !placed, self.root_io);
        self.close(root, broken, io);
    }

    fn diag_io(&self, model: &Model) -> bool {
        model.diag.count(ProblemKind::MftUnreadable) > 0
            || model.diag.count(ProblemKind::MftHole) > 0
    }

    fn entry(
        &mut self,
        frame: &Frame,
        full: &Rc<[u8]>,
        place: Option<Category>,
        edge: Edge,
        children: &mut Vec<Frame>,
    ) {
        let model = self.model;
        let (Some(f), Some(nr)) = (
            model.file(edge.file),
            model
                .names
                .get(to_usize(u64::from(edge.name)).unwrap_or(usize::MAX)),
        ) else {
            return;
        };
        let f = *f;
        let name = model.name_bytes(nr);

        if name.iter().any(|&b| b == 0 || b == b'\\' || b == b'/') {
            // No directory holds such a name; opening it by name could reach
            // somewhere else, so it is kept as damaged and never followed.
            self.diag
                .note(ProblemKind::NameUnusable, u64::from(edge.file), 0);
            self.test_and_set(edge.file);
            let meta = Meta {
                failure: Some(failure(false)),
                ..Meta::default()
            };
            bump(&mut self.tally.files);
            bump(&mut self.tally.unreadable);
            bump(&mut self.unreadable_entries);
            self.batch
                .file(NodeKind::Other, frame.key, name, &meta)
                .ok();
            return;
        }

        let attrs = effective_attrs(&f);
        let class = winrules::classify(attrs, f.reparse_tag);
        // A directory is entered unless it is a link or mount point. A cloud
        // placeholder folder is entered too: the walker must not list one,
        // because listing can fetch its contents, but this reader only reads
        // the table and cannot.
        let enter = f.is_dir() && class.class == EntryClass::Dir;
        if enter {
            self.dir_entry(frame, full, edge, name, &f, class.placeholder, children);
        } else {
            self.file_entry(frame, place, edge, name, &f, attrs, &class);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn file_entry(
        &mut self,
        frame: &Frame,
        place: Option<Category>,
        edge: Edge,
        name: &[u8],
        f: &FileRec,
        attrs: u32,
        class: &winrules::Classified,
    ) {
        let first_sight = !self.test_and_set(edge.file);
        let (logical, allocated) = self.sizes(f, f.is_dir());
        let (kind, mut flags) = kind_and_flags(f, attrs, class);
        let mut link = None;
        let names = self
            .graph
            .names
            .get(to_usize(u64::from(edge.file)).unwrap_or(usize::MAX))
            .copied()
            .unwrap_or(0);
        if names > 1 && !f.is_dir() {
            flags |= NodeFlags::HARD_LINK;
            link = Some(FileKey {
                volume: self.volume_key,
                id: u128::from(u64::from(edge.file) | (u64::from(f.seq) << 48)),
            });
        }
        let mut meta = Meta {
            logical,
            allocated,
            mtime: f.mtime,
            flags,
            link,
            ..Meta::default()
        };
        if f.damaged() {
            meta.failure = Some(failure(false));
            bump(&mut self.tally.unreadable);
            bump(&mut self.unreadable_entries);
        }
        if let Some(classifier) = &self.classifier {
            meta.category = classifier.category(place, kind, name);
        }
        if f.is_dir() {
            // A link or mount point that is a directory: not entered. What
            // the table holds below it is not in the tree.
            let me = to_usize(u64::from(edge.file)).unwrap_or(usize::MAX);
            let below = self
                .graph
                .start
                .get(me.saturating_add(1))
                .copied()
                .unwrap_or(0)
                .saturating_sub(self.graph.start.get(me).copied().unwrap_or(0));
            self.not_entered = self.not_entered.saturating_add(u64::from(below));
            self.shadow_below(edge.file);
        }
        // Every name is a file in the walker's counts; the tree sorts out
        // which of them carries the bytes.
        bump(&mut self.tally.files);
        self.tally.allocated = self.tally.allocated.saturating_add(allocated);
        self.tally.logical = self.tally.logical.saturating_add(logical);
        if first_sight {
            self.note_extras(f);
        }
        // Names are at most 255 UTF-16 units, so a batch cannot be too full
        // for one.
        self.batch.file(kind, frame.key, name, &meta).ok();
    }

    #[allow(clippy::too_many_arguments)]
    fn dir_entry(
        &mut self,
        frame: &Frame,
        full: &Rc<[u8]>,
        edge: Edge,
        name: &[u8],
        f: &FileRec,
        placeholder: bool,
        children: &mut Vec<Frame>,
    ) {
        if self.test_and_set(edge.file) {
            // A second way into a directory: a loop, or a hard-linked
            // directory. The first way stands.
            self.diag.note(
                ProblemKind::DirectoryLinkedTwice,
                u64::from(frame.file),
                u64::from(edge.file),
            );
            return;
        }
        let depth = frame.depth.saturating_add(1);
        let Some(key) = self.ctx.next_dir_key() else {
            // Out of keys: it cannot be described as a directory.
            let meta = Meta {
                failure: Some(ReadFailure {
                    kind: ReadErrorKind::Other,
                    code: 0,
                }),
                mtime: f.mtime,
                ..Meta::default()
            };
            bump(&mut self.tally.unreadable);
            bump(&mut self.unreadable_entries);
            bump(&mut self.tally.files);
            self.batch
                .file(NodeKind::Other, frame.key, name, &meta)
                .ok();
            return;
        };
        let (logical, allocated) = self.sizes(f, true);
        let mut meta = Meta {
            logical,
            allocated,
            mtime: f.mtime,
            flags: if placeholder {
                NodeFlags::CLOUD_PLACEHOLDER
            } else {
                NodeFlags::EMPTY
            },
            ..Meta::default()
        };
        if depth > MAX_DEPTH {
            self.diag.note(
                ProblemKind::TooDeep,
                u64::from(frame.file),
                u64::from(edge.file),
            );
            meta.failure = Some(ReadFailure {
                kind: ReadErrorKind::TooDeep,
                code: 0,
            });
            bump(&mut self.tally.unreadable);
            bump(&mut self.unreadable_entries);
        }
        bump(&mut self.tally.dirs);
        self.tally.allocated = self.tally.allocated.saturating_add(allocated);
        self.tally.logical = self.tally.logical.saturating_add(logical);
        self.note_extras(f);
        self.batch.dir(key, frame.key, name, &meta).ok();
        if depth <= MAX_DEPTH {
            let mut own_path = 0u64;
            let path: Rc<[u8]> = if self.classifier.is_some() {
                // The path of every directory waiting its turn is held in
                // memory; past what the limit leaves, a directory shares its
                // parent's path and only its category lookups lose precision.
                let want = full
                    .len()
                    .saturating_add(1)
                    .saturating_add(name.len())
                    .min(MAX_PATH_BYTES.saturating_add(usize::from(u8::MAX) + 1))
                    as u64;
                if self.path_live.saturating_add(want) <= self.path_room {
                    self.path_live = self.path_live.saturating_add(want);
                    own_path = want;
                    Rc::from(join_path(full, name, self.sep))
                } else {
                    self.diag.note(
                        ProblemKind::PathBudget,
                        u64::from(frame.file),
                        u64::from(edge.file),
                    );
                    Rc::clone(full)
                }
            } else {
                Rc::clone(full)
            };
            children.push(Frame {
                file: edge.file,
                key,
                path: PathSpec::Full(path),
                depth,
                own_path,
            });
        }
    }

    fn set_bit(set: &mut [u64], slot: usize) -> bool {
        match set.get_mut(slot / 64) {
            Some(w) => {
                let bit = 1u64 << (slot % 64);
                let was = *w & bit != 0;
                *w |= bit;
                was
            }
            None => true,
        }
    }

    fn has_bit(set: &[u64], slot: usize) -> bool {
        set.get(slot / 64)
            .is_some_and(|w| (*w >> (slot % 64)) & 1 == 1)
    }

    /// Marks everything that lies below the directory `dir` (the entries
    /// whose names point into it, and so on down) as left out on purpose:
    /// `dir` is a link or mount point the walk does not enter. An explicit
    /// stack, and a mark that stops the descent, so a loop ends.
    fn shadow_below(&mut self, dir: u32) {
        let mut stack = vec![dir];
        while let Some(d) = stack.pop() {
            let me = to_usize(u64::from(d)).unwrap_or(usize::MAX);
            let from = self.graph.start.get(me).copied().unwrap_or(0);
            let to = self
                .graph
                .start
                .get(me.saturating_add(1))
                .copied()
                .unwrap_or(0);
            for at in from..to {
                let Some(edge) = self
                    .graph
                    .edges
                    .get(to_usize(u64::from(at)).unwrap_or(usize::MAX))
                    .copied()
                else {
                    continue;
                };
                let slot = to_usize(u64::from(edge.file)).unwrap_or(usize::MAX);
                if !Self::set_bit(&mut self.shadowed, slot)
                    && self.model.file(edge.file).is_some_and(FileRec::is_dir)
                {
                    stack.push(edge.file);
                }
            }
        }
    }

    /// The bytes a file holds in clusters, whatever the accounting: what the
    /// gap and the unaccounted figures are made of.
    fn clusters_of(f: &FileRec) -> u64 {
        f.c.alloc
            .saturating_add(f.c.ads_alloc)
            .saturating_add(f.c.other_alloc)
    }

    /// Puts every file that is in use, was not reached, is not below a
    /// directory left out on purpose, and has a name or holds clusters, into
    /// a folder of the root, which is marked unreadable: the files are
    /// counted and visible, but where they belong is not known, and a
    /// directory of theirs may be among the records the table lost. Records
    /// with no name and no bytes (the reserved records every volume has) are
    /// not files and stay unplaced.
    ///
    /// `false` when something had to stay unplaced because the tree has no
    /// key left for the folder.
    fn adopt_orphans(&mut self, root: DirKey) -> bool {
        let model = self.model;
        let mut folder: Option<DirKey> = None;
        let mut scratch: Vec<u32> = Vec::new();
        for (i, f) in model.files.iter().enumerate() {
            if !f.present() || self.cancelled {
                continue;
            }
            if self.is_reached(i) || Self::has_bit(&self.shadowed, i) {
                continue;
            }
            let Ok(rec) = u32::try_from(i) else { break };
            live_names(model, f, &mut scratch);
            let first_name = scratch
                .first()
                .and_then(|&nm| {
                    model
                        .names
                        .get(to_usize(u64::from(nm)).unwrap_or(usize::MAX))
                })
                .map(|nr| model.name_bytes(nr));
            if first_name.is_none() && Self::clusters_of(f) == 0 {
                continue;
            }
            let key = match folder {
                Some(k) => k,
                None => {
                    let Some(k) = self.ctx.next_dir_key() else {
                        return false;
                    };
                    let meta = Meta {
                        flags: NodeFlags::SYNTHETIC,
                        ..Meta::default()
                    };
                    self.batch.dir(k, root, ORPHANS_NAME, &meta).ok();
                    bump(&mut self.tally.dirs);
                    folder = Some(k);
                    k
                }
            };
            let synthesized;
            let name: &[u8] = match first_name {
                Some(n) if !n.iter().any(|&b| b == 0 || b == b'\\' || b == b'/') => n,
                other => {
                    if other.is_some() {
                        self.diag.note(ProblemKind::NameUnusable, u64::from(rec), 1);
                    }
                    synthesized = format!("<record {rec}>").into_bytes();
                    &synthesized
                }
            };
            Self::set_bit(&mut self.reached, i);
            let (logical, allocated) = self.sizes(f, f.is_dir());
            // Described like any other entry: a placeholder, a link that is
            // not followed, a sparse stream, and the category by extension
            // (the place is not known, so the place gives none). A directory
            // is not entered: what lies in it is placed here as well, flat.
            let attrs = effective_attrs(f);
            let class = winrules::classify(attrs, f.reparse_tag);
            let (mut kind, mut flags) = kind_and_flags(f, attrs, &class);
            if f.is_dir() && class.class == EntryClass::Dir {
                kind = NodeKind::Other;
            }
            if scratch.len() > 1 && !f.is_dir() {
                flags |= NodeFlags::HARD_LINK;
            }
            let mut meta = Meta {
                logical,
                allocated,
                mtime: f.mtime,
                flags,
                ..Meta::default()
            };
            if let Some(classifier) = &self.classifier {
                meta.category = classifier.category(None, kind, name);
            }
            if f.damaged() {
                meta.failure = Some(failure(false));
                bump(&mut self.tally.unreadable);
                bump(&mut self.unreadable_entries);
            }
            bump(&mut self.tally.files);
            self.tally.allocated = self.tally.allocated.saturating_add(allocated);
            self.tally.logical = self.tally.logical.saturating_add(logical);
            self.note_extras(f);
            self.batch.file(kind, key, name, &meta).ok();
            self.adopted.files = self.adopted.files.saturating_add(1);
            self.adopted.allocated = self.adopted.allocated.saturating_add(Self::clusters_of(f));
            if !self.flush_if_due() {
                self.cancelled = true;
                return true;
            }
        }
        if let Some(k) = folder {
            self.close(k, true, false);
        }
        true
    }

    /// Counts what is in use but is not in the tree: records with no name and
    /// no bytes, and whatever lies below a directory the walk did not enter.
    pub fn account_for_the_rest(&self) -> (Unaccounted, u64) {
        let mut unreachable = Unaccounted::default();
        let mut in_use = 0u64;
        for (i, f) in self.model.files.iter().enumerate() {
            if !f.present() {
                continue;
            }
            in_use = in_use.saturating_add(1);
            if self.is_reached(i) {
                continue;
            }
            unreachable.files = unreachable.files.saturating_add(1);
            // Whatever the accounting, the bytes nobody reaches are counted in
            // full: they are the gap, not part of a comparison.
            unreachable.allocated = unreachable.allocated.saturating_add(Self::clusters_of(f));
        }
        (unreachable, in_use)
    }
}
