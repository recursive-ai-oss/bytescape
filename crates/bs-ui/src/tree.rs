//! The map's view source for a real size tree: lays views out with
//! `bs-layout` and hands the renderer its own item type.
//!
//! The adapter is mechanical. Every field of a layout box maps to a field of
//! [`bs_render::Item`]; the one piece of wording it adds is "Scanning…" in the
//! title bar of a folder that is still being counted, next to the "≥" the
//! layout already puts in front of its size.

use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use bs_knowledge::{Environment, Knowledge, Level};
use bs_layout::{
    DrawItem, ItemKind as LayoutKind, ItemRef, Metrics, ViewId, ViewTable, ViewTarget,
};
use bs_render::{Flags, Item, ItemKind, ItemLabel, Rect};
use bs_tree::{DeleteState, NodeId, NodeKind, Progress, Snapshot};

use crate::card::{CardData, DeleteMark};
use crate::{ItemInfo, Layout, Prepared, RelayoutJob, ViewSource};

/// How many recent layouts are kept so the info bar can describe their boxes.
const KEPT_LAYOUTS: usize = 8;

/// What the latest layout cost, for the host to throttle and report.
#[derive(Clone, Copy, Debug, Default)]
pub struct LayoutStats {
    /// Boxes in the layout.
    pub items: usize,
    /// Wall time of the layout call and the conversion.
    pub took: Duration,
}

/// Views over a snapshot of the size tree.
pub struct TreeSource {
    snapshot: Arc<Snapshot>,
    table: ViewTable,
    root: ViewId,
    metrics: Metrics,
    last: LayoutStats,
    /// The latest layouts, newest last. Hover asks them what a box is.
    laid: Vec<(ViewId, Arc<bs_layout::Layout>)>,
    /// The knowledge tables resolved for this user's folders, built when the
    /// first hover card asks.
    knowledge: OnceLock<Box<Knowledge>>,
}

/// A layout made on a worker thread, waiting to be adopted.
struct Done {
    laid: bs_layout::Layout,
    items: Vec<Item>,
    took: Duration,
}

impl TreeSource {
    /// A source for `snapshot`, or `None` while the snapshot has no root yet.
    pub fn new(snapshot: Arc<Snapshot>, metrics: Metrics) -> Option<TreeSource> {
        let root_node = snapshot.root()?.id();
        let mut table = ViewTable::new();
        let root = table.root(ViewTarget::Folder(root_node));
        Some(TreeSource {
            snapshot,
            table,
            root,
            metrics,
            last: LayoutStats::default(),
            laid: Vec::new(),
            knowledge: OnceLock::new(),
        })
    }

    /// Replaces the data. Views keep their ids; call the app's
    /// `DataChanged` event afterwards so the current view is laid out again.
    pub fn set_snapshot(&mut self, snapshot: Arc<Snapshot>) {
        self.snapshot = snapshot;
    }

    /// The snapshot views are laid out from.
    pub fn snapshot(&self) -> &Arc<Snapshot> {
        &self.snapshot
    }

    /// Changes the font size and scale used for thresholds. Lay views out
    /// again afterwards.
    pub fn set_metrics(&mut self, metrics: Metrics) {
        self.metrics = metrics;
    }

    /// Cost of the most recent layout.
    pub fn last_layout(&self) -> LayoutStats {
        self.last
    }

    fn remember(&mut self, view: ViewId, laid: Arc<bs_layout::Layout>) {
        self.laid.retain(|(v, _)| *v != view);
        self.laid.push((view, laid));
        if self.laid.len() > KEPT_LAYOUTS {
            self.laid.remove(0);
        }
    }

    /// The folder a view shows, or the folder a group view was taken from.
    pub fn view_folder(&self, view: ViewId) -> Option<NodeId> {
        self.table.target(view).map(ViewTarget::folder)
    }

    /// The breadcrumb text of a view: the scanned root, then each folder
    /// down to the view, joined with " › ". Names are shown lossily.
    pub fn breadcrumb(&self, view: ViewId) -> String {
        self.trail(view).join("  ›  ")
    }

    /// The folders from the scanned root down to the view, one name each,
    /// and a last entry for a group of small items.
    pub fn trail(&self, view: ViewId) -> Vec<String> {
        let Some(target) = self.table.target(view) else {
            return Vec::new();
        };
        let mut parts: Vec<String> = Vec::new();
        if let ViewTarget::Group(spec) = target {
            parts.push(format!(
                "{} small items",
                bs_layout::format_count(spec.members.len() as u64)
            ));
        }
        let mut at = self.snapshot.node(target.folder());
        while let Some(node) = at {
            parts.push(bs_layout::display_name(node.name()).into_string());
            at = node.parent();
        }
        parts.reverse();
        parts
    }

    /// The folders of the trail from the scanned root, as node ids.
    fn folder_chain(&self, folder: NodeId) -> Vec<NodeId> {
        let mut chain = Vec::new();
        let mut at = self.snapshot.node(folder);
        while let Some(node) = at {
            chain.push(node.id());
            at = node.parent();
        }
        chain.reverse();
        chain
    }

    fn knowledge(&self) -> &Knowledge {
        self.knowledge.get_or_init(|| {
            Box::new(Knowledge::new(&Environment::from_lookup(|n| {
                std::env::var_os(n)
            })))
        })
    }

    fn node_path(&self, node: NodeId) -> Option<Vec<u8>> {
        let sep = if cfg!(windows) { b'\\' } else { b'/' };
        self.snapshot.path(node, sep)
    }
}

impl ViewSource for TreeSource {
    fn root(&self) -> ViewId {
        self.root
    }

    fn layout(&mut self, view: ViewId, map: Rect) -> Layout {
        let started = Instant::now();
        let Some(target) = self.table.target(view).cloned() else {
            return Layout {
                items: Arc::new(Vec::new()),
                folders: Vec::new(),
            };
        };
        let area = bs_layout::Rect::new(map.x, map.y, map.w, map.h);
        let laid = bs_layout::layout(&self.snapshot, &target, area, self.metrics);
        // Offers are valid until this view is registered again, which is the
        // next layout of it.
        let folders = self.table.register(view, &laid);
        let items: Vec<Item> = laid.items().iter().map(to_item).collect();
        self.last = LayoutStats {
            items: items.len(),
            took: started.elapsed(),
        };
        self.remember(view, Arc::new(laid));
        Layout {
            items: Arc::new(items),
            folders,
        }
    }

    fn parent(&self, view: ViewId) -> Option<ViewId> {
        self.table.parent(view)
    }

    fn enter(&mut self, view: ViewId) {
        let _ = self.table.enter(view);
    }

    fn prune(&mut self, keep: &[ViewId]) {
        self.table.prune(keep);
        self.laid.retain(|(v, _)| keep.contains(v));
    }

    fn breadcrumb(&self, view: ViewId) -> String {
        TreeSource::breadcrumb(self, view)
    }

    fn trail(&self, view: ViewId) -> Vec<String> {
        TreeSource::trail(self, view)
    }

    fn open_trail_segment(&mut self, view: ViewId, index: usize) -> Option<ViewId> {
        let target = self.table.target(view)?.clone();
        let chain = self.folder_chain(target.folder());
        let node = *chain.get(index)?;
        if index == 0 {
            return Some(self.root);
        }
        if matches!(target, ViewTarget::Folder(f) if f == node) {
            return Some(view);
        }
        // Views for the folders between the top and this one, made as
        // needed; a folder that already has a view keeps it.
        let mut at = self.root;
        for &n in chain.iter().take(index + 1).skip(1) {
            at = self.table.child(at, ViewTarget::Folder(n));
        }
        Some(at)
    }

    fn path(&self, view: ViewId, id: u32) -> Option<String> {
        let laid = &self.laid.iter().find(|(v, _)| *v == view)?.1;
        let node = match laid.item_ref(id)? {
            ItemRef::Node(n) => n,
            ItemRef::Group(spec) => spec.folder,
        };
        let bytes = self.node_path(node)?;
        Some(bs_layout::display_name(&bytes).into_string())
    }

    fn card(&self, view: ViewId, id: u32) -> Option<CardData> {
        let laid = &self.laid.iter().find(|(v, _)| *v == view)?.1;
        let bytes = laid.item(id)?.bytes;
        let (node_id, group) = match laid.item_ref(id)? {
            ItemRef::Node(n) => (n, None),
            ItemRef::Group(spec) => (spec.folder, Some(spec.members.len() as u64)),
        };
        let node = self.snapshot.node(node_id)?;
        let raw = self.node_path(node_id)?;
        let path = bs_layout::display_name(&raw).into_string();
        if let Some(count) = group {
            // A group is not a place on disk: no note, no actions on it.
            return Some(CardData {
                name: format!("{} small items", bs_layout::format_count(count)),
                path,
                bytes,
                logical: None,
                parent_bytes: Some(node.allocated()),
                files: count,
                in_progress: node.is_in_progress(),
                estimated: false,
                is_folder: false,
                modified: None,
                category: None,
                note: None,
                handoff: None,
                hard_link: false,
                delete_state: DeleteMark::None,
                complete: node.is_complete(),
                blocked: false,
            });
        }
        let is_folder = node.kind() == NodeKind::Dir;
        let knowledge = self.knowledge();
        let entry = knowledge.recognize_bytes(&raw);
        let category = bs_knowledge::Category::from_number(node.category().0)
            .map(bs_knowledge::Category::label);
        Some(CardData {
            name: bs_layout::display_name(node.name()).into_string(),
            path,
            bytes,
            logical: Some(node.logical()),
            parent_bytes: node.parent().map(|p| p.allocated()),
            files: if is_folder {
                u64::from(node.file_count())
            } else {
                1
            },
            in_progress: node.is_in_progress(),
            estimated: node.size_is_estimated(),
            is_folder,
            modified: Some(node.mtime()).filter(|&t| t != 0),
            category,
            note: entry.map(|e| e.note),
            handoff: entry.and_then(|e| e.handoff),
            hard_link: node.is_hard_link(),
            delete_state: match node.effective_delete_state() {
                DeleteState::None => DeleteMark::None,
                DeleteState::InBin => DeleteMark::InBin,
                DeleteState::Deleted => DeleteMark::Deleted,
            },
            complete: node.is_complete(),
            blocked: knowledge.protection_bytes(&raw).level == Level::Blocked,
        })
    }

    fn describe(&self, view: ViewId, id: u32) -> Option<ItemInfo> {
        let laid = &self.laid.iter().find(|(v, _)| *v == view)?.1;
        let bytes = laid.item(id)?.bytes;
        match laid.item_ref(id)? {
            ItemRef::Node(node_id) => {
                let node = self.snapshot.node(node_id)?;
                let is_folder = node.kind() == NodeKind::Dir;
                Some(ItemInfo {
                    name: bs_layout::display_name(node.name()).into_string(),
                    bytes,
                    parent_bytes: node.parent().map(|p| p.allocated()),
                    files: if is_folder {
                        u64::from(node.file_count())
                    } else {
                        1
                    },
                    in_progress: node.is_in_progress(),
                    is_folder,
                })
            }
            ItemRef::Group(spec) => {
                let folder = self.snapshot.node(spec.folder)?;
                Some(ItemInfo {
                    name: format!(
                        "{} small items",
                        bs_layout::format_count(spec.members.len() as u64)
                    ),
                    bytes,
                    parent_bytes: Some(folder.allocated()),
                    files: spec.members.len() as u64,
                    in_progress: folder.is_in_progress(),
                    is_folder: false,
                })
            }
        }
    }

    fn begin_relayout(&mut self, view: ViewId, map: Rect) -> Option<RelayoutJob> {
        let target = self.table.target(view).cloned()?;
        let snapshot = Arc::clone(&self.snapshot);
        let metrics = self.metrics;
        Some(Box::new(move || {
            let started = Instant::now();
            let area = bs_layout::Rect::new(map.x, map.y, map.w, map.h);
            let laid = bs_layout::layout(&snapshot, &target, area, metrics);
            let items: Vec<Item> = laid.items().iter().map(to_item).collect();
            let done: Prepared = Box::new(Done {
                laid,
                items,
                took: started.elapsed(),
            });
            done
        }))
    }

    fn finish_relayout(&mut self, view: ViewId, _map: Rect, prepared: Prepared) -> Option<Layout> {
        let done = prepared.downcast::<Done>().ok()?;
        let Done { laid, items, took } = *done;
        // Offers are valid until this view is registered again.
        let folders = self.table.register(view, &laid);
        self.last = LayoutStats {
            items: items.len(),
            took,
        };
        self.remember(view, Arc::new(laid));
        Some(Layout {
            items: Arc::new(items),
            folders,
        })
    }
}

/// Maps one layout box to the renderer's item. Layout state bits and the
/// renderer's flag bits are the same by construction.
pub fn to_item(d: &DrawItem) -> Item {
    let kind = match d.kind {
        LayoutKind::FolderTitled => ItemKind::FolderTitled,
        LayoutKind::FolderBlock => ItemKind::FolderBlock,
        LayoutKind::FileTile => ItemKind::FileTile,
        LayoutKind::Group => ItemKind::Group,
        LayoutKind::Free => ItemKind::Free,
        LayoutKind::Unreadable => ItemKind::Unreadable,
        LayoutKind::Reserved => ItemKind::Reserved,
        LayoutKind::RecycleBin => ItemKind::RecycleBin,
    };
    let in_progress = d.flags.contains(bs_layout::StateFlags::IN_PROGRESS);
    let label = d.label.as_ref().map(|l| {
        let size: Box<str> = if in_progress && kind == ItemKind::FolderTitled {
            format!("Scanning… · {}", l.size).into()
        } else {
            l.size.clone()
        };
        Box::new(ItemLabel {
            name: l.name.clone(),
            size,
        })
    });
    Item {
        rect: Rect::new(d.rect.x, d.rect.y, d.rect.w, d.rect.h),
        kind,
        depth: d.depth,
        label,
        id: d.id,
        category: d.category.0,
        flags: Flags(d.flags.0),
    }
}

/// When the map is laid out again while a scan runs: often at first, so the
/// picture fills in quickly, then less and less. A layout that is slow to make
/// pushes the next one further out, so layout work never takes more than a
/// small share of the time.
pub fn relayout_interval(since_start: Duration, last_layout: Duration) -> Duration {
    let secs = since_start.as_secs_f32();
    let scheduled = if secs < 0.5 {
        // The first picture of a big folder should not wait.
        Duration::from_millis(60)
    } else if secs < 2.0 {
        Duration::from_millis(150)
    } else if secs < 10.0 {
        Duration::from_millis(400)
    } else if secs < 30.0 {
        Duration::from_millis(800)
    } else {
        Duration::from_millis(1500)
    };
    scheduled.max(last_layout * 5)
}

/// The status text while a scan runs or after it ended.
///
/// Sizes are the snapshot's, never the scanner's running counters, which count
/// every name of a hard-linked file. While the scan runs, the size carries "≥".
pub fn scan_status(snapshot: &Snapshot) -> String {
    let Some(root) = snapshot.root() else {
        return "Scanning…".to_string();
    };
    let size = bs_layout::format_size(root.allocated());
    let files = bs_layout::format_count(u64::from(root.file_count()));
    if snapshot.scan_stopped() {
        return format!("Scan stopped — results incomplete · ≥ {size} in {files} files");
    }
    if snapshot.scan_complete() {
        let bound = if root.bytes_are_lower_bound() {
            "≥ "
        } else {
            ""
        };
        return format!("{bound}{size} in {files} files");
    }
    match snapshot.progress() {
        Progress::Ratio(r) => format!(
            "Scanning… {:.0}% · ≥ {size} in {files} files",
            (r * 100.0).min(99.0)
        ),
        Progress::Files(_) => format!("Scanning… ≥ {size} in {files} files"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bs_layout::{CategoryId, Label, StateFlags};

    fn draw(kind: LayoutKind, flags: StateFlags) -> DrawItem {
        DrawItem {
            rect: bs_layout::Rect::new(1.0, 2.0, 30.0, 40.0),
            kind,
            depth: 2,
            label: Some(Box::new(Label {
                name: "Pictures".into(),
                size: "≥ 1.2 GB".into(),
            })),
            id: 7,
            category: CategoryId(3),
            flags,
            bytes: 1_200_000_000,
        }
    }

    #[test]
    fn items_map_field_by_field() {
        let it = to_item(&draw(LayoutKind::FileTile, StateFlags::DELETED));
        assert_eq!(it.rect, Rect::new(1.0, 2.0, 30.0, 40.0));
        assert_eq!(it.kind, ItemKind::FileTile);
        assert_eq!((it.depth, it.id, it.category), (2, 7, 3));
        assert!(it.flags.contains(Flags::DELETED));
        let label = it.label.expect("label");
        assert_eq!(&*label.name, "Pictures");
        assert_eq!(&*label.size, "≥ 1.2 GB");
    }

    #[test]
    fn unfinished_folder_titles_say_scanning() {
        let it = to_item(&draw(LayoutKind::FolderTitled, StateFlags::IN_PROGRESS));
        let label = it.label.expect("label");
        assert_eq!(&*label.size, "Scanning… · ≥ 1.2 GB");
        // Files and blocks have no title bar; they keep the plain size.
        let it = to_item(&draw(LayoutKind::FileTile, StateFlags::IN_PROGRESS));
        assert_eq!(&*it.label.expect("label").size, "≥ 1.2 GB");
        let it = to_item(&draw(LayoutKind::FolderTitled, StateFlags::NONE));
        assert_eq!(&*it.label.expect("label").size, "≥ 1.2 GB");
    }

    #[test]
    fn every_kind_maps() {
        for (a, b) in [
            (LayoutKind::FolderBlock, ItemKind::FolderBlock),
            (LayoutKind::Group, ItemKind::Group),
            (LayoutKind::Free, ItemKind::Free),
            (LayoutKind::Unreadable, ItemKind::Unreadable),
            (LayoutKind::Reserved, ItemKind::Reserved),
            (LayoutKind::RecycleBin, ItemKind::RecycleBin),
        ] {
            assert_eq!(to_item(&draw(a, StateFlags::NONE)).kind, b);
        }
    }

    #[test]
    fn relayout_slows_down_and_never_outruns_its_own_cost() {
        let ms = Duration::from_millis;
        let first = relayout_interval(ms(100), ms(2));
        let early = relayout_interval(ms(500), ms(2));
        let mid = relayout_interval(ms(5_000), ms(2));
        let late = relayout_interval(ms(60_000), ms(2));
        assert!(first < early && early < mid && mid < late);
        assert_eq!((first, early), (ms(60), ms(150)));
        // A 200 ms layout is followed by at least a second of other work.
        assert_eq!(relayout_interval(ms(500), ms(200)), ms(1000));
    }
}
