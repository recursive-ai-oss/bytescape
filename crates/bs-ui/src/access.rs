//! The accessibility tree: what a screen reader is told about the window.
//!
//! The tree is built from the same state the window draws from, so what is
//! announced is what is on screen. Updates are incremental: the boxes of the
//! map are described once per layout, a change of selection touches only the
//! boxes involved, and the chrome (a few dozen nodes) is compared against what
//! was sent last. Nothing here knows about a platform; the host hands each
//! [`TreeUpdate`] to the platform adapter and turns each [`ActionRequest`]
//! back into the input events a user would have made.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

pub use accesskit::ActionRequest;
use accesskit::{Action, Live, Node, NodeId, Rect as AkRect, Role, TreeId, TreeInfo, TreeUpdate};
use bs_render::scene::CATEGORY_NAMES;
use bs_render::{Flags, Item, ItemKind, Rect};

use super::{Shell, chrome};
use crate::info::percent;
use crate::picker::{FOLDER_BUTTON, OTHER_HEADING, PickTarget};
use crate::{InputEvent, ItemInfo, ViewId, ViewSource};

/// The window.
pub const ROOT: NodeId = NodeId(1);
const NAV: NodeId = NodeId(2);
const BACK: NodeId = NodeId(3);
const FORWARD: NodeId = NodeId(4);
const UP: NodeId = NodeId(5);
const REFRESH: NodeId = NodeId(6);
const CRUMBS: NodeId = NodeId(7);
const CAPACITY: NodeId = NodeId(8);
const PROGRESS: NodeId = NodeId(9);
const PROGRESS_TEXT: NodeId = NodeId(10);
const PROGRESS_BUTTON: NodeId = NodeId(11);
/// The map: a tree of boxes, or the list of drives in the picker.
pub const MAP: NodeId = NodeId(12);
const FOLDER_BUTTON_NODE: NodeId = NodeId(13);
const STATUS: NodeId = NodeId(14);
const INFO: NodeId = NodeId(15);
const LEGEND: NodeId = NodeId(16);
const OVERFLOW: NodeId = NodeId(17);
const LINK: NodeId = NodeId(18);
const NOTICE: NodeId = NodeId(19);
const CRUMB_BASE: u64 = 100;
const CRUMB_LIMIT: usize = 64;
const SWATCH_BASE: u64 = 200;
const CARD_BASE: u64 = 1_000;
const BOX_BASE: u64 = 1 << 32;

/// Names longer than this many characters are shortened in the middle for
/// the tree; a file name can be thousands of characters long.
const NAME_LIMIT: usize = 160;
const NAME_HEAD: usize = 100;
const NAME_TAIL: usize = 59;

/// A name made safe for a screen reader: control characters become spaces
/// and a very long name is cut in the middle. Works on characters, never on
/// byte positions.
pub fn clean_name(raw: &str) -> String {
    let count = raw.chars().count();
    let map = |c: char| if c.is_control() { ' ' } else { c };
    if count <= NAME_LIMIT {
        return raw.chars().map(map).collect();
    }
    let head: String = raw.chars().take(NAME_HEAD).map(map).collect();
    let tail: String = raw.chars().skip(count - NAME_TAIL).map(map).collect();
    format!("{head}…{tail}")
}

/// What a node stands for, so an action on it can be carried out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccessTarget {
    Back,
    Forward,
    Up,
    Refresh,
    /// A clickable part of the breadcrumb, by its position in the trail.
    Crumb(usize),
    Link,
    /// The progress overlay's Cancel or the stopped banner's Rescan.
    ProgressButton,
    /// A drive card of the picker.
    Card(usize),
    FolderButton,
    /// A box of the map, by item id.
    Box(u32),
}

fn ak_rect(r: Rect) -> AkRect {
    AkRect {
        x0: f64::from(r.x),
        y0: f64::from(r.y),
        x1: f64::from(r.x + r.w),
        y1: f64::from(r.y + r.h),
    }
}

fn node(role: Role, label: Option<&str>, rect: Rect) -> Node {
    let mut n = Node::new(role);
    if let Some(l) = label {
        n.set_label(l);
    }
    n.set_bounds(ak_rect(rect));
    n
}

/// A line of text. Screen readers take the words of a label from its value.
fn text_node(text: &str, rect: Rect) -> Node {
    let mut n = Node::new(Role::Label);
    n.set_value(text);
    n.set_bounds(ak_rect(rect));
    n
}

fn button(label: &str, rect: Rect, enabled: bool, reason: &str) -> Node {
    let mut n = node(Role::Button, Some(label), rect);
    if enabled {
        n.add_action(Action::Click);
    } else {
        n.set_disabled();
        n.set_description(reason);
    }
    n
}

/// One box of the map as a node. `info` carries the figures when the source
/// has them; otherwise the drawn label is all there is.
fn box_node(item: &Item, info: Option<&ItemInfo>) -> Node {
    let fallback = match item.kind {
        ItemKind::Free => "Free space",
        ItemKind::Unreadable => "Couldn't read",
        ItemKind::Reserved => "Reserved",
        ItemKind::RecycleBin => "Recycle Bin",
        ItemKind::Group => "Small files",
        ItemKind::FolderTitled | ItemKind::FolderBlock => "Folder",
        ItemKind::FileTile => "File",
    };
    let drawn = item.label.as_deref();
    let name = match (info, drawn) {
        (Some(i), _) => clean_name(&i.name),
        (None, Some(l)) => clean_name(&l.name),
        (None, None) => fallback.to_string(),
    };
    let mut details: Vec<String> = Vec::new();
    let size = match info {
        Some(i) => {
            let bound = if i.in_progress { "≥ " } else { "" };
            if let Some(p) = i.parent_bytes.and_then(|w| percent(i.bytes, w)) {
                details.push(format!("{bound}{p} of parent"));
            }
            if i.is_folder {
                details.push(format!(
                    "{} {}",
                    bs_layout::format_count(i.files),
                    if i.files == 1 { "file" } else { "files" }
                ));
            }
            Some(format!("{bound}{}", bs_layout::format_size(i.bytes)))
        }
        None => drawn.map(|l| clean_name(&l.size)),
    };
    let label = match &size {
        Some(s) if !s.is_empty() => format!("{name}, {s}"),
        _ => name,
    };
    match item.kind {
        ItemKind::FolderTitled | ItemKind::FolderBlock => details.insert(0, "Folder".into()),
        ItemKind::Group => details.insert(0, "Group of small files".into()),
        ItemKind::FileTile => {
            details.insert(0, "File".into());
            details.push(
                CATEGORY_NAMES
                    .get(item.category as usize)
                    .copied()
                    .unwrap_or("Other")
                    .to_string(),
            );
        }
        _ => {}
    }
    let mut n = node(Role::TreeItem, Some(&label), item.rect);
    n.set_level(usize::from(item.depth) + 1);
    n.set_selected(false);
    n.add_action(Action::Click);
    n.add_action(Action::Focus);
    if item.kind == ItemKind::FolderTitled {
        n.set_expanded(true);
    }
    if item.flags.contains(Flags::DELETED) {
        // The drawn label already says whether it went to the bin or was
        // deleted for good.
        let said = drawn
            .map(|l| clean_name(&l.size))
            .filter(|s| s.starts_with("Moved to ") || s.starts_with("Deleted"));
        n.set_state_description(said.unwrap_or_else(|| "Deleted".into()));
    } else if item.flags.contains(Flags::IN_PROGRESS) {
        n.set_busy();
        n.set_state_description("Scanning, size so far");
    }
    if !details.is_empty() {
        n.set_description(details.join(" · "));
    }
    n
}

fn box_id(id: u32) -> NodeId {
    NodeId(BOX_BASE + u64::from(id))
}

/// The map's boxes as nodes, nested the way the layout nests them.
struct Boxes {
    nodes: HashMap<NodeId, Node>,
    /// The top-level boxes, in drawing order.
    roots: Vec<NodeId>,
    /// Item ids and rectangles, for carrying actions out.
    rects: HashMap<u32, (Rect, ItemKind)>,
}

fn build_boxes(items: &[Item], infos: impl Fn(&Item) -> Option<ItemInfo>) -> Boxes {
    let mut nodes: HashMap<NodeId, Node> = HashMap::with_capacity(items.len());
    let mut rects = HashMap::with_capacity(items.len());
    let mut roots = Vec::new();
    // Open folders, innermost last: the depth and node of each.
    let mut stack: Vec<(u8, NodeId)> = Vec::new();
    for item in items {
        let id = box_id(item.id);
        if nodes.contains_key(&id) {
            continue;
        }
        while stack.last().is_some_and(|(d, _)| *d >= item.depth) {
            stack.pop();
        }
        let info = infos(item);
        let n = box_node(item, info.as_ref());
        match stack.last() {
            Some((_, parent)) => {
                if let Some(p) = nodes.get_mut(parent) {
                    p.push_child(id);
                }
            }
            None => roots.push(id),
        }
        nodes.insert(id, n);
        rects.insert(item.id, (item.rect, item.kind));
        if item.kind == ItemKind::FolderTitled {
            stack.push((item.depth, id));
        }
    }
    Boxes {
        nodes,
        roots,
        rects,
    }
}

/// What was last handed to the platform, and what the boxes were made from.
#[derive(Default)]
pub struct AccessTree {
    started: bool,
    chrome: HashMap<NodeId, Node>,
    boxes: Option<Boxes>,
    boxes_key: Option<(ViewId, u64, usize, usize)>,
    /// The map node last sent, with its children.
    selected: Vec<u32>,
    focus: Option<NodeId>,
    /// What chrome nodes and cards stand for.
    targets: HashMap<NodeId, AccessTarget>,
    /// Where the picker's cards and button are.
    card_rects: Vec<(AccessTarget, Rect)>,
    scale: f32,
    em: f32,
}

impl AccessTree {
    pub fn new() -> Self {
        Self::default()
    }

    /// Forgets what was sent, so the next update carries the whole tree.
    /// Called when a screen reader attaches.
    pub fn reset(&mut self) {
        self.started = false;
        self.chrome.clear();
        self.boxes = None;
        self.boxes_key = None;
        self.selected.clear();
        self.focus = None;
    }

    /// Whether a node stands for something an action can reach, and what.
    pub fn target(&self, id: NodeId) -> Option<AccessTarget> {
        if let Some(t) = self.targets.get(&id) {
            return Some(*t);
        }
        let raw = id.0;
        if raw >= BOX_BASE {
            let item = u32::try_from(raw - BOX_BASE).ok()?;
            return self
                .boxes
                .as_ref()
                .and_then(|b| b.rects.get(&item))
                .map(|_| AccessTarget::Box(item));
        }
        None
    }

    /// The number of nodes the tree holds now.
    pub fn node_count(&self) -> usize {
        self.chrome.len() + self.boxes.as_ref().map_or(0, |b| b.nodes.len())
    }

    /// The node for a box of the map.
    pub fn box_node(&self, item: u32) -> Option<&Node> {
        self.boxes.as_ref()?.nodes.get(&box_id(item))
    }

    /// The node for a box id.
    pub fn box_node_id(item: u32) -> NodeId {
        box_id(item)
    }

    /// The node of a chrome control or card, by what it stands for.
    pub fn node_for(&self, target: AccessTarget) -> Option<NodeId> {
        self.targets
            .iter()
            .find(|(_, t)| **t == target)
            .map(|(id, _)| *id)
    }

    /// The current node by id (chrome or box).
    pub fn get(&self, id: NodeId) -> Option<&Node> {
        self.chrome
            .get(&id)
            .or_else(|| self.boxes.as_ref().and_then(|b| b.nodes.get(&id)))
    }
}

/// Where a click on `target` lands, in window coordinates.
fn click_point(rect: Rect, kind: Option<ItemKind>, em: f32) -> (f32, f32) {
    let x = rect.x + rect.w / 2.0;
    match kind {
        // A folder's children cover its middle; its title bar is the part
        // that is the folder itself.
        Some(ItemKind::FolderTitled) => (x, rect.y + (em * 0.6).min(rect.h / 2.0)),
        _ => (x, rect.y + rect.h / 2.0),
    }
}

impl<S: ViewSource> Shell<S> {
    /// The tree changes since the last call, or the whole tree when none was
    /// sent yet. Always returns an update (at least the focus), because the
    /// platform adapter wants one per call.
    pub fn access_update(&mut self, tree: &mut AccessTree, now: Duration) -> TreeUpdate {
        self.clock = self.clock.max(now);
        let first = !tree.started;
        tree.scale = self.scale;
        tree.em = self.font_px * self.scale;

        let mut changed: Vec<(NodeId, Node)> = Vec::new();
        let mut map_children: Vec<NodeId> = Vec::new();

        // The boxes of the map.
        if let Some(app) = &self.app {
            let items = app.items();
            let key = (
                app.current_view(),
                app.generation(),
                std::sync::Arc::as_ptr(&items) as usize,
                items.len(),
            );
            let selected_now = app.selection().to_vec();
            if tree.boxes_key != Some(key) {
                let view = app.current_view();
                let mut built = build_boxes(&items, |it| app.source().describe(view, it.id));
                for item in &selected_now {
                    if let Some(n) = built.nodes.get_mut(&box_id(*item)) {
                        n.set_selected(true);
                    }
                }
                let old = tree.boxes.take();
                for (id, n) in &built.nodes {
                    let same = old.as_ref().and_then(|o| o.nodes.get(id)) == Some(n);
                    if !same {
                        changed.push((*id, n.clone()));
                    }
                }
                tree.boxes = Some(built);
                tree.boxes_key = Some(key);
                tree.selected = selected_now;
            } else if tree.selected != selected_now
                && let Some(boxes) = tree.boxes.as_mut()
            {
                let touched: HashSet<u32> = tree
                    .selected
                    .iter()
                    .chain(selected_now.iter())
                    .copied()
                    .collect();
                for item in touched {
                    let id = box_id(item);
                    if let Some(n) = boxes.nodes.get_mut(&id) {
                        n.set_selected(selected_now.contains(&item));
                        changed.push((id, n.clone()));
                    }
                }
                tree.selected = selected_now;
            }
        } else if tree.boxes.take().is_some() {
            tree.boxes_key = None;
            tree.selected.clear();
        }
        if let Some(boxes) = &tree.boxes {
            map_children = boxes.roots.clone();
        }

        // The chrome.
        let mut fresh: Vec<(NodeId, Node)> = Vec::new();
        let mut targets: HashMap<NodeId, AccessTarget> = HashMap::new();
        let root_children =
            self.access_chrome(&mut fresh, &mut targets, &mut tree.card_rects, map_children);
        tree.targets = targets;
        let mut root = Node::new(Role::Window);
        root.set_label("Bytescape");
        root.set_bounds(ak_rect(Rect::new(
            0.0,
            0.0,
            self.width as f32,
            self.height as f32,
        )));
        root.set_children(root_children);
        fresh.push((ROOT, root));
        let fresh_ids: HashSet<NodeId> = fresh.iter().map(|(id, _)| *id).collect();
        for (id, n) in fresh {
            if tree.chrome.get(&id) != Some(&n) {
                tree.chrome.insert(id, n.clone());
                changed.push((id, n));
            }
        }
        tree.chrome.retain(|id, _| fresh_ids.contains(id));

        // Focus follows the keyboard selection.
        let focus = if let Some(app) = &self.app {
            app.selected()
                .map(box_id)
                .filter(|id| {
                    tree.boxes
                        .as_ref()
                        .is_some_and(|b| b.nodes.contains_key(id))
                })
                .unwrap_or(MAP)
        } else if let Some(i) = self.picker_sel {
            NodeId(CARD_BASE + i as u64)
        } else {
            MAP
        };
        let focus = if tree.chrome.contains_key(&focus)
            || tree
                .boxes
                .as_ref()
                .is_some_and(|b| b.nodes.contains_key(&focus))
        {
            focus
        } else {
            MAP
        };
        tree.focus = Some(focus);

        let mut update = TreeUpdate {
            nodes: changed,
            tree: None,
            tree_id: TreeId::ROOT,
            focus,
        };
        if first {
            update.tree = Some(TreeInfo::new(ROOT));
            tree.started = true;
        }
        update
    }

    /// The whole tree again, for a screen reader that just attached.
    pub fn access_full(&mut self, tree: &mut AccessTree, now: Duration) -> TreeUpdate {
        tree.reset();
        self.access_update(tree, now)
    }

    /// Builds the chrome nodes. Returns the window's children, in reading
    /// order.
    fn access_chrome(
        &mut self,
        out: &mut Vec<(NodeId, Node)>,
        targets: &mut HashMap<NodeId, AccessTarget>,
        card_rects: &mut Vec<(AccessTarget, Rect)>,
        map_children: Vec<NodeId>,
    ) -> Vec<NodeId> {
        let g = self.geometry().clone();
        let (can_back, can_fwd, can_up) =
            (self.can_go_back(), self.can_go_forward(), self.can_go_up());
        let up_enabled = can_up || (self.picker_return && self.app.is_some());
        let mut root_children = Vec::new();
        card_rects.clear();

        // Navigation bar.
        let mut nav = node(Role::Toolbar, Some("Navigation"), g.nav);
        let mut nav_children = Vec::new();
        for (id, label, rect, enabled, reason, target) in [
            (
                BACK,
                "Back",
                g.back,
                can_back,
                "Nothing to go back to",
                AccessTarget::Back,
            ),
            (
                FORWARD,
                "Forward",
                g.forward,
                can_fwd,
                "Nothing to go forward to",
                AccessTarget::Forward,
            ),
            (
                UP,
                "Up one level",
                g.up,
                up_enabled,
                "Already at the top",
                AccessTarget::Up,
            ),
            (
                REFRESH,
                "Refresh",
                g.refresh,
                true,
                "",
                AccessTarget::Refresh,
            ),
        ] {
            out.push((id, button(label, rect, enabled, reason)));
            if enabled {
                targets.insert(id, target);
            }
            nav_children.push(id);
        }
        let parts = self.crumb_parts();
        let mut crumbs = node(Role::Navigation, Some("Location"), g.crumb);
        for (i, part) in parts.iter().enumerate().take(CRUMB_LIMIT) {
            if part.text == chrome::CRUMB_SEPARATOR {
                continue;
            }
            let id = NodeId(CRUMB_BASE + i as u64);
            let text = clean_name(&part.text);
            let n = match part.target {
                Some(seg) => {
                    targets.insert(id, AccessTarget::Crumb(seg));
                    button(&text, part.rect, true, "")
                }
                None => text_node(&text, part.rect),
            };
            out.push((id, n));
            crumbs.push_child(id);
        }
        out.push((CRUMBS, crumbs));
        nav_children.push(CRUMBS);
        nav.set_children(nav_children);
        out.push((NAV, nav));
        root_children.push(NAV);

        // The capacity bar, as a labelled value.
        if let Some(b) = self.capacity.breakdown() {
            let mut n = node(Role::Meter, Some("Drive capacity"), g.capacity);
            n.set_value(b.describe());
            if b.total > 0 {
                n.set_min_numeric_value(0.0);
                n.set_max_numeric_value(100.0);
                n.set_numeric_value(
                    (b.total.saturating_sub(b.free)) as f64 / b.total as f64 * 100.0,
                );
            }
            out.push((CAPACITY, n));
            root_children.push(CAPACITY);
        }

        // The progress readout, or the stopped banner.
        if let Some(o) = self.overlay() {
            let label = if o.stopped {
                "Scan stopped"
            } else {
                "Scan progress"
            };
            let mut group = node(Role::Group, Some(label), o.panel);
            out.push((PROGRESS_TEXT, text_node(&o.words, o.text)));
            let mut b = button(o.button_text, o.button, true, "");
            if !o.stopped && self.scanning {
                b.set_description("Stops the scan and keeps what was counted");
            }
            out.push((PROGRESS_BUTTON, b));
            targets.insert(PROGRESS_BUTTON, AccessTarget::ProgressButton);
            group.set_children(vec![PROGRESS_TEXT, PROGRESS_BUTTON]);
            if !o.stopped {
                group.set_busy();
            }
            out.push((PROGRESS, group));
            root_children.push(PROGRESS);
        }

        // The map.
        let map_rect = self.map_rect();
        if self.app.is_some() {
            let name = {
                let trail = self.crumb_segments();
                if trail.is_empty() {
                    "Map".to_string()
                } else {
                    format!(
                        "Map of {}",
                        clean_name(trail.last().map_or("", String::as_str))
                    )
                }
            };
            let mut map = node(Role::Tree, Some(&name), map_rect);
            map.set_multiselectable();
            if self.scanning {
                map.set_busy();
            }
            map.set_children(map_children);
            out.push((MAP, map));
        } else if self.picker.is_some() {
            let mut list = node(Role::ListBox, Some("Drives"), map_rect);
            let mut kids = Vec::new();
            let layout = self.picker_layout();
            let rect_of = |t: PickTarget| {
                layout
                    .as_ref()
                    .and_then(|l| l.slots.iter().find(|(s, _)| *s == t).map(|(_, r)| *r))
            };
            let mut button_rect = None;
            if let Some(picker) = &self.picker {
                for (i, card) in picker.cards().iter().enumerate() {
                    let id = NodeId(CARD_BASE + i as u64);
                    let rect = rect_of(PickTarget::Card(i)).unwrap_or(map_rect);
                    let mut n = node(Role::ListBoxOption, Some(&clean_name(&card.title)), rect);
                    n.set_description(format!(
                        "{} · {} · {}",
                        card.detail,
                        card.used_text(),
                        card.scan_text()
                    ));
                    n.set_selected(self.picker_sel == Some(i));
                    n.add_action(Action::Click);
                    n.add_action(Action::Focus);
                    if matches!(
                        card.scan,
                        crate::picker::CardScan::Starting | crate::picker::CardScan::Running { .. }
                    ) {
                        n.set_busy();
                    }
                    targets.insert(id, AccessTarget::Card(i));
                    card_rects.push((AccessTarget::Card(i), rect));
                    out.push((id, n));
                    kids.push(id);
                }
            }
            if let Some(r) = rect_of(PickTarget::Folder) {
                button_rect = Some(r);
            }
            list.set_description(OTHER_HEADING);
            list.set_children(kids);
            out.push((MAP, list));
            if let Some(r) = button_rect {
                out.push((FOLDER_BUTTON_NODE, button(FOLDER_BUTTON, r, true, "")));
                targets.insert(FOLDER_BUTTON_NODE, AccessTarget::FolderButton);
                card_rects.push((AccessTarget::FolderButton, r));
            }
        } else {
            let mut map = node(Role::Group, Some("Map"), map_rect);
            map.set_busy();
            out.push((MAP, map));
        }
        root_children.push(MAP);
        if self.app.is_none()
            && self.picker.is_some()
            && out.iter().any(|(id, _)| *id == FOLDER_BUTTON_NODE)
        {
            root_children.push(FOLDER_BUTTON_NODE);
        }

        // The status bar.
        let mut status = node(Role::Status, Some("Status"), g.status);
        let mut kids = Vec::new();
        let info = self.info_line();
        let info_text = if info.details.is_empty() {
            clean_name(&info.name)
        } else {
            format!("{} · {}", clean_name(&info.name), info.details)
        };
        out.push((INFO, text_node(&info_text, g.info)));
        kids.push(INFO);
        if let Some((text, until)) = &self.notice
            && self.clock < *until
        {
            let mut n = text_node(text, g.info);
            n.set_live(Live::Polite);
            out.push((NOTICE, n));
            kids.push(NOTICE);
        }
        let mut legend = node(Role::Group, Some("Categories"), g.status);
        let mut legend_kids = Vec::new();
        for s in &g.swatches {
            let name = chrome::LEGEND_NAMES[s.category as usize % chrome::LEGEND_NAMES.len()];
            let id = NodeId(SWATCH_BASE + u64::from(s.category));
            let mut n = node(Role::Label, None, s.rect);
            n.set_value(name);
            out.push((id, n));
            legend_kids.push(id);
        }
        if let Some(r) = g.overflow {
            let hidden: Vec<&str> = chrome::LEGEND_NAMES
                .iter()
                .copied()
                .skip(g.swatches.len())
                .collect();
            let mut n = node(Role::Label, None, r);
            n.set_value(format!("More categories: {}", hidden.join(", ")));
            out.push((OVERFLOW, n));
            legend_kids.push(OVERFLOW);
        }
        legend.set_children(legend_kids);
        out.push((LEGEND, legend));
        kids.push(LEGEND);
        let mut link = node(Role::Link, Some(g.link_text), g.link);
        link.set_url(chrome::LINK_URL);
        link.add_action(Action::Click);
        out.push((LINK, link));
        targets.insert(LINK, AccessTarget::Link);
        kids.push(LINK);
        status.set_children(kids);
        out.push((STATUS, status));
        root_children.push(STATUS);
        root_children
    }

    /// Carries out an action a screen reader asked for, through the same
    /// input events a pointer would make. Returns what the host must do next,
    /// as for any other event.
    pub fn access_action(
        &mut self,
        tree: &AccessTree,
        request: &ActionRequest,
        now: Duration,
    ) -> Vec<super::ShellEffect> {
        if !matches!(request.action, Action::Click | Action::Focus) {
            return Vec::new();
        }
        let Some(target) = tree.target(request.target_node) else {
            return Vec::new();
        };
        let em = tree.em;
        let point = match target {
            AccessTarget::Box(id) => tree
                .boxes
                .as_ref()
                .and_then(|b| b.rects.get(&id))
                .map(|(r, k)| click_point(*r, Some(*k), em)),
            AccessTarget::Card(_) | AccessTarget::FolderButton => tree
                .card_rects
                .iter()
                .find(|(t, _)| *t == target)
                .map(|(_, r)| click_point(*r, None, em)),
            AccessTarget::Crumb(index) => {
                // The trail segment itself, not a position on screen.
                self.hide_card();
                self.card.rest = None;
                return self.map_event(InputEvent::CrumbClick { index }, now);
            }
            _ => tree
                .get(request.target_node)
                .and_then(Node::bounds)
                .map(|b| (((b.x0 + b.x1) / 2.0) as f32, ((b.y0 + b.y1) / 2.0) as f32)),
        };
        // Focusing a control does nothing but a click on a box or card: the
        // keyboard selection is what the focus follows.
        let boxlike = matches!(target, AccessTarget::Box(_) | AccessTarget::Card(_));
        if request.action == Action::Focus && !boxlike {
            return Vec::new();
        }
        match point {
            Some((x, y)) => self.update(InputEvent::Click { x, y }, now),
            None => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bs_render::ItemLabel;

    fn item(kind: ItemKind, flags: Flags, size: &str) -> Item {
        Item {
            rect: Rect::new(10.0, 20.0, 100.0, 50.0),
            kind,
            depth: 1,
            label: Some(Box::new(ItemLabel {
                name: "old.iso".into(),
                size: size.into(),
            })),
            id: 7,
            category: 4,
            flags,
        }
    }

    #[test]
    fn a_deleted_box_says_where_it_went() {
        let n = box_node(
            &item(
                ItemKind::FileTile,
                Flags::DELETED,
                "Moved to Recycle Bin · 4.0 GB",
            ),
            None,
        );
        assert_eq!(n.state_description(), Some("Moved to Recycle Bin · 4.0 GB"));
        let n = box_node(
            &item(ItemKind::FileTile, Flags::DELETED, "Deleted · 4.0 GB"),
            None,
        );
        assert_eq!(n.state_description(), Some("Deleted · 4.0 GB"));
        // Without a drawn label it still says it was deleted.
        let mut bare = item(ItemKind::FileTile, Flags::DELETED, "");
        bare.label = None;
        assert_eq!(box_node(&bare, None).state_description(), Some("Deleted"));
    }

    #[test]
    fn a_box_still_being_counted_is_busy() {
        let n = box_node(
            &item(
                ItemKind::FolderTitled,
                Flags::IN_PROGRESS,
                "Scanning… · 1.2 GB",
            ),
            None,
        );
        assert!(n.is_busy());
        assert_eq!(n.state_description(), Some("Scanning, size so far"));
        assert_eq!(n.is_expanded(), Some(true));
        assert_eq!(n.level(), Some(2));
    }

    #[test]
    fn special_blocks_are_named_even_without_a_label() {
        let mut free = item(ItemKind::Free, Flags::default(), "");
        free.label = None;
        assert_eq!(box_node(&free, None).label(), Some("Free space"));
        let mut bin = item(ItemKind::RecycleBin, Flags::default(), "");
        bin.label = None;
        assert_eq!(box_node(&bin, None).label(), Some("Recycle Bin"));
    }

    #[test]
    fn box_ids_never_meet_the_chrome_ids() {
        assert!(box_id(0).0 > CARD_BASE + 1_000_000);
        assert_eq!(clean_name(""), "");
    }
}
