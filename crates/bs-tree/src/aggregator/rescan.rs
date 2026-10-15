//! Replacing one directory's contents with a fresh scan of it, in the tree
//! of a finished scan.
//!
//! The directory keeps its node id, its parent, its name and its place among
//! its siblings. Its old descendants stay in the arena (ids are never reused)
//! but are cut off from the tree: the direct children get a "detached"
//! parent, so nothing reachable from the root leads to them, and
//! [`NodeRef::is_detached`](crate::NodeRef::is_detached) tells a view that
//! still holds one of those ids.
//!
//! The replacement happens when the scan's root record arrives, not when the
//! rescan begins: a scan that cannot even open the folder changes nothing.
//! From then on the folder and every folder above it are incomplete until the
//! new listing finishes, the way they were during the first scan.

use super::{Aggregator, OpenDir, Step};
use crate::batch::Entry;
use crate::error::{ApplyError, RescanError};
use crate::store::{DETACHED, NONE};
use crate::types::{ByteCount, DeleteState, FileKey, NodeFlags, NodeId, NodeKind, ReadFailure};
use std::collections::BTreeMap;
use std::sync::Arc;

/// Flags that describe a folder's own state or what is below it, and are
/// worked out again by the new scan.
const FRESH_START: NodeFlags = NodeFlags::from_bits_truncate(
    NodeFlags::COMPLETE.bits()
        | NodeFlags::UNREADABLE.bits()
        | NodeFlags::UNREADABLE_BELOW.bits()
        | NodeFlags::RECORDS_LOST_BELOW.bits()
        | NodeFlags::ESTIMATED_SIZE.bits()
        | NodeFlags::CLOUD_PLACEHOLDER.bits()
        | NodeFlags::NOT_FOLLOWED.bits()
        | NodeFlags::SPARSE_OR_COMPRESSED.bits(),
);

/// The producer flags that make sense on a folder.
const FOLDER_FLAGS: NodeFlags = NodeFlags::from_bits_truncate(
    NodeFlags::CLOUD_PLACEHOLDER.bits()
        | NodeFlags::NOT_FOLLOWED.bits()
        | NodeFlags::SPARSE_OR_COMPRESSED.bits()
        | NodeFlags::ESTIMATED_SIZE.bits(),
);

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Phase {
    /// Waiting for the scan's root record.
    Waiting,
    /// The folder's contents were replaced; the new listing is arriving.
    Bound,
    /// The root record named another folder than the one being rescanned.
    /// Everything the scan sends is ignored.
    Refused,
}

pub(super) struct Rescan {
    pub node: u32,
    pub expected: Option<Box<[u8]>>,
    pub phase: Phase,
    pub report: RescanReport,
}

/// What a rescan did to the tree. Read it with
/// [`Aggregator::rescan_report`] after the rescan finished.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RescanReport {
    /// The folder that was rescanned.
    pub node: NodeId,
    /// The folder's old contents were replaced. `false` when the scan never
    /// delivered the folder's record (it could not be opened, or was
    /// stopped first): the tree is then exactly what it was.
    pub replaced: bool,
    /// The scan's root record named a different folder than the one being
    /// rescanned, so the whole scan was ignored.
    pub refused: bool,
    /// The totals the old contents held (the folder's own entry excluded).
    pub removed: ByteCount,
    /// Nodes cut off from the tree (they stay in the arena).
    pub detached_nodes: u32,
    /// Delete marks on the old contents that were dropped with them. The
    /// files they named are not in the folder any more, or they would have
    /// been found again.
    pub marks_dropped: u32,
    /// Hard-linked files whose bytes were counted in the old contents and
    /// are now counted at a surviving name outside them.
    pub link_carriers_moved: u32,
}

fn is_in(sorted: &[u32], id: u32) -> bool {
    sorted.binary_search(&id).is_ok()
}

/// Removes from `map` every entry whose key is in `dead`.
fn forget<V: Clone>(map: &mut Arc<crate::hash::FastMap<u32, V>>, dead: &[u32]) {
    if map.keys().any(|&k| is_in(dead, k)) {
        Arc::make_mut(map).retain(|&k, _| !is_in(dead, k));
    }
}

/// Everything the replacement needs to know, worked out before anything is
/// changed.
struct Plan {
    /// Every node below the folder, sorted by id.
    dead: Vec<u32>,
    /// The folder's children (the tops of the cut-off subtrees).
    tops: Vec<u32>,
    /// What the old contents hold.
    removed: ByteCount,
    /// The folder's own entry, as counted now.
    own_old: ByteCount,
    /// Marks on the old contents, ancestors before descendants.
    marks: Vec<u32>,
}

impl Aggregator {
    /// Begins replacing the contents of directory `id` with a fresh scan of
    /// it. Feed the scan's batches to [`Aggregator::apply`] as for any scan
    /// (the first record is the folder itself, with no parent) and call
    /// [`Aggregator::finish`] when the scan ends.
    ///
    /// `expected_root_name` is the name the scan's root record must carry,
    /// the folder's path as the scanner writes the root; when it differs the
    /// whole scan is ignored and the tree stays as it was, which is how a
    /// path that was swapped for a link since the first scan is refused.
    /// `None` skips the check.
    ///
    /// When the root record arrives:
    ///
    /// * the old contents are cut off, and their bytes leave the folder's
    ///   totals and its ancestors';
    /// * delete marks inside them are dropped, with their bytes, exactly as
    ///   undoing them would;
    /// * a hard-linked file whose bytes were counted inside and that has
    ///   another name outside is counted at that name from now on, and a
    ///   name inside whose file is counted outside is forgotten with the
    ///   rest. A file counted inside whose other names are all marked
    ///   deleted is not counted any more. Names the scan finds again are
    ///   matched against the files counted anywhere else in the tree;
    /// * the folder and every complete folder above it become incomplete
    ///   again, and complete again as the new listing finishes. Their
    ///   "something below could not be read" flags are worked out again
    ///   from what is left; "records were lost" and "estimated" flags above
    ///   the folder are kept, because the tree does not remember which
    ///   child they came from, so they can only be too cautious;
    /// * delete marks outside the folder are untouched.
    ///
    /// A file is matched with the files counted elsewhere only if the first
    /// scan could tell it has several names (the scanner flagged it, or a
    /// second name was seen). A name that is linked to a file only after the
    /// first scan, for a file the scanner had no reason to flag, is counted
    /// again at the new name.
    ///
    /// Cost: linear in the old contents.
    ///
    /// # Errors
    ///
    /// [`RescanError`] if the node cannot be rescanned; nothing changed.
    pub fn begin_rescan(
        &mut self,
        id: NodeId,
        expected_root_name: Option<&[u8]>,
    ) -> Result<(), RescanError> {
        if self.rescan.is_some() {
            return Err(RescanError::Busy);
        }
        if !self.finished {
            return Err(RescanError::ScanRunning);
        }
        let node = id.0;
        if !self.store.contains(node) {
            return Err(RescanError::UnknownNode(id));
        }
        if self.store.kind(node) != NodeKind::Dir {
            return Err(RescanError::NotADirectory(id));
        }
        if self.store.is_detached(node) {
            return Err(RescanError::Detached(id));
        }
        if self
            .chain(node)
            .into_iter()
            .any(|at| self.store.delete_state(at) != DeleteState::None)
        {
            return Err(RescanError::Marked(id));
        }
        self.finished = false;
        self.links = std::mem::take(&mut self.link_keys);
        self.last_rescan = None;
        self.rescan = Some(Rescan {
            node,
            expected: expected_root_name.map(Box::from),
            phase: Phase::Waiting,
            report: RescanReport {
                node: id,
                replaced: false,
                refused: false,
                removed: ByteCount::default(),
                detached_nodes: 0,
                marks_dropped: 0,
                link_carriers_moved: 0,
            },
        });
        Ok(())
    }

    /// Whether a rescan has begun and not yet finished.
    pub fn is_rescanning(&self) -> bool {
        self.rescan.is_some()
    }

    /// Whether the running rescan was refused: the scan's root record named
    /// another folder than the one being rescanned, and everything the scan
    /// sends is being ignored. A driver should stop such a scan.
    pub fn rescan_refused(&self) -> bool {
        self.rescan
            .as_ref()
            .is_some_and(|r| r.phase == Phase::Refused)
    }

    /// The folder being rescanned, while a rescan runs.
    pub fn rescan_node(&self) -> Option<NodeId> {
        self.rescan.as_ref().map(|r| NodeId(r.node))
    }

    /// What the last rescan did; `None` before any rescan finished, and while
    /// one runs.
    pub fn rescan_report(&self) -> Option<RescanReport> {
        self.last_rescan
    }

    /// Records that the last rescan stopped on an error after the folder's
    /// contents were replaced: the folder is marked unreadable, with the
    /// error, and stays incomplete. Returns `false` (and changes nothing)
    /// when the last rescan did not replace the folder, or the folder is
    /// already marked.
    pub fn mark_rescan_failed(&mut self, failure: ReadFailure) -> bool {
        let Some(report) = self.last_rescan else {
            return false;
        };
        let node = report.node.0;
        if !self.finished
            || !report.replaced
            || self.store.flags(node).contains(NodeFlags::UNREADABLE)
        {
            return false;
        }
        self.note_unreadable(node, failure);
        true
    }

    /// The scan's root record, for a rescan: replaces the folder's contents.
    pub(super) fn bind_rescan_root(&mut self, e: &Entry, name: &[u8]) -> Step {
        let Some(rescan) = self.rescan.as_ref() else {
            return Step::Reject(ApplyError::RootAlreadyPresent);
        };
        if rescan.phase != Phase::Waiting {
            return Step::Reject(ApplyError::RootAlreadyPresent);
        }
        if e.kind != NodeKind::Dir {
            return Step::Reject(ApplyError::RootMustBeDirectory);
        }
        let node = rescan.node;
        if rescan
            .expected
            .as_deref()
            .is_some_and(|expected| expected != name)
        {
            if let Some(r) = self.rescan.as_mut() {
                r.phase = Phase::Refused;
                r.report.refused = true;
            }
            return Step::Reject(ApplyError::RescanRootMismatch);
        }
        let Some(plan) = self.plan_replacement(node, e) else {
            return Step::Reject(ApplyError::RescanInconsistent);
        };
        if !self.drop_marks(&plan) {
            return Step::Reject(ApplyError::RescanInconsistent);
        }
        let moved = self.hand_over_links(&plan.dead);
        self.cut_off(node, e, &plan);

        // The folder and the complete folders above it count again.
        for at in self.chain(node).into_iter().skip(1) {
            if self.store.flags(at).contains(NodeFlags::COMPLETE) {
                self.store.clear_flag(at, NodeFlags::COMPLETE);
                self.open.insert(
                    at,
                    OpenDir {
                        pending: 1,
                        listing_done: true,
                    },
                );
            }
        }
        self.keys.insert(e.key.0, node);
        self.open.insert(
            node,
            OpenDir {
                pending: 0,
                listing_done: false,
            },
        );
        self.ready.push(e.key.0);
        if self.store.flags(node).contains(NodeFlags::ESTIMATED_SIZE) {
            self.raise(node, NodeFlags::ESTIMATED_SIZE, true, false);
        }
        if let Some(r) = self.rescan.as_mut() {
            r.phase = Phase::Bound;
            r.report.replaced = true;
            r.report.removed = plan.removed;
            r.report.detached_nodes = u32::try_from(plan.dead.len()).unwrap_or(u32::MAX);
            r.report.marks_dropped = u32::try_from(plan.marks.len()).unwrap_or(u32::MAX);
            r.report.link_carriers_moved = moved;
        }
        if let Some(failure) = e.meta.failure {
            self.note_unreadable(node, failure);
        }
        Step::Done
    }

    /// Works out what the replacement touches. `None` if the tree's own
    /// numbers do not add up (which they should always do); nothing has been
    /// changed then.
    fn plan_replacement(&self, node: u32, e: &Entry) -> Option<Plan> {
        let len = self.store.len();
        let mut tops = Vec::new();
        let mut removed = ByteCount::default();
        let mut next = self.store.first_child(node)?;
        while next != NONE {
            if tops.len() > len as usize {
                return None;
            }
            tops.push(next);
            removed = removed.checked_add(self.store.totals(next))?;
            next = self.store.next_sibling(next)?;
        }
        let mut dead: Vec<u32> = Vec::new();
        let mut stack = tops.clone();
        while let Some(n) = stack.pop() {
            if dead.len() > len as usize {
                return None;
            }
            dead.push(n);
            if self.store.kind(n) == NodeKind::Dir {
                let mut child = self.store.first_child(n)?;
                while child != NONE {
                    stack.push(child);
                    child = self.store.next_sibling(child)?;
                }
            }
        }
        dead.sort_unstable();
        let own_old = self.store.totals(node).checked_sub(removed)?;
        // The folders above hold the old contents too.
        for at in self.chain(node) {
            self.store.totals(at).checked_sub(removed)?;
        }
        // Room for the folder's own entry growing.
        self.accepted_logical
            .checked_add(e.meta.logical.saturating_sub(own_old.logical))?;
        self.accepted_allocated
            .checked_add(e.meta.allocated.saturating_sub(own_old.allocated))?;
        let mut marks: Vec<u32> = self
            .store
            .marks
            .keys()
            .copied()
            .filter(|&k| is_in(&dead, k))
            .collect();
        // Ancestors first: a mark can only be undone once those above it
        // are.
        marks.sort_unstable();
        Some(Plan {
            dead,
            tops,
            removed,
            own_old,
            marks,
        })
    }

    /// Undoes the delete marks inside the old contents.
    fn drop_marks(&mut self, plan: &Plan) -> bool {
        plan.marks
            .iter()
            .all(|&m| self.unmark_deleted(NodeId(m)).is_ok())
    }

    /// Gives the bytes of hard-linked files counted inside the old contents
    /// to a surviving name outside them, and forgets names inside whose file
    /// is counted outside. Returns how many files changed carrier.
    fn hand_over_links(&mut self, dead: &[u32]) -> u32 {
        if self.store.link_origin.is_empty() && self.store.linked.is_empty() {
            return 0;
        }
        // Names outside that point at an origin inside.
        let mut survivors: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
        for (&dup, &origin) in self.store.link_origin.iter() {
            if is_in(dead, origin) && !is_in(dead, dup) {
                survivors.entry(origin).or_default().push(dup);
            }
        }
        let mut moved = 0u32;
        for (origin, mut names) in survivors {
            names.sort_unstable();
            // A name that is itself marked deleted cannot take the bytes
            // over: they would show as still on disk beside a mark that
            // says they are in the bin.
            let carrier = names
                .iter()
                .copied()
                .find(|&n| self.effective_state(n) == DeleteState::None);
            let first = carrier.or_else(|| names.first().copied());
            let Some(first) = first else { continue };
            let origin_flags = self.store.flags(origin);
            if carrier.is_some() {
                let bytes = ByteCount {
                    logical: self.store.logical(origin),
                    allocated: self.store.allocated(origin),
                    files: 0,
                };
                // The name itself, then everything above it.
                for at in self.chain(first) {
                    self.store.add_to(at, bytes);
                }
                moved = moved.saturating_add(1);
            }
            self.store.clear_flag(first, NodeFlags::HARD_LINK_DUPLICATE);
            self.store.set_flag(first, NodeFlags::HARD_LINK);
            {
                let origins = Arc::make_mut(&mut self.store.link_origin);
                origins.remove(&first);
                for &other in &names {
                    if other != first {
                        origins.insert(other, first);
                    }
                }
            }
            Arc::make_mut(&mut self.store.linked).insert(first, ());
            if origin_flags.contains(NodeFlags::SHARED) {
                self.store.set_flag(first, NodeFlags::SHARED);
                let exclusive = self.store.exclusive.get(&origin).copied().unwrap_or(0);
                let allocated = self.store.allocated(first);
                Arc::make_mut(&mut self.store.exclusive).insert(first, exclusive.min(allocated));
            }
            if origin_flags.contains(NodeFlags::ESTIMATED_SIZE) {
                self.store.set_flag(first, NodeFlags::ESTIMATED_SIZE);
                if let Some(parent) = self.store.live_parent(first) {
                    self.raise(parent, NodeFlags::ESTIMATED_SIZE, true, false);
                }
            }
            // Later sightings of the same file match the new carrier.
            for node in self.links.values_mut() {
                if *node == origin {
                    *node = first;
                }
            }
        }
        moved
    }

    /// The delete state in effect for `node`: its own, else its nearest
    /// marked ancestor's.
    fn effective_state(&self, node: u32) -> DeleteState {
        self.chain(node)
            .into_iter()
            .map(|at| self.store.delete_state(at))
            .find(|s| *s != DeleteState::None)
            .unwrap_or(DeleteState::None)
    }

    /// Cuts the old contents off, takes their bytes out of the totals,
    /// forgets their side-table entries and resets the folder's own state.
    fn cut_off(&mut self, node: u32, e: &Entry, plan: &Plan) {
        let chain = self.chain(node);
        // The old contents' bytes leave the folder and everything above.
        for &at in &chain {
            self.store.sub_from(at, plan.removed);
        }
        self.accepted_logical = self.accepted_logical.saturating_sub(plan.removed.logical);
        self.accepted_allocated = self
            .accepted_allocated
            .saturating_sub(plan.removed.allocated);
        // The folder's own entry may have a new size.
        let grow = ByteCount {
            logical: e.meta.logical.saturating_sub(plan.own_old.logical),
            allocated: e.meta.allocated.saturating_sub(plan.own_old.allocated),
            files: 0,
        };
        let shrink = ByteCount {
            logical: plan.own_old.logical.saturating_sub(e.meta.logical),
            allocated: plan.own_old.allocated.saturating_sub(e.meta.allocated),
            files: 0,
        };
        for &at in &chain {
            self.store.add_to(at, grow);
            self.store.sub_from(at, shrink);
        }
        self.accepted_logical = self.accepted_logical.saturating_add(grow.logical);
        self.accepted_allocated = self.accepted_allocated.saturating_add(grow.allocated);

        // Cut the children off. Their own links and sizes stay as they were,
        // so a detached subtree is still a consistent tree.
        for &top in &plan.tops {
            self.store.set_parent(top, DETACHED);
        }
        self.store.set_first_child(node, NONE);
        self.store.meta.detached = self
            .store
            .meta
            .detached
            .saturating_add(plan.dead.len() as u64);

        // Nothing may name a cut-off node any more. Marks were undone
        // already; the totals entries of nodes that held marks are gone with
        // them.
        forget(&mut self.store.failures, &plan.dead);
        forget(&mut self.store.synthetic, &plan.dead);
        forget(&mut self.store.exclusive, &plan.dead);
        forget(&mut self.store.linked, &plan.dead);
        forget(&mut self.store.link_origin, &plan.dead);
        forget(&mut self.store.delete_totals, &plan.dead);
        self.links.retain(|_, n| !is_in(&plan.dead, *n));

        // The folder starts over.
        if self.store.failures.contains_key(&node) {
            Arc::make_mut(&mut self.store.failures).remove(&node);
        }
        self.store.clear_flag(node, FRESH_START);
        self.store.set_flag(node, e.meta.flags.and(FOLDER_FLAGS));
        self.store
            .set_description(node, e.meta.mtime, e.meta.category.0);

        self.forget_unreadable_above(node);
    }

    /// Works out again which folders above `node` still have something
    /// unreadable below, now that `node` no longer does.
    fn forget_unreadable_above(&mut self, node: u32) {
        let mut at = self.store.live_parent(node);
        while let Some(a) = at {
            if !self.store.flags(a).contains(NodeFlags::UNREADABLE_BELOW) {
                return;
            }
            let mut child = self.store.first_child(a).unwrap_or(NONE);
            let mut seen = 0u32;
            while child != NONE && seen <= self.store.len() {
                if self
                    .store
                    .flags(child)
                    .intersects(NodeFlags::UNREADABLE | NodeFlags::UNREADABLE_BELOW)
                {
                    return;
                }
                seen += 1;
                child = self.store.next_sibling(child).unwrap_or(NONE);
            }
            self.store.clear_flag(a, NodeFlags::UNREADABLE_BELOW);
            at = self.store.live_parent(a);
        }
    }

    /// The links kept after a scan: names of files with more than one name.
    pub(super) fn keep_link_keys(
        &self,
        links: crate::hash::FastMap<FileKey, u32>,
    ) -> crate::hash::FastMap<FileKey, u32> {
        links
            .into_iter()
            .filter(|(_, n)| self.store.flags(*n).contains(NodeFlags::HARD_LINK))
            .collect()
    }
}
