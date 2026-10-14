//! The owner side of the tree: applies batches, keeps counters and
//! completeness, handles delete marks and publishes snapshots.

use crate::batch::{Batch, Entry, Record};
use crate::error::{ApplyError, DeleteError};
use crate::hash::FastMap;
use crate::snapshot::Snapshot;
use crate::store::{Mark, NONE, NewNode, Store};
use crate::types::{
    ByteCount, DeleteState, DeleteTotals, FileKey, MAX_NAME_LEN, Moved, NodeFlags, NodeId,
    NodeKind, ReadFailure,
};
use std::collections::BTreeMap;
use std::sync::{Arc, PoisonError, RwLock};
use std::time::{Duration, Instant};

/// Caps that keep hostile or broken input from using unbounded memory.
#[derive(Clone, Copy, Debug)]
pub struct ApplierLimits {
    /// Most nodes the tree will hold. Defaults to the largest id space there
    /// is; lower it to test exhaustion.
    pub max_nodes: u32,
    /// Most records held back while waiting for their parent directory.
    pub max_parked_records: usize,
    /// Most name bytes held back with them.
    pub max_parked_name_bytes: usize,
}

impl Default for ApplierLimits {
    fn default() -> Self {
        ApplierLimits {
            max_nodes: u32::MAX,
            max_parked_records: 1 << 20,
            max_parked_name_bytes: 64 << 20,
        }
    }
}

/// What happened to the records of one [`Aggregator::apply`] call.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ApplyReport {
    /// Records applied, including earlier ones that were waiting for a
    /// parent that this batch delivered.
    pub applied: u32,
    /// Records held back because their parent directory has not arrived.
    pub parked: u32,
    /// Records refused as invalid.
    pub rejected: u32,
    /// The first few refusals: the record's position in the batch (`None`
    /// for a record that had been held back) and why.
    pub first_errors: Vec<(Option<u32>, ApplyError)>,
}

impl ApplyReport {
    const MAX_ERRORS: usize = 16;

    fn reject(&mut self, index: Option<u32>, error: ApplyError) {
        self.rejected = self.rejected.saturating_add(1);
        if self.first_errors.len() < Self::MAX_ERRORS {
            self.first_errors.push((index, error));
        }
    }
}

/// Result of [`Aggregator::finish`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FinishReport {
    /// Records that were still waiting for a parent that never arrived.
    /// They are counted as lost.
    pub unresolved: u64,
}

/// A directory still being counted.
struct OpenDir {
    /// Subdirectories that are not complete yet.
    pending: u32,
    /// Whether the scanner reported the listing finished.
    listing_done: bool,
}

/// A record waiting for its parent directory.
struct Parked {
    rec: Record,
    name: Box<[u8]>,
}

/// Size changes of one batch, collected and then applied to every affected
/// directory and its ancestors in a single pass at the end of the batch.
///
/// Entries of one directory arrive together, so they are summed in a run and
/// moved into `deltas` when the directory changes. Settling then walks the
/// touched directories from the highest node id down: a parent always has a
/// lower id than its children, so each directory is updated once, after
/// everything below it has been added into its delta.
struct Pending {
    node: u32,
    delta: ByteCount,
    deltas: BTreeMap<u32, ByteCount>,
}

fn add_counts(into: &mut ByteCount, delta: ByteCount) {
    into.logical = into.logical.saturating_add(delta.logical);
    into.allocated = into.allocated.saturating_add(delta.allocated);
    into.files = into.files.saturating_add(delta.files);
}

impl Pending {
    fn new() -> Pending {
        Pending {
            node: NONE,
            delta: ByteCount::default(),
            deltas: BTreeMap::new(),
        }
    }

    fn add(&mut self, parent: u32, delta: ByteCount) {
        if self.node != parent {
            self.close_run();
            self.node = parent;
        }
        add_counts(&mut self.delta, delta);
    }

    fn close_run(&mut self) {
        if self.node != NONE {
            add_counts(self.deltas.entry(self.node).or_default(), self.delta);
        }
        self.node = NONE;
        self.delta = ByteCount::default();
    }
}

enum Step {
    Done,
    Park,
    Reject(ApplyError),
}

/// Handle for reading the latest published snapshot from any thread.
///
/// Loading never waits for the aggregator's work. The only lock involved
/// guards a single pointer: the aggregator takes it for the instant it swaps
/// in a new snapshot (never while applying batches, building a snapshot or
/// freeing an old one), and readers take it for the instant it takes to clone
/// the pointer.
#[derive(Clone)]
pub struct SnapshotReader {
    cell: Arc<RwLock<Arc<Snapshot>>>,
}

impl SnapshotReader {
    /// The most recently published snapshot.
    pub fn load(&self) -> Arc<Snapshot> {
        // Nothing can panic while the lock is held, so poisoning cannot
        // happen; if it somehow did, the pointer inside is still valid.
        let guard = self.cell.read().unwrap_or_else(PoisonError::into_inner);
        Arc::clone(&guard)
    }
}

/// Owns the tree and applies batches to it. All mutation goes through here,
/// from a single thread; readers use [`SnapshotReader`].
pub struct Aggregator {
    store: Store,
    limits: ApplierLimits,
    /// Directory key to node id, for every directory seen.
    keys: FastMap<u32, u32>,
    /// Directories still being counted, by node id.
    open: FastMap<u32, OpenDir>,
    /// First sighting of each hard-linked file.
    links: FastMap<FileKey, u32>,
    /// Records waiting for the directory with this key.
    parked: FastMap<u32, Vec<Parked>>,
    /// For parked directory entries: their key to the key they wait for.
    /// Followed with path halving to find the key at the top of a chain of
    /// waiting directories, which makes the cycle check cheap.
    wait_links: FastMap<u32, u32>,
    parked_records: usize,
    parked_bytes: usize,
    /// Directory keys whose parked records can now be applied.
    ready: Vec<u32>,
    /// Sum of everything accepted, including bytes not yet added to
    /// ancestors, for overflow checks.
    accepted_logical: u64,
    accepted_allocated: u64,
    rejected_total: u64,
    /// A record was lost before the root existed, so the root, once it
    /// arrives, must be flagged.
    lost_before_root: bool,
    /// `finish` ran: scan-time bookkeeping is gone and nothing more is
    /// accepted.
    finished: bool,
    cell: Arc<RwLock<Arc<Snapshot>>>,
    epoch: u64,
    last_publish: Option<Instant>,
}

impl Default for Aggregator {
    fn default() -> Self {
        Aggregator::new()
    }
}

impl Aggregator {
    /// An empty tree with default limits.
    pub fn new() -> Aggregator {
        Aggregator::with_limits(ApplierLimits::default())
    }

    /// An empty tree with explicit limits.
    pub fn with_limits(limits: ApplierLimits) -> Aggregator {
        let empty = Arc::new(Snapshot {
            epoch: 0,
            store: Store::default(),
        });
        Aggregator {
            store: Store::default(),
            limits,
            keys: FastMap::default(),
            open: FastMap::default(),
            links: FastMap::default(),
            parked: FastMap::default(),
            wait_links: FastMap::default(),
            parked_records: 0,
            parked_bytes: 0,
            ready: Vec::new(),
            accepted_logical: 0,
            accepted_allocated: 0,
            rejected_total: 0,
            lost_before_root: false,
            finished: false,
            cell: Arc::new(RwLock::new(empty)),
            epoch: 0,
            last_publish: None,
        }
    }

    /// A handle readers can clone and use from other threads.
    pub fn reader(&self) -> SnapshotReader {
        SnapshotReader {
            cell: Arc::clone(&self.cell),
        }
    }

    /// Applies every record of `batch`, in order. Invalid records are
    /// refused individually and reported; the rest are still applied. The
    /// tree is consistent when this returns: totals and completeness are
    /// propagated for the whole batch.
    ///
    /// A refused record is never silent in the tree: the directory it
    /// belonged to, and all its ancestors, get
    /// [`NodeFlags::RECORDS_LOST_BELOW`] so their sizes show as lower
    /// bounds. When the directory is not known, the root gets it.
    ///
    /// Cost: linear in the records, plus one update for each directory
    /// touched by the batch and each ancestor of those, shared between
    /// them. A batch of files in one deep directory costs the depth once.
    pub fn apply(&mut self, batch: &Batch) -> ApplyReport {
        let mut report = ApplyReport::default();
        let mut pending = Pending::new();
        for (index, rec) in batch.records().iter().enumerate() {
            let index = u32::try_from(index).ok();
            if self.finished {
                self.refuse(&mut report, index, rec, ApplyError::ScanFinished);
                continue;
            }
            let name: &[u8] = match rec {
                Record::Entry(e) => match batch.name(e.name) {
                    Some(name) => name,
                    None => {
                        self.refuse(&mut report, index, rec, ApplyError::BadNameSpan);
                        continue;
                    }
                },
                _ => &[],
            };
            match self.apply_one(rec, name, &mut pending) {
                Step::Done => report.applied = report.applied.saturating_add(1),
                Step::Park => match self.park(rec, name) {
                    Ok(()) => report.parked = report.parked.saturating_add(1),
                    Err(e) => self.refuse(&mut report, index, rec, e),
                },
                Step::Reject(e) => self.refuse(&mut report, index, rec, e),
            }
            self.drain_ready(&mut pending, &mut report);
        }
        self.settle(&mut pending);
        self.rejected_total = self
            .rejected_total
            .saturating_add(u64::from(report.rejected));
        report
    }

    /// Call when the scanner will send nothing more (finished or cancelled).
    ///
    /// Records still waiting for a parent that never arrived are dropped,
    /// counted as lost, and flag the root as a lower bound
    /// ([`NodeFlags::RECORDS_LOST_BELOW`]). Everything the scan needed to
    /// remember between batches (directory keys, hard-link identities,
    /// directories still open) is released, so the aggregator then holds only the
    /// tree and its delete marks. After this, [`Aggregator::apply`] refuses
    /// every record. Calling it again does nothing.
    pub fn finish(&mut self) -> FinishReport {
        if self.finished {
            return FinishReport { unresolved: 0 };
        }
        let mut unresolved = 0u64;
        for (_, list) in self.parked.drain() {
            unresolved = unresolved.saturating_add(list.len() as u64);
        }
        self.rejected_total = self.rejected_total.saturating_add(unresolved);
        if unresolved > 0 {
            self.mark_lost(None);
        }
        self.finished = true;
        // Dropped, not cleared: clearing keeps the table's memory.
        self.keys = FastMap::default();
        self.open = FastMap::default();
        self.links = FastMap::default();
        self.parked = FastMap::default();
        self.wait_links = FastMap::default();
        self.ready = Vec::new();
        self.parked_records = 0;
        self.parked_bytes = 0;
        FinishReport { unresolved }
    }

    /// Whether [`Aggregator::finish`] has run.
    pub fn is_finished(&self) -> bool {
        self.finished
    }

    /// Records the cancellation of the scan. The tree keeps its partial
    /// results; whatever was unfinished stays incomplete.
    pub fn mark_stopped(&mut self) {
        self.store.meta.stopped = true;
    }

    /// Sets the used-bytes figure the OS reports for the volume, for
    /// progress and for noticing when counted bytes exceed it.
    pub fn set_os_used_bytes(&mut self, used: Option<u64>) {
        self.store.meta.os_used = used;
    }

    /// Publishes a snapshot of the current state, replacing the one readers
    /// see. Costs time proportional to the number of chunks plus a copy of
    /// each chunk changed since the last publication.
    pub fn publish(&mut self) -> Arc<Snapshot> {
        let snapshot = Arc::new(self.take_snapshot());
        let previous = {
            let mut slot = self.cell.write().unwrap_or_else(PoisonError::into_inner);
            std::mem::replace(&mut *slot, Arc::clone(&snapshot))
        };
        // The old snapshot may be the last reference and large to free; do
        // that after the lock is released.
        drop(previous);
        self.last_publish = Some(Instant::now());
        snapshot
    }

    /// Publishes only if at least `min_interval` has passed since the last
    /// publication. Returns whether it published. Use this to throttle
    /// publication during a scan.
    pub fn publish_if_due(&mut self, min_interval: Duration) -> bool {
        let due = self
            .last_publish
            .is_none_or(|at| at.elapsed() >= min_interval);
        if due {
            self.publish();
        }
        due
    }

    /// A snapshot of the current state that is not made available to
    /// readers.
    pub fn snapshot(&mut self) -> Snapshot {
        self.take_snapshot()
    }

    /// Ends the aggregator and returns the final state as a snapshot.
    pub fn into_snapshot(mut self) -> Snapshot {
        self.take_snapshot()
    }

    fn take_snapshot(&mut self) -> Snapshot {
        self.epoch += 1;
        self.store.meta.rejected = self.rejected_total;
        self.store.meta.parked = self.parked_records as u64;
        Snapshot {
            epoch: self.epoch,
            store: self.store.clone(),
        }
    }

    // ---- applying records -------------------------------------------------

    /// Applies the batch's collected size changes: each touched directory
    /// once, highest node id first, passing its change on to its parent.
    fn settle(&mut self, pending: &mut Pending) {
        pending.close_run();
        while let Some((node, delta)) = pending.deltas.pop_last() {
            self.store.add_to(node, delta);
            let parent = self.store.parent(node).unwrap_or(NONE);
            if parent != NONE {
                add_counts(pending.deltas.entry(parent).or_default(), delta);
            }
        }
    }

    /// Reports a refused record and flags where its bytes would have gone.
    fn refuse(
        &mut self,
        report: &mut ApplyReport,
        index: Option<u32>,
        rec: &Record,
        error: ApplyError,
    ) {
        let place = match rec {
            Record::Entry(e) => e.parent.0,
            Record::ListingDone { dir } | Record::Unreadable { dir, .. } => dir.0,
        };
        let node = self.keys.get(&place).copied();
        self.mark_lost(node);
        report.reject(index, error);
    }

    /// Flags `node` and its ancestors as having lost records. With no node
    /// (its directory is unknown) the root is flagged instead, or the flag is
    /// held back until the root arrives.
    fn mark_lost(&mut self, node: Option<u32>) {
        let target = node.or((self.store.len() > 0).then_some(0));
        match target {
            Some(at) => self.raise(at, NodeFlags::RECORDS_LOST_BELOW, true, true),
            None => self.lost_before_root = true,
        }
    }

    /// Sets `flag` on `start` (if `include_self`) and on every ancestor,
    /// stopping at the first node that already has it: a node that has the
    /// flag has it on all its ancestors too, so each node is visited once per
    /// flag over the life of the tree.
    ///
    /// `lower_bound` says the flag means "the totals here are a lower bound",
    /// which delete marks on the nodes passed must hear about.
    fn raise(&mut self, start: u32, flag: NodeFlags, include_self: bool, lower_bound: bool) {
        let mut at = if include_self {
            start
        } else {
            self.store.parent(start).unwrap_or(NONE)
        };
        for _ in 0..=self.store.len() {
            if at == NONE || self.store.flags(at).contains(flag) {
                return;
            }
            self.store.set_flag(at, flag);
            if lower_bound {
                self.became_lower_bound(at);
            }
            at = self.store.parent(at).unwrap_or(NONE);
        }
    }

    /// A node just became a lower bound. If a delete mark sits on it, the
    /// bytes that mark moved are now a lower bound too, and so are the totals
    /// above it.
    fn became_lower_bound(&mut self, node: u32) {
        if self.store.marks.is_empty() {
            return;
        }
        let Some(mark) = self.store.marks.get(&node).copied() else {
            return;
        };
        if mark.lower_bound {
            return;
        }
        Arc::make_mut(&mut self.store.marks).insert(
            node,
            Mark {
                lower_bound: true,
                ..mark
            },
        );
        self.adjust_lower_bound_marks(node, true);
    }

    /// Adds or removes one lower-bound mark in the delete totals of `node`
    /// and every ancestor.
    fn adjust_lower_bound_marks(&mut self, node: u32, add: bool) {
        let chain = self.chain(node);
        let table = Arc::make_mut(&mut self.store.delete_totals);
        for at in chain {
            let mut totals = table.get(&at).copied().unwrap_or_default();
            totals.lower_bound_marks = if add {
                totals.lower_bound_marks.saturating_add(1)
            } else {
                totals.lower_bound_marks.saturating_sub(1)
            };
            if totals.is_zero() {
                table.remove(&at);
            } else {
                table.insert(at, totals);
            }
        }
    }

    fn apply_one(&mut self, rec: &Record, name: &[u8], pending: &mut Pending) -> Step {
        match rec {
            Record::Entry(e) => self.apply_entry(e, name, pending),
            Record::ListingDone { dir } => match self.keys.get(&dir.0).copied() {
                None => Step::Park,
                Some(node) => self.finish_listing(node),
            },
            Record::Unreadable { dir, failure } => match self.keys.get(&dir.0).copied() {
                None => Step::Park,
                Some(node) => {
                    self.note_unreadable(node, *failure);
                    Step::Done
                }
            },
        }
    }

    fn apply_entry(&mut self, e: &Entry, name: &[u8], pending: &mut Pending) -> Step {
        let is_dir = e.kind == NodeKind::Dir;
        if is_dir {
            if e.key == crate::DirKey::NONE {
                return Step::Reject(ApplyError::ReservedKey);
            }
            if e.key == e.parent {
                return Step::Reject(ApplyError::SelfParent);
            }
            // Applied already, or waiting for its parent: either way the key
            // is taken, and the later record is the one refused. (A waiting
            // directory's own link is removed before it is retried.)
            if self.keys.contains_key(&e.key.0) || self.wait_links.contains_key(&e.key.0) {
                return Step::Reject(ApplyError::DuplicateKey(e.key));
            }
        }
        if e.parent == crate::DirKey::NONE {
            if self.store.len() != 0 || !self.keys.is_empty() {
                return Step::Reject(ApplyError::RootAlreadyPresent);
            }
            if !is_dir {
                return Step::Reject(ApplyError::RootMustBeDirectory);
            }
            return self.create(NONE, e, name, pending);
        }
        let Some(parent) = self.keys.get(&e.parent.0).copied() else {
            return Step::Park;
        };
        if !self.open.contains_key(&parent) {
            return Step::Reject(ApplyError::ParentComplete);
        }
        self.create(parent, e, name, pending)
    }

    /// Creates the node for `e` under `parent` (`NONE` for the root).
    fn create(&mut self, parent: u32, e: &Entry, name: &[u8], pending: &mut Pending) -> Step {
        let is_dir = e.kind == NodeKind::Dir;
        let mut logical = e.meta.logical;
        let mut allocated = e.meta.allocated;
        let mut flags = e.meta.flags.and(NodeFlags::PRODUCER_MASK);
        let mut first_sighting = None;
        let mut duplicate_of = None;
        if !is_dir && let Some(key) = e.meta.link {
            match self.links.get(&key) {
                Some(&origin) => {
                    // A later name of a file already counted: it adds no
                    // bytes, and now both names are known to be links. A
                    // key seen once flags nothing.
                    flags |= NodeFlags::HARD_LINK | NodeFlags::HARD_LINK_DUPLICATE;
                    logical = 0;
                    allocated = 0;
                    duplicate_of = Some(origin);
                }
                None => first_sighting = Some(key),
            }
        }
        let name = if name.len() > MAX_NAME_LEN {
            flags |= NodeFlags::NAME_TRUNCATED;
            name.get(..MAX_NAME_LEN).unwrap_or(name)
        } else {
            name
        };
        let (Some(new_logical), Some(new_allocated)) = (
            self.accepted_logical.checked_add(logical),
            self.accepted_allocated.checked_add(allocated),
        ) else {
            return Step::Reject(ApplyError::SizeOverflow);
        };
        if !is_dir {
            flags |= NodeFlags::COMPLETE;
        }
        let files = u32::from(!is_dir);
        let node = NewNode {
            parent,
            kind: e.kind,
            flags,
            category: e.meta.category.0,
            logical,
            allocated,
            files,
            mtime: e.meta.mtime,
            name,
        };
        let id = match self.store.push(&node, self.limits.max_nodes) {
            Ok(id) => id.0,
            Err(error) => return Step::Reject(error),
        };
        self.accepted_logical = new_logical;
        self.accepted_allocated = new_allocated;
        if let Some(key) = first_sighting {
            self.links.insert(key, id);
        }
        if let Some(origin) = duplicate_of {
            Arc::make_mut(&mut self.store.link_origin).insert(id, origin);
            self.store.set_flag(origin, NodeFlags::HARD_LINK);
        }
        if parent != NONE {
            pending.add(
                parent,
                ByteCount {
                    logical,
                    allocated,
                    files: u64::from(files),
                },
            );
        }
        if flags.contains(NodeFlags::ESTIMATED_SIZE) && parent != NONE {
            // A guess in the totals shows on every directory above it.
            self.raise(parent, NodeFlags::ESTIMATED_SIZE, true, false);
        }
        if parent == NONE && self.lost_before_root {
            self.store.set_flag(id, NodeFlags::RECORDS_LOST_BELOW);
        }
        if is_dir {
            self.keys.insert(e.key.0, id);
            self.open.insert(
                id,
                OpenDir {
                    pending: 0,
                    listing_done: false,
                },
            );
            if let Some(p) = self.open.get_mut(&parent) {
                p.pending = p.pending.saturating_add(1);
            }
            self.ready.push(e.key.0);
        }
        if let Some(failure) = e.meta.failure {
            self.note_unreadable(id, failure);
        }
        Step::Done
    }

    fn finish_listing(&mut self, node: u32) -> Step {
        match self.open.get_mut(&node) {
            Some(dir) if !dir.listing_done => {
                dir.listing_done = true;
                self.try_complete(node);
                Step::Done
            }
            // Already reported as unreadable, which counted as finished.
            _ if self.store.flags(node).contains(NodeFlags::UNREADABLE) => Step::Done,
            _ => Step::Reject(ApplyError::ListingAlreadyDone),
        }
    }

    /// Records that an entry could not be read. For a directory this also
    /// counts as finishing its listing, so the scan can complete around it.
    fn note_unreadable(&mut self, node: u32, failure: ReadFailure) {
        self.store.set_flag(node, NodeFlags::UNREADABLE);
        self.became_lower_bound(node);
        Arc::make_mut(&mut self.store.failures).insert(node, failure);
        self.raise(node, NodeFlags::UNREADABLE_BELOW, false, true);
        if let Some(dir) = self.open.get_mut(&node)
            && !dir.listing_done
        {
            dir.listing_done = true;
            self.try_complete(node);
        }
    }

    /// Marks `node` complete if its listing is done and no subdirectory is
    /// pending, then does the same for its parent, and so on upward.
    fn try_complete(&mut self, node: u32) {
        let mut at = node;
        loop {
            let ready = self
                .open
                .get(&at)
                .is_some_and(|d| d.listing_done && d.pending == 0);
            if !ready {
                return;
            }
            self.open.remove(&at);
            self.store.set_flag(at, NodeFlags::COMPLETE);
            let parent = self.store.parent(at).unwrap_or(NONE);
            if parent == NONE {
                return;
            }
            if let Some(p) = self.open.get_mut(&parent) {
                p.pending = p.pending.saturating_sub(1);
            }
            at = parent;
        }
    }

    // ---- records waiting for their parent ----------------------------------

    fn park(&mut self, rec: &Record, name: &[u8]) -> Result<(), ApplyError> {
        let waiting_for = match rec {
            Record::Entry(e) => e.parent.0,
            Record::ListingDone { dir } | Record::Unreadable { dir, .. } => dir.0,
        };
        if self.parked_records >= self.limits.max_parked_records
            || self.parked_bytes.saturating_add(name.len()) > self.limits.max_parked_name_bytes
        {
            return Err(ApplyError::ParkLimit);
        }
        if let Record::Entry(e) = rec
            && e.kind == NodeKind::Dir
        {
            // Keys are unique per scan. A second directory with the key of
            // one that is already waiting is refused: letting it replace the
            // first one's link could close a loop in the table.
            if self.wait_links.contains_key(&e.key.0) {
                return Err(ApplyError::DuplicateKey(e.key));
            }
            // The chain of waiting directories above the parent ends at a
            // key nobody has defined yet. If that key is this directory's
            // own, none of them can ever be attached.
            match self.waiting_top(e.parent.0) {
                Some(top) if top != e.key.0 => {}
                _ => return Err(ApplyError::Cycle),
            }
            self.wait_links.insert(e.key.0, e.parent.0);
        }
        self.parked_records += 1;
        self.parked_bytes += name.len();
        self.parked.entry(waiting_for).or_default().push(Parked {
            rec: *rec,
            name: name.into(),
        });
        Ok(())
    }

    /// The key at the top of the chain of waiting directories that starts at
    /// `start`: the first one that is not itself waiting on another. Halves
    /// the path as it goes, so repeated lookups stay cheap even on a long
    /// hostile chain.
    ///
    /// Insertion refuses what would close a loop, so the links should form no
    /// loop, but this does not rely on it. A chain longer than the table is
    /// itself a loop: one link on it is removed, so that later lookups are
    /// cheap again, and `None` is returned, which callers treat as a cycle.
    fn waiting_top(&mut self, start: u32) -> Option<u32> {
        let mut at = start;
        for _ in 0..=self.wait_links.len() {
            let Some(&next) = self.wait_links.get(&at) else {
                return Some(at);
            };
            match self.wait_links.get(&next).copied() {
                Some(grand) => {
                    self.wait_links.insert(at, grand);
                    at = grand;
                }
                None => at = next,
            }
        }
        // More steps than there are links: `at` is on a loop.
        self.wait_links.remove(&at);
        None
    }

    /// Applies the parked records of directories created since the last
    /// call, and of the directories those records create, until none is
    /// left. Uses an explicit work list, not recursion.
    fn drain_ready(&mut self, pending: &mut Pending, report: &mut ApplyReport) {
        while let Some(key) = self.ready.pop() {
            let Some(list) = self.parked.remove(&key) else {
                continue;
            };
            self.parked_records = self.parked_records.saturating_sub(list.len());
            for parked in list {
                self.parked_bytes = self.parked_bytes.saturating_sub(parked.name.len());
                if let Record::Entry(e) = &parked.rec
                    && e.kind == NodeKind::Dir
                {
                    self.wait_links.remove(&e.key.0);
                }
                match self.apply_one(&parked.rec, &parked.name, pending) {
                    Step::Done => report.applied = report.applied.saturating_add(1),
                    Step::Park => {
                        if let Err(e) = self.park(&parked.rec, &parked.name) {
                            self.refuse(report, None, &parked.rec, e);
                        }
                    }
                    Step::Reject(e) => self.refuse(report, None, &parked.rec, e),
                }
            }
        }
    }

    // ---- delete marks ------------------------------------------------------

    /// Marks the subtree at `id` as moved to the bin or permanently deleted.
    ///
    /// Nothing is removed: counted sizes and layout stay as they were, and
    /// the node's ancestors (and the node) report the bytes in
    /// [`NodeRef::delete_totals`](crate::NodeRef::delete_totals). Only
    /// complete subtrees can be marked. A node inside an already marked
    /// subtree cannot be marked; a node with marked descendants can, and its
    /// mark covers the bytes not already marked. Returns the bytes this mark
    /// moved.
    ///
    /// A subtree with unreadable parts or lost records can be marked, but
    /// its bytes are then a lower bound: the result says so
    /// ([`Moved::lower_bound`]), and so do the delete totals of the node and
    /// its ancestors ([`DeleteTotals::is_lower_bound`]), so a confirmation
    /// can say "at least". This also holds if the subtree becomes a lower
    /// bound after it was marked.
    pub fn mark_deleted(&mut self, id: NodeId, state: DeleteState) -> Result<Moved, DeleteError> {
        if state == DeleteState::None {
            return Err(DeleteError::NoState);
        }
        let node = id.0;
        if !self.store.contains(node) {
            return Err(DeleteError::UnknownNode(id));
        }
        if !self.store.flags(node).contains(NodeFlags::COMPLETE) {
            return Err(DeleteError::NotComplete(id));
        }
        if self.store.marks.contains_key(&node) {
            return Err(DeleteError::AlreadyMarked(id));
        }
        self.reject_if_ancestor_marked(node, id)?;
        let existing = self.store.delete_totals_of(node);
        let already = existing
            .in_bin
            .checked_add(existing.deleted)
            .ok_or(DeleteError::Inconsistent(id))?;
        let moved = self
            .store
            .totals(node)
            .checked_sub(already)
            .ok_or(DeleteError::Inconsistent(id))?;
        let lower_bound = self.store.flags(node).intersects(
            NodeFlags::UNREADABLE | NodeFlags::UNREADABLE_BELOW | NodeFlags::RECORDS_LOST_BELOW,
        );
        let mut updated = Vec::new();
        for at in self.chain(node) {
            let mut totals = self.store.delete_totals_of(at);
            let field = totals
                .field_mut(state)
                .ok_or(DeleteError::Inconsistent(id))?;
            *field = field
                .checked_add(moved)
                .ok_or(DeleteError::Inconsistent(id))?;
            if lower_bound {
                totals.lower_bound_marks = totals.lower_bound_marks.saturating_add(1);
            }
            updated.push((at, totals));
        }
        let table = Arc::make_mut(&mut self.store.delete_totals);
        for (at, totals) in updated {
            table.insert(at, totals);
        }
        Arc::make_mut(&mut self.store.marks).insert(
            node,
            Mark {
                state,
                moved,
                lower_bound,
            },
        );
        self.store.set_delete_state(node, state);
        Ok(Moved {
            bytes: moved,
            lower_bound,
        })
    }

    /// Reverses [`Aggregator::mark_deleted`] for `id`, restoring every
    /// total exactly.
    ///
    /// Refused with [`DeleteError::AncestorMarked`] while an ancestor is
    /// marked, because the node's bytes are accounted in the ancestor's mark.
    /// Undo in the reverse of the order the marks were made (the way an undo
    /// stack does) and this never arises.
    pub fn unmark_deleted(&mut self, id: NodeId) -> Result<Moved, DeleteError> {
        let node = id.0;
        if !self.store.contains(node) {
            return Err(DeleteError::UnknownNode(id));
        }
        let Some(mark) = self.store.marks.get(&node).copied() else {
            return Err(DeleteError::NotMarked(id));
        };
        self.reject_if_ancestor_marked(node, id)?;
        let mut updated = Vec::new();
        for at in self.chain(node) {
            let mut totals = self.store.delete_totals_of(at);
            let field = totals
                .field_mut(mark.state)
                .ok_or(DeleteError::Inconsistent(id))?;
            *field = field
                .checked_sub(mark.moved)
                .ok_or(DeleteError::Inconsistent(id))?;
            if mark.lower_bound {
                totals.lower_bound_marks = totals
                    .lower_bound_marks
                    .checked_sub(1)
                    .ok_or(DeleteError::Inconsistent(id))?;
            }
            updated.push((at, totals));
        }
        let table = Arc::make_mut(&mut self.store.delete_totals);
        for (at, totals) in updated {
            if totals.is_zero() {
                table.remove(&at);
            } else {
                table.insert(at, totals);
            }
        }
        Arc::make_mut(&mut self.store.marks).remove(&node);
        self.store.set_delete_state(node, DeleteState::None);
        Ok(Moved {
            bytes: mark.moved,
            lower_bound: mark.lower_bound,
        })
    }

    fn reject_if_ancestor_marked(&self, node: u32, id: NodeId) -> Result<(), DeleteError> {
        let mut at = self.store.parent(node).unwrap_or(NONE);
        for _ in 0..=self.store.len() {
            if at == NONE {
                return Ok(());
            }
            if self.store.delete_state(at) != DeleteState::None {
                return Err(DeleteError::AncestorMarked(id));
            }
            at = self.store.parent(at).unwrap_or(NONE);
        }
        Err(DeleteError::Inconsistent(id))
    }

    /// `node` and its ancestors, nearest first.
    fn chain(&self, node: u32) -> Vec<u32> {
        let mut out = Vec::new();
        let mut at = node;
        while at != NONE && out.len() <= self.store.len() as usize {
            out.push(at);
            at = self.store.parent(at).unwrap_or(NONE);
        }
        out
    }
}

impl DeleteTotals {
    /// Bytes in either state.
    pub fn total(&self) -> ByteCount {
        ByteCount {
            logical: self.in_bin.logical.saturating_add(self.deleted.logical),
            allocated: self.in_bin.allocated.saturating_add(self.deleted.allocated),
            files: self.in_bin.files.saturating_add(self.deleted.files),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::batch::{Meta, NameSpan};
    use crate::types::DirKey;

    #[test]
    fn a_name_outside_the_batch_is_refused_and_flags_its_directory() {
        let mut agg = Aggregator::new();
        let mut b = Batch::new();
        b.dir(DirKey(0), DirKey::NONE, b"r", &Meta::default())
            .unwrap();
        b.dir(DirKey(1), DirKey(0), b"a", &Meta::default()).unwrap();
        b.push_raw(Record::Entry(Entry {
            kind: NodeKind::File,
            key: DirKey(0),
            parent: DirKey(1),
            name: NameSpan {
                offset: 1,
                len: 100,
            },
            meta: Meta::default(),
        }));
        let report = agg.apply(&b);
        assert_eq!(
            report.first_errors,
            vec![(Some(2), ApplyError::BadNameSpan)]
        );
        let snap = agg.publish();
        for id in [0, 1] {
            let n = snap.node(NodeId::from_index(id)).unwrap();
            assert!(n.has_lost_records_below());
            assert!(n.bytes_are_lower_bound());
        }
        snap.verify().unwrap();
    }
}

#[cfg(test)]
mod waiting_tests {
    use super::*;

    #[test]
    fn a_loop_in_the_waiting_table_is_detected_and_broken_once() {
        // Not reachable through `apply`, which refuses what would make one;
        // built by hand to check that the lookup does not trust that.
        let mut agg = Aggregator::new();
        for (key, waits_for) in [(1, 2), (2, 3), (3, 1), (10, 1)] {
            agg.wait_links.insert(key, waits_for);
        }
        assert_eq!(agg.waiting_top(10), None);
        // The loop is gone: the lookups that follow end, and are cheap.
        assert!(agg.waiting_top(10).is_some());
        assert!(agg.waiting_top(1).is_some());
        assert!(agg.wait_links.len() <= 3);
    }
}
