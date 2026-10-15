//! Immutable, internally consistent views of the tree.

use crate::error::Violation;
use crate::store::{NONE, Store};
use crate::types::{
    ByteCount, Category, DeleteState, DeleteTotals, NodeFlags, NodeId, NodeKind, ReadFailure,
};

/// How far along the scan is, for a progress display.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Progress {
    /// Counted allocated bytes divided by the bytes the OS reports used,
    /// between 0 and 1. Exactly 1 once the scan is complete.
    Ratio(f64),
    /// The ratio is meaningless (the OS figure is unknown, or the counted
    /// bytes already exceed it because of sharing or compression), so show
    /// the number of files counted so far.
    Files(u64),
}

/// What deleting a node would free, as far as the tree can tell.
///
/// The two figures bracket the truth. Allocated bytes that other files also
/// use (reflinks, snapshots) stay on disk when only one of the files goes, so
/// the answer lies between the bytes only this subtree uses and everything the
/// subtree is counted as holding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FreeEstimate {
    /// Bytes that are certainly freed: what is not shared with anything
    /// outside the files deleted together.
    pub at_least: u64,
    /// The most that can be freed: the node's allocated bytes still on disk.
    pub at_most: u64,
}

impl FreeEstimate {
    /// Whether the two figures agree, so the answer is exact (as far as the
    /// tree's sizes are).
    pub fn is_exact(&self) -> bool {
        self.at_least == self.at_most
    }
}

/// A point-in-time copy of the whole tree. Never changes after it is
/// published; share it freely between threads.
///
/// Every snapshot is taken between batches, so within one snapshot a
/// directory's totals equal its own entry plus the sum of its children, and
/// a directory marked complete has only complete subdirectories.
pub struct Snapshot {
    pub(crate) epoch: u64,
    pub(crate) store: Store,
}

impl Snapshot {
    /// Publication counter: 0 for the empty tree before any publication,
    /// then 1, 2, 3, … Later snapshots have larger epochs.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Number of nodes.
    pub fn len(&self) -> u32 {
        self.store.len()
    }

    /// Whether the tree has no nodes (the scan root has not arrived).
    pub fn is_empty(&self) -> bool {
        self.store.len() == 0
    }

    /// The scan root.
    pub fn root(&self) -> Option<NodeRef<'_>> {
        self.node(NodeId::ROOT)
    }

    /// Looks a node up. `None` if the id is not in this snapshot.
    pub fn node(&self, id: NodeId) -> Option<NodeRef<'_>> {
        self.store.contains(id.0).then_some(NodeRef {
            store: &self.store,
            id,
        })
    }

    /// Whether the whole scan finished and nothing was lost: the root is
    /// complete, no record was rejected and none is waiting for a parent.
    /// A "complete" tree can still hold unreadable directories; see
    /// [`NodeRef::bytes_are_lower_bound`].
    pub fn scan_complete(&self) -> bool {
        self.lost_records() == 0 && self.root().is_some_and(|r| r.is_complete())
    }

    /// Whether the scan was cancelled. The tree keeps what was counted and
    /// the unfinished parts stay incomplete.
    pub fn scan_stopped(&self) -> bool {
        self.store.meta.stopped
    }

    /// Records the tree could not use: rejected as invalid, or still waiting
    /// for a parent directory. While this is above zero, totals may be
    /// short.
    pub fn lost_records(&self) -> u64 {
        self.store.meta.rejected + self.store.meta.parked
    }

    /// The used-bytes figure the OS reported for the volume, if one was set.
    pub fn os_used_bytes(&self) -> Option<u64> {
        self.store.meta.os_used
    }

    /// Whether the counted allocated bytes exceed what the OS reports used.
    /// This happens with sharing and compression; a percentage would mislead.
    pub fn counted_exceeds_used(&self) -> bool {
        match (self.root(), self.store.meta.os_used) {
            (Some(root), Some(used)) => root.allocated() > used,
            _ => false,
        }
    }

    /// Progress for display; see [`Progress`].
    pub fn progress(&self) -> Progress {
        let Some(root) = self.root() else {
            return Progress::Files(0);
        };
        if self.scan_complete() {
            return Progress::Ratio(1.0);
        }
        match self.store.meta.os_used {
            Some(used) if used > 0 && root.allocated() <= used => {
                Progress::Ratio(root.allocated() as f64 / used as f64)
            }
            _ => Progress::Files(u64::from(root.file_count())),
        }
    }

    /// The children of `id` ordered by counted allocated size, largest
    /// first, ties broken by lower node id. Empty for unknown ids and files.
    pub fn children_by_size(&self, id: NodeId) -> Vec<NodeId> {
        let Some(node) = self.node(id) else {
            return Vec::new();
        };
        let mut children: Vec<NodeId> = node.children().map(|c| c.id()).collect();
        children.sort_by(|a, b| {
            self.store
                .allocated(b.0)
                .cmp(&self.store.allocated(a.0))
                .then(a.0.cmp(&b.0))
        });
        children
    }

    /// The path of `id` as raw bytes: the root's name, then each component,
    /// joined with `separator`. No separator is added after a root name that
    /// already ends with one (`/`, `C:\`). `None` for unknown ids.
    pub fn path(&self, id: NodeId, separator: u8) -> Option<Vec<u8>> {
        let mut parts: Vec<&[u8]> = Vec::new();
        let mut at = self.node(id)?;
        loop {
            parts.push(at.name());
            match at.parent() {
                Some(parent) => at = parent,
                None => break,
            }
        }
        let mut out = Vec::new();
        for (i, part) in parts.iter().rev().enumerate() {
            if i > 0 && out.last() != Some(&separator) {
                out.push(separator);
            }
            out.extend_from_slice(part);
        }
        Some(out)
    }

    /// Checks the tree's invariants over every node. Costs one pass over the
    /// tree; meant for tests and debugging, not for the frame loop.
    ///
    /// Checked: parent/child links agree; a directory's file count is the sum
    /// of its children's; its sizes are at least the sum of its children's;
    /// a complete directory has only complete subdirectories; unreadable,
    /// lost-record and estimated-size flags show on every ancestor; both names of a hard link
    /// are flagged; delete totals never exceed the counted
    /// totals; only directories have children.
    pub fn verify(&self) -> Result<(), Violation> {
        let s = &self.store;
        let bad = |node: u32, what: &'static str| Violation {
            node: NodeId(node),
            what,
        };
        for id in 0..s.len() {
            let Some(me) = self.node(NodeId(id)) else {
                return Err(bad(id, "node missing from its chunk"));
            };
            let parent = s.parent(id).unwrap_or(NONE);
            if (id == 0) != (parent == NONE) {
                return Err(bad(id, "only the root may lack a parent"));
            }
            if parent != NONE {
                if parent >= id {
                    return Err(bad(id, "parent was not created before its child"));
                }
                if s.kind(parent) != NodeKind::Dir {
                    return Err(bad(id, "parent is not a directory"));
                }
            }
            let flags = me.flags();
            let is_dir = me.kind() == NodeKind::Dir;
            if !is_dir && s.first_child(id) != Some(NONE) {
                return Err(bad(id, "non-directory has children"));
            }
            if !is_dir && !flags.contains(NodeFlags::COMPLETE) {
                return Err(bad(id, "non-directory is not complete"));
            }
            let mut sum = ByteCount::default();
            let mut seen = 0u32;
            let mut child_unreadable = false;
            let mut child_lost = false;
            let mut child_estimated = false;
            let mut all_children_complete = true;
            let mut next = s.first_child(id).unwrap_or(NONE);
            while next != NONE {
                seen += 1;
                if seen > s.len() {
                    return Err(bad(id, "child list does not end"));
                }
                if s.parent(next) != Some(id) {
                    return Err(bad(next, "child lists a different parent"));
                }
                let child_flags = s.flags(next);
                child_unreadable |=
                    child_flags.intersects(NodeFlags::UNREADABLE | NodeFlags::UNREADABLE_BELOW);
                child_lost |= child_flags.contains(NodeFlags::RECORDS_LOST_BELOW);
                child_estimated |= child_flags.contains(NodeFlags::ESTIMATED_SIZE);
                if !child_flags.contains(NodeFlags::COMPLETE) {
                    all_children_complete = false;
                }
                sum = sum
                    .checked_add(s.totals(next))
                    .ok_or_else(|| bad(id, "children's totals overflow"))?;
                next = s.next_sibling(next).unwrap_or(NONE);
            }
            let total = s.totals(id);
            if is_dir {
                if total.files != sum.files {
                    return Err(bad(id, "file count is not the sum of the children's"));
                }
                if total.logical < sum.logical || total.allocated < sum.allocated {
                    return Err(bad(id, "size is less than the sum of the children's"));
                }
                if flags.contains(NodeFlags::COMPLETE) && !all_children_complete {
                    return Err(bad(id, "complete directory has an incomplete child"));
                }
                if child_unreadable && !flags.contains(NodeFlags::UNREADABLE_BELOW) {
                    return Err(bad(id, "unreadable child not reflected in parent"));
                }
                if child_lost && !flags.contains(NodeFlags::RECORDS_LOST_BELOW) {
                    return Err(bad(id, "lost records below not reflected in parent"));
                }
                if child_estimated && !flags.contains(NodeFlags::ESTIMATED_SIZE) {
                    return Err(bad(id, "estimated size below not reflected in parent"));
                }
            }
            if flags.contains(NodeFlags::HARD_LINK_DUPLICATE) {
                let origin = s.link_origin.get(&id).copied();
                if !origin.is_some_and(|o| s.flags(o).contains(NodeFlags::HARD_LINK))
                    || !flags.contains(NodeFlags::HARD_LINK)
                {
                    return Err(bad(
                        id,
                        "hard-link duplicate and origin are not both flagged",
                    ));
                }
            }
            if let Some(&exclusive) = s.exclusive.get(&id)
                && (exclusive > total.allocated || !flags.contains(NodeFlags::SHARED))
            {
                return Err(bad(id, "exclusive bytes exceed the allocated bytes"));
            }
            if flags.contains(NodeFlags::SHARED) && !s.exclusive.contains_key(&id) {
                return Err(bad(id, "shared entry has no exclusive figure"));
            }
            let del = s.delete_totals_of(id);
            for part in [del.in_bin, del.deleted] {
                if part.logical > total.logical
                    || part.allocated > total.allocated
                    || part.files > total.files
                {
                    return Err(bad(id, "deleted bytes exceed the counted bytes"));
                }
            }
            if del.in_bin.checked_add(del.deleted).is_none_or(|d| {
                d.logical > total.logical || d.allocated > total.allocated || d.files > total.files
            }) {
                return Err(bad(id, "deleted bytes exceed the counted bytes"));
            }
        }
        Ok(())
    }

    /// Bytes of name data stored (including each name's length prefix).
    pub fn name_bytes(&self) -> usize {
        self.store.pool_used_bytes()
    }

    /// What deleting `id` and everything below it would free, counting the
    /// bytes still on disk. `None` for unknown ids.
    ///
    /// `at_most` is the node's allocated size less what is already in the bin
    /// or deleted. `at_least` also leaves out:
    ///
    /// - for every shared file below (and the node itself), the part other
    ///   files use as well, as the scanner recorded it. Files that share data
    ///   with each other inside the deleted subtree are treated as sharing
    ///   with outsiders, so `at_least` can be short;
    /// - every hard-linked file below, whole. The tree does not know a file's
    ///   link count, so it cannot tell that all its names lie inside the
    ///   deleted subtree; another name keeps the data. Files whose names are
    ///   all inside are therefore left out too, and `at_least` is short there.
    ///
    /// A file already in the bin or deleted is skipped by both rules, as its
    /// bytes are not in `at_most` any more. `at_least` is never long. Without
    /// any shared or hard-linked file the two figures are equal.
    ///
    /// The cost is one look at every shared or hard-linked file of the tree,
    /// however small the node asked about, so ask when a figure is shown (a
    /// hover card, a confirmation), not for every box of a layout.
    pub fn delete_would_free(&self, id: NodeId) -> Option<FreeEstimate> {
        let node = self.node(id)?;
        let at_most = node.current_allocated();
        let mut kept_away = 0u64;
        for &sid in self.store.linked.keys() {
            if !self.is_at_or_below(sid, id.0) {
                continue;
            }
            let Some(s) = self.node(NodeId(sid)) else {
                continue;
            };
            if s.effective_delete_state() != DeleteState::None {
                continue;
            }
            kept_away = kept_away.saturating_add(self.store.allocated(sid));
        }
        for (&sid, &exclusive) in self.store.exclusive.iter() {
            // A hard-linked file is already out whole.
            if self.store.linked.contains_key(&sid) || !self.is_at_or_below(sid, id.0) {
                continue;
            }
            let Some(s) = self.node(NodeId(sid)) else {
                continue;
            };
            if s.effective_delete_state() != DeleteState::None {
                continue;
            }
            kept_away =
                kept_away.saturating_add(self.store.allocated(sid).saturating_sub(exclusive));
        }
        Some(FreeEstimate {
            at_least: at_most.saturating_sub(kept_away),
            at_most,
        })
    }

    /// Whether `node` is `ancestor` or lies below it.
    fn is_at_or_below(&self, node: u32, ancestor: u32) -> bool {
        let mut at = node;
        // Parents are created before their children, so ids only go down.
        while at != NONE {
            if at == ancestor {
                return true;
            }
            if at < ancestor {
                return false;
            }
            at = self.store.parent(at).unwrap_or(NONE);
        }
        false
    }
}

/// A node in a snapshot. Cheap to copy; borrows the snapshot.
#[derive(Clone, Copy)]
pub struct NodeRef<'a> {
    store: &'a Store,
    id: NodeId,
}

impl<'a> NodeRef<'a> {
    /// The node's id.
    pub fn id(&self) -> NodeId {
        self.id
    }

    /// The name as raw OS bytes (UTF-8-ish on Linux, WTF-8 for Windows
    /// names). The root's name is the scanned path.
    pub fn name(&self) -> &'a [u8] {
        self.store.name(self.id.0)
    }

    /// File, directory, link or other.
    pub fn kind(&self) -> NodeKind {
        self.store.kind(self.id.0)
    }

    /// The containing directory; `None` for the root.
    pub fn parent(&self) -> Option<NodeRef<'a>> {
        let p = self.store.parent(self.id.0)?;
        (p != NONE).then_some(NodeRef {
            store: self.store,
            id: NodeId(p),
        })
    }

    /// The children, most recently added first. Empty for non-directories.
    pub fn children(&self) -> Children<'a> {
        Children {
            store: self.store,
            next: self.store.first_child(self.id.0).unwrap_or(NONE),
        }
    }

    /// Logical (apparent) size in bytes, counted as scanned: for a
    /// directory, its own entry plus everything below. Deleting does not
    /// change it; see [`NodeRef::current_logical`].
    pub fn logical(&self) -> u64 {
        self.store.logical(self.id.0)
    }

    /// Allocated size (size on disk) in bytes, counted as scanned. This is
    /// the size layouts are drawn from and it never shrinks on delete.
    pub fn allocated(&self) -> u64 {
        self.store.allocated(self.id.0)
    }

    /// Non-directory entries at and below this node (1 for a file). Every
    /// name of a hard-linked file counts, though only the first carries
    /// bytes.
    pub fn file_count(&self) -> u32 {
        self.store.files(self.id.0)
    }

    /// Modification time in whole seconds since the Unix epoch (sub-second
    /// precision is not kept).
    pub fn mtime(&self) -> i64 {
        self.store.mtime(self.id.0)
    }

    /// Category assigned by the scanner.
    pub fn category(&self) -> Category {
        Category(self.store.category(self.id.0))
    }

    /// All flags.
    pub fn flags(&self) -> NodeFlags {
        self.store.flags(self.id.0)
    }

    /// Everything at and below this node has been counted.
    pub fn is_complete(&self) -> bool {
        self.flags().contains(NodeFlags::COMPLETE)
    }

    /// Still being counted: show "≥" and "Scanning…", and keep actions off.
    pub fn is_in_progress(&self) -> bool {
        !self.is_complete()
    }

    /// This entry itself could not be read.
    pub fn is_unreadable(&self) -> bool {
        self.flags().contains(NodeFlags::UNREADABLE)
    }

    /// Something below this node could not be read.
    pub fn has_unreadable_below(&self) -> bool {
        self.flags().contains(NodeFlags::UNREADABLE_BELOW)
    }

    /// Records for entries at or below this directory were refused or lost.
    pub fn has_lost_records_below(&self) -> bool {
        self.flags().contains(NodeFlags::RECORDS_LOST_BELOW)
    }

    /// Whether the sizes are only a lower bound: still counting, part of the
    /// subtree could not be read, or records for it were refused or lost.
    /// This is the condition for showing "≥". A complete directory can still
    /// be a lower bound.
    pub fn bytes_are_lower_bound(&self) -> bool {
        self.is_in_progress()
            || self.flags().intersects(
                NodeFlags::UNREADABLE | NodeFlags::UNREADABLE_BELOW | NodeFlags::RECORDS_LOST_BELOW,
            )
    }

    /// Whether the allocated size of this entry, or of something at or below
    /// it, is an estimate rather than a figure the filesystem gave (see
    /// [`NodeFlags::ESTIMATED_SIZE`]). Show it with "≈" and never as a
    /// measured number. Logical sizes are not affected.
    pub fn size_is_estimated(&self) -> bool {
        self.flags().contains(NodeFlags::ESTIMATED_SIZE)
    }

    /// Why the entry could not be read, if it could not.
    pub fn read_failure(&self) -> Option<ReadFailure> {
        self.store.failures.get(&self.id.0).copied()
    }

    /// The entry shares data with other files (reflinks, snapshots). Deleting
    /// it frees only [`NodeRef::exclusive_allocated`] of its bytes.
    pub fn is_shared(&self) -> bool {
        self.flags().contains(NodeFlags::SHARED)
    }

    /// For a shared entry: the allocated bytes only it uses, which deleting
    /// it frees. `None` for entries the scanner did not report as shared.
    pub fn exclusive_allocated(&self) -> Option<u64> {
        self.is_shared()
            .then(|| self.store.exclusive.get(&self.id.0).copied().unwrap_or(0))
    }

    /// The reader made this entry up (see [`NodeFlags::SYNTHETIC`]): it is not
    /// a real file or folder, and nothing may be done to it by name.
    pub fn is_synthetic(&self) -> bool {
        !self.store.synthetic.is_empty() && self.store.synthetic.contains_key(&self.id.0)
    }

    /// This entry, or a folder above it, was made up by the reader. Paths
    /// through it are not real, so every action on it, delete included, must
    /// be refused.
    pub fn is_under_synthetic(&self) -> bool {
        if self.store.synthetic.is_empty() {
            return false;
        }
        let mut at = Some(*self);
        while let Some(node) = at {
            if node.is_synthetic() {
                return true;
            }
            at = node.parent();
        }
        false
    }

    /// The file has more than one name.
    pub fn is_hard_link(&self) -> bool {
        self.flags().contains(NodeFlags::HARD_LINK)
    }

    /// For a later name of a hard-linked file: the node where its bytes are
    /// counted. `None` for the counted name and for other files.
    pub fn hard_link_origin(&self) -> Option<NodeId> {
        self.store.link_origin.get(&self.id.0).map(|&n| NodeId(n))
    }

    /// This node's own delete mark.
    pub fn delete_state(&self) -> DeleteState {
        self.store.delete_state(self.id.0)
    }

    /// The delete state in effect: this node's mark, else the nearest marked
    /// ancestor's.
    pub fn effective_delete_state(&self) -> DeleteState {
        let mut at = Some(*self);
        while let Some(node) = at {
            let state = node.delete_state();
            if state != DeleteState::None {
                return state;
            }
            at = node.parent();
        }
        DeleteState::None
    }

    /// Bytes at and below this node marked moved-to-bin or deleted.
    pub fn delete_totals(&self) -> DeleteTotals {
        self.store.delete_totals_of(self.id.0)
    }

    /// Allocated bytes still on disk: counted minus bin and deleted.
    pub fn current_allocated(&self) -> u64 {
        let d = self.delete_totals();
        self.allocated()
            .saturating_sub(d.in_bin.allocated)
            .saturating_sub(d.deleted.allocated)
    }

    /// Logical bytes still on disk: counted minus bin and deleted.
    pub fn current_logical(&self) -> u64 {
        let d = self.delete_totals();
        self.logical()
            .saturating_sub(d.in_bin.logical)
            .saturating_sub(d.deleted.logical)
    }

    /// Files still on disk: counted minus bin and deleted.
    pub fn current_files(&self) -> u64 {
        let d = self.delete_totals();
        u64::from(self.file_count())
            .saturating_sub(d.in_bin.files)
            .saturating_sub(d.deleted.files)
    }
}

/// Iterator over a node's children; see [`NodeRef::children`].
pub struct Children<'a> {
    store: &'a Store,
    next: u32,
}

impl<'a> Iterator for Children<'a> {
    type Item = NodeRef<'a>;

    fn next(&mut self) -> Option<NodeRef<'a>> {
        if self.next == NONE {
            return None;
        }
        let id = self.next;
        self.next = self.store.next_sibling(id).unwrap_or(NONE);
        Some(NodeRef {
            store: self.store,
            id: NodeId(id),
        })
    }
}
