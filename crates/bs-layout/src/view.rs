//! What a layout shows, and a table that gives views numeric ids.

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use bs_tree::{NodeId, Snapshot};

/// The contents of a synthetic group of small siblings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GroupSpec {
    /// The folder the members were taken from.
    pub folder: NodeId,
    /// The members, in a fixed order. Zooming into the group lays them out.
    pub members: Vec<NodeId>,
}

impl Hash for GroupSpec {
    /// Hashes the folder, the member count and a few members, not all of
    /// them: equal groups still hash alike, and looking a group up stays
    /// cheap even for a million members.
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.folder.hash(state);
        self.members.len().hash(state);
        for at in [
            0,
            self.members.len() / 2,
            self.members.len().wrapping_sub(1),
        ] {
            self.members.get(at).hash(state);
        }
    }
}

/// A thing a layout can be made for.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ViewTarget {
    /// A folder and everything below it.
    Folder(NodeId),
    /// The members of a small-file group.
    Group(Arc<GroupSpec>),
}

impl ViewTarget {
    /// The folder this view belongs to: the folder itself, or the one a
    /// group was taken from.
    pub fn folder(&self) -> NodeId {
        match self {
            ViewTarget::Folder(id) => *id,
            ViewTarget::Group(spec) => spec.folder,
        }
    }
}

/// Numeric view identifier, as the UI's navigation uses.
pub type ViewId = u64;

struct Entry {
    target: ViewTarget,
    parent: Option<ViewId>,
    /// Entered, so history may refer to it. A pinned view's ancestors are
    /// pinned too.
    pinned: bool,
}

/// Gives views numbers and remembers which view each was entered from.
///
/// A layout offers many boxes that could be entered, and a scan produces a
/// new layout every few hundred milliseconds, each with new groups. The
/// table therefore holds on only to what can still be reached:
///
/// * Views registered from a layout ([`ViewTable::register`]) stay until
///   the same parent is registered again, which replaces them with the new
///   layout's.
/// * Views that were entered ([`ViewTable::enter`], [`ViewTable::child`])
///   are pinned, with their ancestors, until [`ViewTable::prune`] says
///   history no longer reaches them.
///
/// Ids are never reused. A view's parent is the view it was first entered
/// from; asking for the same target again from elsewhere returns the same
/// id and keeps the first parent.
#[derive(Default)]
pub struct ViewTable {
    entries: HashMap<ViewId, Entry>,
    ids: HashMap<ViewTarget, ViewId>,
    /// Views registered from a parent's latest layout.
    offered: HashMap<ViewId, Vec<ViewId>>,
    next: ViewId,
}

impl ViewTable {
    /// An empty table.
    pub fn new() -> ViewTable {
        ViewTable::default()
    }

    /// Registers a view that has no parent (the top of the navigation). It
    /// is never dropped.
    pub fn root(&mut self, target: ViewTarget) -> ViewId {
        self.intern(None, &target, true)
    }

    /// Registers `target` as entered from `parent`, and pins it.
    pub fn child(&mut self, parent: ViewId, target: ViewTarget) -> ViewId {
        self.intern(Some(parent), &target, true)
    }

    /// Pins a view that was registered from a layout and returns its target:
    /// call it when the user enters the view, and lay the returned target
    /// out. `None` for an unknown id.
    pub fn enter(&mut self, view: ViewId) -> Option<ViewTarget> {
        let target = self.entries.get(&view)?.target.clone();
        self.pin(view);
        Some(target)
    }

    /// Registers every enterable item of `layout` as a child of `view`, and
    /// returns `(item id, view id)` pairs in item order. Views a previous
    /// layout of `view` offered and this one does not are dropped unless
    /// they were entered.
    pub fn register(&mut self, view: ViewId, layout: &crate::Layout) -> Vec<(u32, ViewId)> {
        let offered: Vec<(u32, ViewId)> = layout
            .enterable()
            .iter()
            .map(|(item, target)| (*item, self.intern(Some(view), target, false)))
            .collect();
        let fresh: HashSet<ViewId> = offered.iter().map(|&(_, id)| id).collect();
        let list: Vec<ViewId> = offered.iter().map(|&(_, id)| id).collect();
        for old in self.offered.remove(&view).unwrap_or_default() {
            if fresh.contains(&old) {
                continue;
            }
            match self.entries.get(&old) {
                // Entered: kept by its pin, no longer offered.
                Some(e) if e.pinned => {}
                // Another parent owns it; just stop offering it here.
                Some(e) if e.parent != Some(view) => {}
                Some(_) => {
                    // Another view's latest layout may offer it too (a nested
                    // folder is a box in its grandparent's layout as well):
                    // that box must keep its view, so the view moves to that
                    // parent instead of being dropped.
                    let other = self
                        .offered
                        .iter()
                        .filter(|(parent, _)| **parent != view)
                        .filter(|(_, list)| list.contains(&old))
                        .map(|(parent, _)| *parent)
                        .min();
                    match (other, self.entries.get_mut(&old)) {
                        (Some(parent), Some(entry)) => entry.parent = Some(parent),
                        _ => self.remove(old),
                    }
                }
                None => {}
            }
        }
        self.offered.insert(view, list);
        offered
    }

    /// Drops everything history cannot reach: keeps the roots, the views in
    /// `keep` with their ancestors (pinned), and what the kept views last
    /// offered (unpinned). Call it with the current view and the views in
    /// the back and forward history.
    pub fn prune(&mut self, keep: &[ViewId]) {
        let mut live: HashSet<ViewId> = HashSet::new();
        let roots = self
            .entries
            .iter()
            .filter(|(_, e)| e.parent.is_none())
            .map(|(&id, _)| id)
            .collect::<Vec<_>>();
        for &start in keep.iter().chain(&roots) {
            let mut at = Some(start);
            while let Some(id) = at {
                if !self.entries.contains_key(&id) || !live.insert(id) {
                    break;
                }
                at = self.entries.get(&id).and_then(|e| e.parent);
            }
        }
        let mut retained = live.clone();
        for id in &live {
            retained.extend(self.offered.get(id).into_iter().flatten().copied());
        }
        let gone: Vec<ViewId> = self
            .entries
            .keys()
            .filter(|id| !retained.contains(id))
            .copied()
            .collect();
        for id in gone {
            if let Some(entry) = self.entries.remove(&id) {
                self.ids.remove(&entry.target);
            }
        }
        for (id, entry) in &mut self.entries {
            entry.pinned = live.contains(id);
        }
        self.offered.retain(|id, _| live.contains(id));
        for list in self.offered.values_mut() {
            list.retain(|id| retained.contains(id));
        }
    }

    /// The target of a view id.
    pub fn target(&self, id: ViewId) -> Option<&ViewTarget> {
        self.entries.get(&id).map(|e| &e.target)
    }

    /// The view `id` was entered from; `None` for roots and unknown ids.
    pub fn parent(&self, id: ViewId) -> Option<ViewId> {
        self.entries.get(&id)?.parent
    }

    /// How many views the table holds.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the table holds no views.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// How many group members the table's views hold in total, for
    /// measuring what the table costs.
    pub fn held_member_ids(&self) -> usize {
        self.entries
            .values()
            .map(|e| match &e.target {
                ViewTarget::Group(spec) => spec.members.len(),
                ViewTarget::Folder(_) => 0,
            })
            .sum()
    }

    /// The target of the view above `id` in the folder tree, for views that
    /// were never entered through a layout (such as a path typed in).
    pub fn tree_parent(snapshot: &Snapshot, target: &ViewTarget) -> Option<ViewTarget> {
        let node = snapshot.node(target.folder())?;
        match target {
            ViewTarget::Group(_) => Some(ViewTarget::Folder(node.id())),
            ViewTarget::Folder(_) => node.parent().map(|p| ViewTarget::Folder(p.id())),
        }
    }

    fn intern(&mut self, parent: Option<ViewId>, target: &ViewTarget, pinned: bool) -> ViewId {
        if let Some(&id) = self.ids.get(target) {
            if pinned {
                self.pin(id);
            }
            return id;
        }
        let id = self.next;
        self.next += 1;
        self.ids.insert(target.clone(), id);
        self.entries.insert(
            id,
            Entry {
                target: target.clone(),
                parent,
                pinned: false,
            },
        );
        if pinned {
            self.pin(id);
        }
        id
    }

    /// Pins a view and its ancestors.
    fn pin(&mut self, id: ViewId) {
        let mut at = Some(id);
        while let Some(id) = at {
            match self.entries.get_mut(&id) {
                Some(e) if !e.pinned => {
                    e.pinned = true;
                    at = e.parent;
                }
                _ => break,
            }
        }
    }

    /// Removes an unpinned view and what it offered.
    fn remove(&mut self, id: ViewId) {
        let mut stack = vec![id];
        while let Some(id) = stack.pop() {
            let Some(entry) = self.entries.get(&id) else {
                continue;
            };
            if entry.pinned {
                continue;
            }
            if let Some(entry) = self.entries.remove(&id) {
                self.ids.remove(&entry.target);
            }
            for child in self.offered.remove(&id).unwrap_or_default() {
                if self
                    .entries
                    .get(&child)
                    .is_some_and(|e| e.parent == Some(id))
                {
                    stack.push(child);
                }
            }
        }
    }
}
