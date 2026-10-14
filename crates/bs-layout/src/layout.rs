//! The layout itself: a pure function from a snapshot and a view to a flat
//! list of boxes, and the operations that keep a stored layout current
//! without moving anything.

use std::sync::Arc;

use bs_tree::{DeleteState, NodeFlags, NodeId, NodeKind, NodeRef, Snapshot};

use crate::format::{display_name, format_count, format_size, write_size};
use crate::geom::{Metrics, PxRect, Rect, Thresholds, px_content};
use crate::item::{CATEGORY_COUNT, CategoryId, DrawItem, ItemKind, Label, StateFlags};
use crate::squarify::{Edges, squarify};
use crate::view::{GroupSpec, ViewTarget};

/// Folders nest no deeper than this below the view's own box; deeper ones
/// are drawn as blocks. It keeps the `depth` field exact.
const MAX_NEST_DEPTH: u8 = 250;

/// Fewest siblings that make a group.
const MIN_GROUP_SIZE: usize = 3;

/// Knobs that are not about the look.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Options {
    /// The most items one layout may hold. Past it the remaining boxes are
    /// left out and [`Layout::is_truncated`] says so. It only matters when
    /// a view is full of equally tiny boxes that can't be grouped.
    pub max_items: usize,
    /// What texts call the place deleted files go: the Recycle Bin on
    /// Windows, the Trash elsewhere.
    pub bin: BinName,
}

impl Default for Options {
    fn default() -> Options {
        Options {
            max_items: 200_000,
            bin: BinName::platform(),
        }
    }
}

/// The name of the place "moved to" files go, as the operating system calls
/// it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinName {
    /// "Recycle Bin", on Windows.
    RecycleBin,
    /// "Trash", on Linux.
    Trash,
}

impl BinName {
    /// The name the current platform uses.
    pub const fn platform() -> BinName {
        if cfg!(windows) {
            BinName::RecycleBin
        } else {
            BinName::Trash
        }
    }

    /// The name as written in texts.
    pub const fn as_str(self) -> &'static str {
        match self {
            BinName::RecycleBin => "Recycle Bin",
            BinName::Trash => "Trash",
        }
    }
}

/// What an item id stands for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ItemRef<'a> {
    /// A node of the tree.
    Node(NodeId),
    /// A group of small siblings.
    Group(&'a Arc<GroupSpec>),
}

/// The boxes of one view, laid out once and kept as they are.
#[derive(Clone, Debug)]
pub struct Layout {
    target: ViewTarget,
    viewport: Rect,
    metrics: Metrics,
    options: Options,
    epoch: u64,
    node_count: u32,
    items: Vec<DrawItem>,
    enterable: Vec<(u32, ViewTarget)>,
    groups: Vec<Arc<GroupSpec>>,
    truncated: bool,
}

/// Lays out `target` to fill `viewport` (device pixels).
///
/// The result depends only on the arguments: the same snapshot, view,
/// viewport and metrics always give the same list, in the same order.
///
/// * Parents come before their children.
/// * Children are placed by allocated size, largest first, ties by node id.
///   Sizes are the counted ones, so a delete never changes a box.
/// * Boxes have whole-pixel edges and stay inside their parent's content
///   area ([`crate::content_rect`]).
/// * Nothing narrower or shorter than 2 px is drawn. Its bytes still belong
///   to its parent, whose other boxes keep their places.
/// * Siblings whose share of the parent's content area is under a square of
///   2.5 em on a side are merged into one group box when there are at least
///   three of them. Entering the group lays out its members.
pub fn layout(
    snapshot: &Snapshot,
    target: &ViewTarget,
    viewport: Rect,
    metrics: Metrics,
) -> Layout {
    layout_with(snapshot, target, viewport, metrics, Options::default())
}

/// Like [`layout`], with explicit [`Options`].
pub fn layout_with(
    snapshot: &Snapshot,
    target: &ViewTarget,
    viewport: Rect,
    metrics: Metrics,
    options: Options,
) -> Layout {
    let metrics = metrics.sanitized();
    let mut layout = Layout {
        target: target.clone(),
        viewport,
        metrics,
        options,
        epoch: snapshot.epoch(),
        node_count: snapshot.len(),
        items: Vec::new(),
        enterable: Vec::new(),
        groups: Vec::new(),
        truncated: false,
    };
    let Some(root_rect) = PxRect::from_viewport(viewport) else {
        return layout;
    };
    if let ViewTarget::Group(spec) = target {
        layout.groups.push(Arc::clone(spec));
    }
    let mut b = Builder {
        snap: snapshot,
        m: metrics,
        th: Thresholds::new(metrics),
        out: layout,
        stack: Vec::new(),
        cands: Vec::new(),
        small: Vec::new(),
        areas: Vec::new(),
        edges: Vec::new(),
        scratch: String::new(),
        fast: if matches!(target, ViewTarget::Group(_)) {
            vec![None]
        } else {
            Vec::new()
        },
    };
    b.run(root_rect);
    b.out
}

impl Layout {
    /// The view this layout is of.
    pub fn target(&self) -> &ViewTarget {
        &self.target
    }

    /// The rectangle it was laid out for, as given.
    pub fn viewport(&self) -> Rect {
        self.viewport
    }

    /// The font and scale it was laid out for.
    pub fn metrics(&self) -> Metrics {
        self.metrics
    }

    /// Epoch of the snapshot the geometry came from.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// The boxes, parents before children, siblings largest first.
    pub fn items(&self) -> &[DrawItem] {
        &self.items
    }

    /// The boxes that can be entered: folders, and groups, as
    /// `(item id, view)`. Item order.
    pub fn enterable(&self) -> &[(u32, ViewTarget)] {
        &self.enterable
    }

    /// Whether boxes were left out because [`Options::max_items`] was
    /// reached.
    pub fn is_truncated(&self) -> bool {
        self.truncated
    }

    /// The item with this id (see [`DrawItem::id`]).
    pub fn item(&self, id: u32) -> Option<&DrawItem> {
        self.items.iter().find(|i| i.id == id)
    }

    /// What an item id stands for.
    pub fn item_ref(&self, id: u32) -> Option<ItemRef<'_>> {
        if id < self.node_count {
            return Some(ItemRef::Node(NodeId::from_index(id)));
        }
        let index = usize::try_from(id - self.node_count).ok()?;
        self.groups.get(index).map(ItemRef::Group)
    }

    /// The item showing this node, if it is drawn. A node added to the tree
    /// after this layout was made (a scan is still running) has no item,
    /// even though its number may equal a group's id.
    pub fn item_for_node(&self, node: NodeId) -> Option<&DrawItem> {
        if node.index() >= self.node_count {
            return None;
        }
        self.item(node.index())
    }

    /// Lays the same view out again, for a new rectangle, font or scale, or
    /// to pick up nodes added since. This is the only way boxes move.
    pub fn relayout(&self, snapshot: &Snapshot, viewport: Rect, metrics: Metrics) -> Layout {
        layout_with(snapshot, &self.target, viewport, metrics, self.options)
    }

    /// Brings states and texts up to date with `snapshot` without moving or
    /// adding anything: delete marks, progress, "≥" and the sizes in
    /// labels. Rectangles, kinds, ids and the set of boxes stay exactly as
    /// they are, so undoing a delete restores the previous items exactly.
    ///
    /// Nodes the snapshot does not know are left untouched.
    pub fn refresh(&mut self, snapshot: &Snapshot) {
        let node_count = self.node_count;
        let groups = &self.groups;
        let mut scratch = String::new();
        let bin = self.options.bin;
        for item in &mut self.items {
            if item.id < node_count {
                let Some(node) = snapshot.node(NodeId::from_index(item.id)) else {
                    continue;
                };
                let eff = node.effective_delete_state();
                item.flags = item
                    .flags
                    .with(StateFlags::DELETED, eff != DeleteState::None)
                    .with(StateFlags::IN_PROGRESS, node.is_in_progress());
                if let Some(label) = &mut item.label {
                    scratch.clear();
                    write_node_size(&mut scratch, &node, eff, bin);
                    if *label.size != *scratch {
                        label.size = scratch.as_str().into();
                    }
                }
            } else {
                let Some(spec) = usize::try_from(item.id - node_count)
                    .ok()
                    .and_then(|i| groups.get(i))
                else {
                    continue;
                };
                let Some(info) = group_info(snapshot, spec) else {
                    continue;
                };
                item.flags = item
                    .flags
                    .with(StateFlags::DELETED, info.all_marked)
                    .with(StateFlags::IN_PROGRESS, info.in_progress);
                if let Some(label) = &mut item.label {
                    let size = info.size_text(bin);
                    if *label.size != *size {
                        label.size = size.into_boxed_str();
                    }
                }
            }
        }
        self.epoch = snapshot.epoch();
    }
}

/// A sibling waiting to be placed.
#[derive(Clone, Copy)]
struct Cand {
    size: u64,
    /// A node index, or a group index when `group` is set.
    id: u32,
    /// The node's flag bits (12 bits), a folder bit and its own delete
    /// state, packed; read once while gathering so that a group's summary
    /// costs nothing more.
    info: u16,
    /// Palette index, or `CategoryId::NEUTRAL` for folders.
    category: u8,
    group: bool,
}

const INFO_DIR: u16 = 1 << 12;
const INFO_DELETE_SHIFT: u16 = 13;

// The packing above needs the tree's flags to stay below `INFO_DIR`, and its
// delete states to be 0, 1 and 2 (two bits). If the tree grows another flag
// or state, this stops compiling instead of corrupting group summaries.
const _: () = {
    assert!(NodeFlags::from_bits_truncate(u16::MAX).bits() < INFO_DIR);
    assert!(DeleteState::None as u16 == 0);
    assert!(DeleteState::InBin as u16 == 1);
    assert!(DeleteState::Deleted as u16 == 2);
};

impl Cand {
    fn flags(self) -> NodeFlags {
        NodeFlags::from_bits_truncate(self.info)
    }

    fn is_dir(self) -> bool {
        self.info & INFO_DIR != 0
    }

    fn own_delete_state(self) -> DeleteState {
        match self.info >> INFO_DELETE_SHIFT {
            1 => DeleteState::InBin,
            2 => DeleteState::Deleted,
            _ => DeleteState::None,
        }
    }
}

/// A box waiting to be emitted.
#[derive(Clone, Copy)]
struct Pending {
    group: bool,
    id: u32,
    rect: PxRect,
    depth: u8,
    /// The delete state in effect above this box.
    inherited: DeleteState,
    view_root: bool,
}

struct Builder<'a> {
    snap: &'a Snapshot,
    m: Metrics,
    th: Thresholds,
    out: Layout,
    stack: Vec<Pending>,
    cands: Vec<Cand>,
    small: Vec<Cand>,
    areas: Vec<f64>,
    edges: Vec<Edges>,
    /// Summaries of the groups made while laying out, by group index; `None`
    /// for a group view's own group, which is summarized when emitted.
    fast: Vec<Option<GroupInfo>>,
    /// Text under construction, reused for every label.
    scratch: String,
}

impl Builder<'_> {
    fn run(&mut self, root_rect: PxRect) {
        let min_tile = self.th.min_tile as i32;
        if root_rect.w() < min_tile || root_rect.h() < min_tile {
            return;
        }
        let root = match &self.out.target {
            ViewTarget::Folder(id) => {
                let Some(node) = self.snap.node(*id) else {
                    return;
                };
                Pending {
                    group: false,
                    id: id.index(),
                    rect: root_rect,
                    depth: 0,
                    inherited: node
                        .parent()
                        .map_or(DeleteState::None, |p| p.effective_delete_state()),
                    view_root: true,
                }
            }
            ViewTarget::Group(spec) => Pending {
                group: true,
                id: self.out.node_count,
                rect: root_rect,
                depth: 0,
                inherited: self
                    .snap
                    .node(spec.folder)
                    .map_or(DeleteState::None, |n| n.effective_delete_state()),
                view_root: true,
            },
        };
        self.stack.push(root);
        while let Some(p) = self.stack.pop() {
            if self.out.items.len() >= self.out.options.max_items {
                self.out.truncated = true;
                self.stack.clear();
                break;
            }
            if p.group {
                self.emit_group(p);
            } else {
                self.emit_node(p);
            }
        }
    }

    fn emit_node(&mut self, p: Pending) {
        let Some(node) = self.snap.node(NodeId::from_index(p.id)) else {
            return;
        };
        let (w, h) = (p.rect.w() as f32, p.rect.h() as f32);
        let is_dir = node.kind() == NodeKind::Dir;
        let nest = is_dir && w >= self.th.nest_w && h >= self.th.nest_h && p.depth < MAX_NEST_DEPTH;
        let kind = match (is_dir, nest) {
            (true, true) => ItemKind::FolderTitled,
            (true, false) => ItemKind::FolderBlock,
            (false, _) => ItemKind::FileTile,
        };
        let eff = match node.delete_state() {
            DeleteState::None => p.inherited,
            own => own,
        };
        let labeled = nest || (w >= self.th.label_w && h >= self.th.label_h);
        let label = labeled.then(|| {
            self.scratch.clear();
            write_node_size(&mut self.scratch, &node, eff, self.out.options.bin);
            Box::new(Label {
                name: display_name(node.name()),
                size: self.scratch.as_str().into(),
            })
        });
        let category = if is_dir {
            CategoryId::NEUTRAL
        } else {
            CategoryId::from_tree(node.category())
        };
        self.out.items.push(DrawItem {
            rect: p.rect.to_rect(),
            kind,
            depth: p.depth,
            label,
            id: p.id,
            category,
            flags: StateFlags::NONE
                .with(StateFlags::DELETED, eff != DeleteState::None)
                .with(StateFlags::IN_PROGRESS, node.is_in_progress()),
            bytes: node.allocated(),
        });
        if is_dir && !p.view_root {
            self.out
                .enterable
                .push((p.id, ViewTarget::Folder(NodeId::from_index(p.id))));
        }
        if nest && let Some(content) = px_content(p.rect, self.m) {
            let folder = NodeId::from_index(p.id);
            self.expand(Source::Folder(folder), folder, content, p.depth, eff, false);
        }
    }

    fn emit_group(&mut self, p: Pending) {
        let index = (p.id - self.out.node_count) as usize;
        let Some(spec) = self.out.groups.get(index).cloned() else {
            return;
        };
        let (w, h) = (p.rect.w() as f32, p.rect.h() as f32);
        let nest = p.view_root && w >= self.th.nest_w && h >= self.th.nest_h;
        let labeled = nest || (w >= self.th.label_w && h >= self.th.label_h);
        // The summary made while the group was formed covers everything but
        // the amounts for the label, which are only worked out when a label
        // is shown.
        let fast = self.fast.get(index).cloned().flatten();
        let info = match fast {
            Some(info) if !labeled => info,
            _ => match group_info(self.snap, &spec) {
                Some(info) => info,
                None => return,
            },
        };
        let kind = if nest {
            ItemKind::FolderTitled
        } else {
            ItemKind::Group
        };
        let label = labeled.then(|| {
            Box::new(Label {
                name: info.name_text().into_boxed_str(),
                size: info.size_text(self.out.options.bin).into_boxed_str(),
            })
        });
        self.out.items.push(DrawItem {
            rect: p.rect.to_rect(),
            kind,
            depth: p.depth,
            label,
            id: p.id,
            category: if nest {
                CategoryId::NEUTRAL
            } else {
                info.dominant
            },
            flags: StateFlags::NONE
                .with(StateFlags::DELETED, info.all_marked)
                .with(StateFlags::IN_PROGRESS, info.in_progress),
            bytes: info.counted,
        });
        if !p.view_root {
            self.out
                .enterable
                .push((p.id, ViewTarget::Group(spec.clone())));
        }
        if nest && let Some(content) = px_content(p.rect, self.m) {
            self.expand(
                Source::Members(spec.clone()),
                spec.folder,
                content,
                p.depth,
                p.inherited,
                true,
            );
        }
    }

    /// Places the children of a nested box inside `content` and queues them,
    /// first child on top of the stack so the list comes out in order.
    fn expand(
        &mut self,
        source: Source,
        folder: NodeId,
        content: PxRect,
        depth: u8,
        inherited: DeleteState,
        group_view_root: bool,
    ) {
        let mut cands = std::mem::take(&mut self.cands);
        let mut small = std::mem::take(&mut self.small);
        let mut areas = std::mem::take(&mut self.areas);
        let mut edges = std::mem::take(&mut self.edges);
        cands.clear();
        small.clear();
        areas.clear();
        edges.clear();

        let mut total = 0.0f64;
        let mut push = |node: NodeRef<'_>| {
            let size = node.allocated();
            if size > 0 {
                let is_dir = node.kind() == NodeKind::Dir;
                let mut info = node.flags().bits();
                if is_dir {
                    info |= INFO_DIR;
                }
                info |= (node.delete_state() as u16) << INFO_DELETE_SHIFT;
                cands.push(Cand {
                    size,
                    id: node.id().index(),
                    info,
                    category: if is_dir {
                        CategoryId::NEUTRAL.0
                    } else {
                        CategoryId::from_tree(node.category()).0
                    },
                    group: false,
                });
                total += size as f64;
            }
        };
        match &source {
            Source::Folder(id) => {
                if let Some(node) = self.snap.node(*id) {
                    node.children().for_each(&mut push);
                }
            }
            Source::Members(spec) => {
                spec.members
                    .iter()
                    .filter_map(|&id| self.snap.node(id))
                    .for_each(&mut push);
            }
        }

        if !cands.is_empty() {
            let cw = f64::from(content.w());
            let ch = f64::from(content.h());
            let area = cw * ch;
            let limit = f64::from(self.th.group_side);
            let small_area = limit * limit;
            // A sibling is small when its share of the content area is under
            // the group square: size / total * area < small_area.
            let mut keep = 0;
            for i in 0..cands.len() {
                let c = cands[i];
                if (c.size as f64) * area < small_area * total {
                    small.push(c);
                } else {
                    cands[keep] = c;
                    keep += 1;
                }
            }
            cands.truncate(keep);
            // Grouping everything in a group's own view would show the same
            // group again, forever; show the members themselves instead.
            let group_ok = small.len() >= MIN_GROUP_SIZE && !(group_view_root && cands.is_empty());
            if group_ok && let Some(gid) = self.next_group_id() {
                let members: Vec<NodeId> = small.iter().map(|c| NodeId::from_index(c.id)).collect();
                let sum = small.iter().fold(0u64, |acc, c| acc.saturating_add(c.size));
                self.out
                    .groups
                    .push(Arc::new(GroupSpec { folder, members }));
                self.fast.push(Some(summarize(&small, inherited)));
                cands.push(Cand {
                    size: sum,
                    id: gid,
                    info: 0,
                    category: CategoryId::NEUTRAL.0,
                    group: true,
                });
            } else {
                cands.extend_from_slice(&small);
            }
            cands.sort_unstable_by(|a, b| {
                b.size
                    .cmp(&a.size)
                    .then(a.group.cmp(&b.group))
                    .then(a.id.cmp(&b.id))
            });
            areas.extend(cands.iter().map(|c| c.size as f64 / total * area));
            squarify(
                &areas,
                f64::from(content.x0),
                f64::from(content.y0),
                f64::from(content.x1),
                f64::from(content.y1),
                &mut edges,
            );
            let min_tile = self.th.min_tile as i32;
            let child_depth = depth.saturating_add(1);
            for (c, e) in cands.iter().zip(&edges).rev() {
                let rect = PxRect {
                    x0: snap_edge(e[0], content.x0, content.x1),
                    y0: snap_edge(e[1], content.y0, content.y1),
                    x1: snap_edge(e[2], content.x0, content.x1),
                    y1: snap_edge(e[3], content.y0, content.y1),
                };
                if rect.w() < min_tile || rect.h() < min_tile {
                    continue;
                }
                self.stack.push(Pending {
                    group: c.group,
                    id: c.id,
                    rect,
                    depth: child_depth,
                    inherited,
                    view_root: false,
                });
            }
        }

        self.cands = cands;
        self.small = small;
        self.areas = areas;
        self.edges = edges;
    }

    /// The id for the next group, or `None` if the id space is used up (a
    /// tree of nearly 2^32 nodes), in which case nothing is grouped.
    fn next_group_id(&self) -> Option<u32> {
        let index = u32::try_from(self.out.groups.len()).ok()?;
        self.out.node_count.checked_add(index)
    }
}

enum Source {
    Folder(NodeId),
    Members(Arc<GroupSpec>),
}

fn snap_edge(v: f64, lo: i32, hi: i32) -> i32 {
    // `as` saturates and maps NaN to 0, and the clamp keeps the edge inside
    // the parent whatever the arithmetic did.
    (v.round() as i32).clamp(lo, hi)
}

/// What goes before a size: "≥" when it is a lower bound, "≈" when it
/// includes a figure the filesystem did not give (an estimate), both when
/// both hold.
fn size_mark(lower_bound: bool, estimated: bool) -> &'static str {
    match (lower_bound, estimated) {
        (false, false) => "",
        (true, false) => "≥ ",
        (false, true) => "≈ ",
        (true, true) => "≥ ≈ ",
    }
}

/// Writes the size text of a node into `out`: the bytes still on disk, "≥"
/// when they are only a lower bound, and what was moved to the bin
/// or deleted. Writing into a caller's buffer keeps labels to one
/// allocation each.
fn write_node_size(out: &mut String, node: &NodeRef<'_>, eff: DeleteState, bin: BinName) {
    let mark = size_mark(node.bytes_are_lower_bound(), node.size_is_estimated());
    match eff {
        DeleteState::InBin => {
            out.push_str("Moved to ");
            out.push_str(bin.as_str());
            out.push_str(" · ");
            out.push_str(mark);
            write_size(out, node.allocated());
        }
        DeleteState::Deleted => {
            out.push_str("Deleted · ");
            out.push_str(mark);
            write_size(out, node.allocated());
        }
        DeleteState::None => {
            out.push_str(mark);
            write_size(out, node.current_allocated());
            let totals = node.delete_totals();
            let moved_mark = if totals.is_lower_bound() { "≥ " } else { "" };
            if totals.in_bin.allocated > 0 || totals.in_bin.files > 0 {
                out.push_str(" · ");
                out.push_str(moved_mark);
                write_size(out, totals.in_bin.allocated);
                out.push_str(" moved to ");
                out.push_str(bin.as_str());
            }
            if totals.deleted.allocated > 0 || totals.deleted.files > 0 {
                out.push_str(" · ");
                out.push_str(moved_mark);
                write_size(out, totals.deleted.allocated);
                out.push_str(" deleted");
            }
        }
    }
}

/// Summarizes the members of a new group from what gathering already read.
/// The label amounts stay zero; see `emit_group`.
fn summarize(members: &[Cand], inherited: DeleteState) -> GroupInfo {
    let mut info = GroupInfo {
        members: members.len(),
        has_folder: false,
        counted: 0,
        current: 0,
        in_bin: 0,
        deleted: 0,
        lower_bound: false,
        estimated: false,
        in_progress: false,
        all_marked: !members.is_empty(),
        dominant: CategoryId::NEUTRAL,
    };
    let mut by_category = [0u64; CATEGORY_COUNT as usize];
    for c in members {
        info.counted = info.counted.saturating_add(c.size);
        info.has_folder |= c.is_dir();
        let flags = c.flags();
        info.in_progress |= !flags.contains(NodeFlags::COMPLETE);
        info.estimated |= flags.contains(NodeFlags::ESTIMATED_SIZE);
        info.lower_bound |= !flags.contains(NodeFlags::COMPLETE)
            || flags.intersects(
                NodeFlags::UNREADABLE | NodeFlags::UNREADABLE_BELOW | NodeFlags::RECORDS_LOST_BELOW,
            );
        if c.own_delete_state() == DeleteState::None && inherited == DeleteState::None {
            info.all_marked = false;
        }
        let category = if c.is_dir() {
            CategoryId::OTHER.0
        } else {
            c.category
        };
        if let Some(slot) = by_category.get_mut(usize::from(category)) {
            *slot = slot.saturating_add(c.size);
        }
    }
    info.dominant = dominant_category(&by_category);
    info
}

/// A group takes its dominant category's color only when that category
/// holds more than 60% of the bytes.
fn dominant_category(by_category: &[u64; CATEGORY_COUNT as usize]) -> CategoryId {
    let total: u128 = by_category.iter().map(|&b| u128::from(b)).sum();
    match by_category
        .iter()
        .enumerate()
        .max_by_key(|&(i, &b)| (b, std::cmp::Reverse(i)))
    {
        Some((index, &best)) if u128::from(best) * 10 > total * 6 => CategoryId(index as u8),
        _ => CategoryId::NEUTRAL,
    }
}

/// Facts about a group's members, read from a snapshot.
#[derive(Clone)]
struct GroupInfo {
    members: usize,
    has_folder: bool,
    counted: u64,
    current: u64,
    in_bin: u64,
    deleted: u64,
    lower_bound: bool,
    estimated: bool,
    in_progress: bool,
    all_marked: bool,
    dominant: CategoryId,
}

fn group_info(snap: &Snapshot, spec: &GroupSpec) -> Option<GroupInfo> {
    let inherited = snap.node(spec.folder)?.effective_delete_state();
    let mut info = GroupInfo {
        members: spec.members.len(),
        has_folder: false,
        counted: 0,
        current: 0,
        in_bin: 0,
        deleted: 0,
        lower_bound: false,
        estimated: false,
        in_progress: false,
        all_marked: !spec.members.is_empty(),
        dominant: CategoryId::NEUTRAL,
    };
    let mut by_category = [0u64; CATEGORY_COUNT as usize];
    for &id in &spec.members {
        let Some(node) = snap.node(id) else {
            continue;
        };
        let allocated = node.allocated();
        info.counted = info.counted.saturating_add(allocated);
        let is_dir = node.kind() == NodeKind::Dir;
        info.has_folder |= is_dir;
        info.lower_bound |= node.bytes_are_lower_bound();
        info.estimated |= node.size_is_estimated();
        info.in_progress |= node.is_in_progress();
        let own = match node.delete_state() {
            DeleteState::None => inherited,
            own => own,
        };
        match own {
            DeleteState::InBin => info.in_bin = info.in_bin.saturating_add(allocated),
            DeleteState::Deleted => info.deleted = info.deleted.saturating_add(allocated),
            DeleteState::None => {
                info.all_marked = false;
                let totals = node.delete_totals();
                info.current = info.current.saturating_add(node.current_allocated());
                info.in_bin = info.in_bin.saturating_add(totals.in_bin.allocated);
                info.deleted = info.deleted.saturating_add(totals.deleted.allocated);
            }
        }
        let category = if is_dir {
            CategoryId::OTHER
        } else {
            CategoryId::from_tree(node.category())
        };
        if let Some(slot) = by_category.get_mut(usize::from(category.0)) {
            *slot = slot.saturating_add(allocated);
        }
    }
    info.dominant = dominant_category(&by_category);
    Some(info)
}

impl GroupInfo {
    fn name_text(&self) -> String {
        let what = if self.has_folder { "items" } else { "files" };
        format!("{} small {what}", format_count(self.members as u64))
    }

    fn size_text(&self, bin: BinName) -> String {
        let bin = bin.as_str();
        let mark = size_mark(self.lower_bound, self.estimated);
        if self.all_marked {
            return match (self.in_bin > 0, self.deleted > 0) {
                (true, true) => format!(
                    "{mark}{} moved to {bin} · {mark}{} deleted",
                    format_size(self.in_bin),
                    format_size(self.deleted)
                ),
                (false, true) => format!("Deleted · {mark}{}", format_size(self.deleted)),
                _ => format!("Moved to {bin} · {mark}{}", format_size(self.in_bin)),
            };
        }
        let mut text = format!("{mark}{}", format_size(self.current));
        if self.in_bin > 0 {
            text.push_str(&format!(
                " · {mark}{} moved to {bin}",
                format_size(self.in_bin)
            ));
        }
        if self.deleted > 0 {
            text.push_str(&format!(" · {mark}{} deleted", format_size(self.deleted)));
        }
        text
    }
}
