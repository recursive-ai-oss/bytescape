//! The application state machine, input handling, views, widgets and animation.
//!
//! Input is described by this crate's own [`InputEvent`] and handled by the
//! pure [`App::update`], which returns [`Effect`]s for the host to apply. The
//! window code translates real events into `InputEvent`s; tests and the
//! benchmark harness feed the same events in directly, so a scripted zoom takes
//! exactly the path a user's wheel takes.

pub mod capacity;
pub mod chrome;
pub mod info;
pub mod shell;
pub mod tree;

use std::any::Any;
use std::sync::Arc;
use std::time::Duration;

use bs_render::{Item, Rect, ZoomDirection};

/// Identifies a folder view. Opaque to the UI; the [`ViewSource`] defines it.
pub type ViewId = u64;

/// The laid-out contents of one view.
#[derive(Clone, Debug)]
pub struct Layout {
    pub items: Arc<Vec<Item>>,
    /// For each folder box in `items` that can be entered: its item id and the
    /// view it opens.
    pub folders: Vec<(u32, ViewId)>,
}

/// What a source prepared on another thread: the finished layout of a view,
/// in a form only the source that made it understands.
pub type Prepared = Box<dyn Any + Send>;

/// Work a source can do on a worker thread.
pub type RelayoutJob = Box<dyn FnOnce() -> Prepared + Send>;

/// What the info bar says about one box.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ItemInfo {
    /// The name, converted for display.
    pub name: String,
    /// Allocated bytes the box stands for, as counted so far.
    pub bytes: u64,
    /// Allocated bytes of the folder that holds it, when it has one.
    pub parent_bytes: Option<u64>,
    /// Files inside (a file counts as one).
    pub files: u64,
    /// Still being counted: `bytes` is a lower bound.
    pub in_progress: bool,
    pub is_folder: bool,
}

/// Where views come from. The real implementation sits on the size tree and
/// the layout code; tests and the benchmark use a synthetic one.
pub trait ViewSource {
    fn root(&self) -> ViewId;
    /// Lays out `view` to fill `map` (device pixels).
    fn layout(&mut self, view: ViewId, map: Rect) -> Layout;
    fn parent(&self, view: ViewId) -> Option<ViewId>;
    /// The user entered `view`: keep it, and the views above it, available
    /// until history no longer reaches them.
    fn enter(&mut self, _view: ViewId) {}
    /// Navigation moved on. Only the views in `keep` (the current stack and
    /// the history) still matter; the source may forget the rest.
    fn prune(&mut self, _keep: &[ViewId]) {}
    /// The breadcrumb text of a view.
    fn breadcrumb(&self, _view: ViewId) -> String {
        String::new()
    }
    /// What to tell about box `id` of `view`.
    fn describe(&self, _view: ViewId, _id: u32) -> Option<ItemInfo> {
        None
    }
    /// Starts laying `view` out again with the data as it is now, on another
    /// thread. `None` when the source can only lay out in place.
    fn begin_relayout(&mut self, _view: ViewId, _map: Rect) -> Option<RelayoutJob> {
        None
    }
    /// Takes what the job returned and makes it the view's layout.
    fn finish_relayout(
        &mut self,
        _view: ViewId,
        _map: Rect,
        _prepared: Prepared,
    ) -> Option<Layout> {
        None
    }
}

/// Keys that were held while a button was pressed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Modifiers {
    pub ctrl: bool,
    pub shift: bool,
}

/// A keyboard key the map reacts to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Enter,
    Backspace,
    Home,
    Refresh,
    Escape,
    AltLeft,
    AltRight,
    AltUp,
}

/// An input event, already translated from the windowing system.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum InputEvent {
    PointerMoved {
        x: f32,
        y: f32,
    },
    PointerLeft,
    /// Positive `steps` is wheel up (deeper), negative is wheel down (up).
    Wheel {
        x: f32,
        y: f32,
        steps: f32,
    },
    Click {
        x: f32,
        y: f32,
    },
    /// A click with Ctrl or Shift held: Ctrl toggles a box in the selection,
    /// Shift adds the boxes between the last click and this one.
    ClickWith {
        x: f32,
        y: f32,
        mods: Modifiers,
    },
    DoubleClick {
        x: f32,
        y: f32,
    },
    Key(Key),
    /// The window size or scale factor changed.
    Resized {
        map: Rect,
    },
    /// The data behind the views changed (a scan published more of the tree).
    /// The current view is laid out again; the views above it are refreshed
    /// only when the user goes back to them.
    DataChanged,
}

/// What the host should do after an event.
#[derive(Clone, Debug)]
pub enum Effect {
    SetHover(Option<u32>),
    SetSelected(Option<u32>),
    /// The selection holds several boxes.
    SetSelection(Vec<u32>),
    /// Show a new view without animation.
    SetView(Arc<Vec<Item>>),
    /// Animate to a new view. `child` is the folder's box in the larger view.
    Zoom {
        direction: ZoomDirection,
        items: Arc<Vec<Item>>,
        child: Rect,
    },
    /// The running animation should jump to its end now.
    SkipAnimation,
    /// The view's folder path changed; the breadcrumb should be rebuilt.
    ViewChanged(ViewId),
}

struct Level {
    view: ViewId,
    layout: Layout,
    /// Laid out from older data than the top of the stack; refreshed when it
    /// is shown again.
    stale: bool,
}

/// A layout to make on a worker thread. See [`App::begin_relayout`].
pub struct PendingRelayout {
    token: u64,
    view: ViewId,
    map: Rect,
    job: RelayoutJob,
}

impl PendingRelayout {
    /// Does the layout work. Call it on a worker thread.
    pub fn run(self) -> FinishedRelayout {
        FinishedRelayout {
            token: self.token,
            view: self.view,
            map: self.map,
            prepared: (self.job)(),
        }
    }
}

/// The result of a [`PendingRelayout`].
pub struct FinishedRelayout {
    token: u64,
    view: ViewId,
    map: Rect,
    prepared: Prepared,
}

/// History keeps this many entries on each side.
const HISTORY_LIMIT: usize = 256;

/// The state of the map window.
pub struct App<S: ViewSource> {
    source: S,
    map: Rect,
    stack: Vec<Level>,
    hover: Option<u32>,
    /// Selected boxes in the order they were picked; the last is the primary
    /// one, which Enter opens and the info bar describes.
    selected: Vec<u32>,
    /// Where a Shift-click range starts.
    anchor: Option<u32>,
    /// Changes with every navigation, so work prepared for an earlier view is
    /// recognized and dropped.
    nav_token: u64,
    /// Whether the renderer is still running a zoom. The renderer owns the
    /// animation clock; the host copies its answer here before each event.
    animating: bool,
    history_back: Vec<ViewId>,
    history_forward: Vec<ViewId>,
}

impl<S: ViewSource> App<S> {
    pub fn new(mut source: S, map: Rect) -> Self {
        let root = source.root();
        let layout = source.layout(root, map);
        Self {
            source,
            map,
            stack: vec![Level {
                view: root,
                layout,
                stale: false,
            }],
            hover: None,
            selected: Vec::new(),
            anchor: None,
            nav_token: 0,
            animating: false,
            history_back: Vec::new(),
            history_forward: Vec::new(),
        }
    }

    /// Tells the app whether the renderer is still animating a zoom. Call this
    /// with `renderer.is_animating()` before feeding an event; navigation that
    /// arrives mid-animation then asks the renderer to jump to the end first.
    pub fn set_animating(&mut self, animating: bool) {
        self.animating = animating;
    }

    /// The map rectangle the views were last laid out for.
    pub fn map(&self) -> Rect {
        self.map
    }

    /// The view source.
    pub fn source(&self) -> &S {
        &self.source
    }

    /// The view source, for changing its geometry before a resize.
    pub fn source_mut(&mut self) -> &mut S {
        &mut self.source
    }

    pub fn items(&self) -> Arc<Vec<Item>> {
        self.stack
            .last()
            .map(|l| Arc::clone(&l.layout.items))
            .unwrap_or_default()
    }

    pub fn current_view(&self) -> ViewId {
        self.stack.last().map_or(0, |l| l.view)
    }

    pub fn depth(&self) -> usize {
        self.stack.len()
    }

    pub fn hover(&self) -> Option<u32> {
        self.hover
    }

    /// The primary selected box.
    pub fn selected(&self) -> Option<u32> {
        self.selected.last().copied()
    }

    /// Every selected box.
    pub fn selection(&self) -> &[u32] {
        &self.selected
    }

    /// Whether there is a view to go back to.
    pub fn can_go_back(&self) -> bool {
        !self.history_back.is_empty()
    }

    /// Whether there is a view to go forward to.
    pub fn can_go_forward(&self) -> bool {
        !self.history_forward.is_empty()
    }

    /// Whether there is a folder above this view.
    pub fn can_go_up(&self) -> bool {
        self.stack.len() > 1
    }

    /// What the info bar tells about the box under the pointer, or else about
    /// the selection. `None` when there is neither.
    pub fn info(&self) -> Option<Vec<ItemInfo>> {
        let view = self.current_view();
        let ids: Vec<u32> = match self.hover {
            Some(h) => vec![h],
            None => self.selected.clone(),
        };
        let infos: Vec<ItemInfo> = ids
            .iter()
            .filter_map(|&id| self.source.describe(view, id))
            .collect();
        (!infos.is_empty()).then_some(infos)
    }

    /// Whether the info bar is describing the pointer rather than the
    /// selection.
    pub fn hovering(&self) -> bool {
        self.hover.is_some()
    }

    fn top(&self) -> Option<&Level> {
        self.stack.last()
    }

    fn hit(&self, x: f32, y: f32) -> Option<&Item> {
        self.top()?
            .layout
            .items
            .iter()
            .rev()
            .find(|it| it.rect.contains(x, y))
    }

    /// The box one level below the view's root that holds the point.
    fn child_under(&self, x: f32, y: f32) -> Option<(u32, ViewId, Rect)> {
        let level = self.top()?;
        let it = level
            .layout
            .items
            .iter()
            .find(|it| it.depth == 1 && it.rect.contains(x, y))?;
        let view = level
            .layout
            .folders
            .iter()
            .find(|(id, _)| *id == it.id)
            .map(|(_, v)| *v)?;
        Some((it.id, view, it.rect))
    }

    /// Handles one event. `_now` is the time since the app started; nothing
    /// uses it yet, and it is reserved for time-based input handling such as
    /// coalescing wheel events.
    pub fn update(&mut self, event: InputEvent, _now: Duration) -> Vec<Effect> {
        let mut fx = Vec::new();
        match event {
            InputEvent::PointerMoved { x, y } => {
                let id = self.hit(x, y).map(|i| i.id);
                if id != self.hover {
                    self.hover = id;
                    fx.push(Effect::SetHover(id));
                }
            }
            InputEvent::PointerLeft => {
                if self.hover.take().is_some() {
                    fx.push(Effect::SetHover(None));
                }
            }
            InputEvent::Click { x, y } => self.click(x, y, Modifiers::default(), &mut fx),
            InputEvent::ClickWith { x, y, mods } => self.click(x, y, mods, &mut fx),
            InputEvent::Wheel { x, y, steps } => {
                self.skip_running_animation(&mut fx);
                if steps > 0.0 {
                    self.zoom_in_at(x, y, &mut fx);
                } else if steps < 0.0 {
                    self.zoom_out(&mut fx);
                }
            }
            InputEvent::DoubleClick { x, y } => {
                self.skip_running_animation(&mut fx);
                self.zoom_in_at(x, y, &mut fx);
            }
            InputEvent::Key(Key::Enter) => {
                self.skip_running_animation(&mut fx);
                if let Some(sel) = self.selected() {
                    let target = self.top().and_then(|l| {
                        let it = l.layout.items.iter().find(|i| i.id == sel)?;
                        let view = l.layout.folders.iter().find(|(id, _)| *id == sel)?.1;
                        Some((view, it.rect))
                    });
                    if let Some((view, rect)) = target {
                        self.enter(view, rect, &mut fx);
                    }
                }
            }
            InputEvent::Key(Key::Backspace | Key::AltUp) => {
                self.skip_running_animation(&mut fx);
                self.zoom_out(&mut fx);
            }
            InputEvent::Key(Key::Home) => {
                self.skip_running_animation(&mut fx);
                if self.stack.len() > 1 {
                    // Back returns to where the user was.
                    self.push_history(self.current_view());
                    self.history_forward.clear();
                }
                while self.stack.len() > 1 {
                    self.stack.pop();
                }
                self.after_set_view(&mut fx);
            }
            InputEvent::Key(Key::Escape) => {
                if !self.selected.is_empty() {
                    self.selected.clear();
                    self.anchor = None;
                    fx.push(Effect::SetSelected(None));
                }
            }
            InputEvent::Key(Key::Refresh) => {
                self.nav_token += 1;
                let top = self.stack.len().saturating_sub(1);
                for (i, level) in self.stack.iter_mut().enumerate() {
                    level.stale = i != top;
                }
                self.refresh_top_now();
                self.restore_after_relayout(&mut fx);
            }
            InputEvent::Key(Key::AltLeft) => {
                self.skip_running_animation(&mut fx);
                if let Some(view) = self.history_back.pop() {
                    let here = self.current_view();
                    self.history_forward.push(here);
                    self.jump_to(view, &mut fx);
                }
            }
            InputEvent::Key(Key::AltRight) => {
                self.skip_running_animation(&mut fx);
                if let Some(view) = self.history_forward.pop() {
                    let here = self.current_view();
                    self.history_back.push(here);
                    self.jump_to(view, &mut fx);
                }
            }
            InputEvent::DataChanged => {
                let top = self.stack.len().saturating_sub(1);
                for (i, level) in self.stack.iter_mut().enumerate() {
                    level.stale = i != top;
                }
                self.refresh_top_now();
                self.restore_after_relayout(&mut fx);
            }
            InputEvent::Resized { map } => {
                self.map = map;
                let view = self.current_view();
                let mut views = Vec::new();
                let mut v = Some(view);
                while let Some(id) = v {
                    views.push(id);
                    v = self.source.parent(id);
                }
                views.reverse();
                self.stack = views
                    .into_iter()
                    .map(|view| Level {
                        view,
                        layout: self.source.layout(view, map),
                        stale: false,
                    })
                    .collect();
                self.hover = None;
                self.selected.clear();
                self.anchor = None;
                self.nav_token += 1;
                self.animating = false;
                fx.push(Effect::SetView(self.items()));
            }
        }
        fx
    }

    /// Lays the top of the stack out again, here and now.
    fn refresh_top_now(&mut self) {
        let map = self.map;
        if let Some(level) = self.stack.last_mut() {
            level.layout = self.source.layout(level.view, map);
            level.stale = false;
        }
    }

    /// After the top view got a new layout for the same folder: show it and
    /// give hover and selection back to the boxes that are still there. The
    /// renderer forgets both with the old view.
    fn restore_after_relayout(&mut self, fx: &mut Vec<Effect>) {
        fx.push(Effect::SetView(self.items()));
        let items = self.items();
        let present = |id: u32| items.iter().any(|i| i.id == id);
        self.hover = self.hover.filter(|&id| present(id));
        self.selected.retain(|&id| present(id));
        if let Some(id) = self.hover {
            fx.push(Effect::SetHover(Some(id)));
        }
        if !self.selected.is_empty() {
            fx.push(self.selection_effect());
        }
    }

    fn selection_effect(&self) -> Effect {
        match self.selected.as_slice() {
            [] => Effect::SetSelected(None),
            [one] => Effect::SetSelected(Some(*one)),
            many => Effect::SetSelection(many.to_vec()),
        }
    }

    fn click(&mut self, x: f32, y: f32, mods: Modifiers, fx: &mut Vec<Effect>) {
        let Some(id) = self.hit(x, y).map(|i| i.id) else {
            return;
        };
        let next: Vec<u32> = if mods.shift {
            let from = self.anchor.unwrap_or(id);
            let mut range = self.range_between(from, id);
            // A range adds to what was selected before it.
            let mut all = self.selected.clone();
            all.retain(|i| !range.contains(i));
            all.append(&mut range);
            // The clicked box is the primary one.
            all.retain(|&i| i != id);
            all.push(id);
            all
        } else if mods.ctrl {
            let mut all = self.selected.clone();
            if let Some(at) = all.iter().position(|&i| i == id) {
                all.remove(at);
            } else {
                all.push(id);
            }
            self.anchor = Some(id);
            all
        } else {
            self.anchor = Some(id);
            vec![id]
        };
        if next != self.selected {
            self.selected = next;
            fx.push(self.selection_effect());
        }
    }

    /// The boxes from `a` to `b` in layout order that share a parent and a
    /// depth. When the two do not, only `b`.
    fn range_between(&self, a: u32, b: u32) -> Vec<u32> {
        let Some(level) = self.top() else {
            return vec![b];
        };
        let items = &level.layout.items;
        let at = |id: u32| items.iter().position(|i| i.id == id);
        let (Some(ia), Some(ib)) = (at(a), at(b)) else {
            return vec![b];
        };
        let depth = items[ia].depth;
        let parent_of = |i: usize| (0..i).rev().find(|&p| items[p].depth + 1 == items[i].depth);
        if items[ib].depth != depth || depth == 0 || parent_of(ia) != parent_of(ib) {
            return vec![b];
        }
        let parent = parent_of(ia);
        let (lo, hi) = (ia.min(ib), ia.max(ib));
        (lo..=hi)
            .filter(|&i| items[i].depth == depth && parent_of(i) == parent)
            .map(|i| items[i].id)
            .collect()
    }

    /// Starts laying the current view out again on another thread, for sources
    /// that can (the data behind the view changed). The window runs the job
    /// and hands the result to [`App::finish_relayout`]; until then the old
    /// layout stays on screen. `None` when the source lays out in place; use
    /// [`InputEvent::DataChanged`] then.
    pub fn begin_relayout(&mut self) -> Option<PendingRelayout> {
        let view = self.current_view();
        let map = self.map;
        let job = self.source.begin_relayout(view, map)?;
        let top = self.stack.len().saturating_sub(1);
        for (i, level) in self.stack.iter_mut().enumerate() {
            level.stale = i != top;
        }
        Some(PendingRelayout {
            token: self.nav_token,
            view,
            map,
            job,
        })
    }

    /// Whether a finished layout still fits the view on screen. When it does
    /// not (the user navigated or resized meanwhile) the window starts another.
    pub fn relayout_is_current(&self, done: &FinishedRelayout) -> bool {
        done.token == self.nav_token && done.view == self.current_view() && done.map == self.map
    }

    /// Shows a layout prepared by [`App::begin_relayout`]. A result that no
    /// longer fits (the user navigated or resized meanwhile) is dropped.
    pub fn finish_relayout(&mut self, done: FinishedRelayout) -> Vec<Effect> {
        let mut fx = Vec::new();
        if !self.relayout_is_current(&done) {
            return fx;
        }
        let Some(layout) = self
            .source
            .finish_relayout(done.view, done.map, done.prepared)
        else {
            return fx;
        };
        if let Some(level) = self.stack.last_mut() {
            level.layout = layout;
            level.stale = false;
        }
        self.restore_after_relayout(&mut fx);
        fx
    }

    fn skip_running_animation(&mut self, fx: &mut Vec<Effect>) {
        if self.animating {
            fx.push(Effect::SkipAnimation);
            self.animating = false;
        }
    }

    fn zoom_in_at(&mut self, x: f32, y: f32, fx: &mut Vec<Effect>) {
        if let Some((_, view, rect)) = self.child_under(x, y) {
            self.enter(view, rect, fx);
        }
    }

    fn enter(&mut self, view: ViewId, child: Rect, fx: &mut Vec<Effect>) {
        self.source.enter(view);
        let layout = self.source.layout(view, self.map);
        let items = Arc::clone(&layout.items);
        self.push_history(self.current_view());
        self.history_forward.clear();
        self.stack.push(Level {
            view,
            layout,
            stale: false,
        });
        self.prune_views();
        self.hover = None;
        self.selected.clear();
        self.anchor = None;
        self.nav_token += 1;
        fx.push(Effect::Zoom {
            direction: ZoomDirection::In,
            items,
            child,
        });
        fx.push(Effect::ViewChanged(view));
    }

    fn zoom_out(&mut self, fx: &mut Vec<Effect>) {
        if self.stack.len() < 2 {
            return;
        }
        let Some(gone) = self.stack.pop() else {
            return;
        };
        self.push_history(gone.view);
        self.history_forward.clear();
        self.refresh_top();
        self.prune_views();
        let Some(parent) = self.stack.last() else {
            return;
        };
        let child = parent
            .layout
            .folders
            .iter()
            .find(|(_, v)| *v == gone.view)
            .and_then(|(id, _)| parent.layout.items.iter().find(|i| i.id == *id))
            .map(|i| i.rect)
            .unwrap_or(self.map);
        self.hover = None;
        self.selected.clear();
        self.anchor = None;
        self.nav_token += 1;
        fx.push(Effect::Zoom {
            direction: ZoomDirection::Out,
            items: Arc::clone(&parent.layout.items),
            child,
        });
        fx.push(Effect::ViewChanged(parent.view));
    }

    fn jump_to(&mut self, view: ViewId, fx: &mut Vec<Effect>) {
        let mut chain = vec![view];
        let mut v = self.source.parent(view);
        while let Some(id) = v {
            chain.push(id);
            v = self.source.parent(id);
        }
        chain.reverse();
        self.stack = chain
            .into_iter()
            .map(|view| Level {
                view,
                layout: self.source.layout(view, self.map),
                stale: false,
            })
            .collect();
        self.after_set_view(fx);
    }

    /// Lays the top of the stack out again if it was left behind by newer
    /// data while another view was on top.
    fn refresh_top(&mut self) {
        if let Some(level) = self.stack.last_mut()
            && level.stale
        {
            level.layout = self.source.layout(level.view, self.map);
            level.stale = false;
        }
    }

    fn push_history(&mut self, view: ViewId) {
        self.history_back.push(view);
        if self.history_back.len() > HISTORY_LIMIT {
            self.history_back.remove(0);
        }
    }

    /// Tells the source which views history can still reach.
    fn prune_views(&mut self) {
        let keep: Vec<ViewId> = self
            .stack
            .iter()
            .map(|l| l.view)
            .chain(self.history_back.iter().copied())
            .chain(self.history_forward.iter().copied())
            .collect();
        self.source.prune(&keep);
    }

    fn after_set_view(&mut self, fx: &mut Vec<Effect>) {
        self.refresh_top();
        self.prune_views();
        self.hover = None;
        self.selected.clear();
        self.anchor = None;
        self.nav_token += 1;
        fx.push(Effect::SetView(self.items()));
        fx.push(Effect::ViewChanged(self.current_view()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bs_render::{Flags, ItemKind};

    /// A two-level source: the root holds two folders, each of which opens a
    /// view of its own.
    struct Fake;

    fn item(id: u32, depth: u8, rect: Rect, kind: ItemKind) -> Item {
        Item {
            rect,
            kind,
            depth,
            label: None,
            id,
            category: 0,
            flags: Flags::default(),
        }
    }

    impl ViewSource for Fake {
        fn root(&self) -> ViewId {
            0
        }
        fn layout(&mut self, view: ViewId, map: Rect) -> Layout {
            let half = map.w / 2.0;
            let mut items = vec![item(1, 0, map, ItemKind::FolderTitled)];
            let mut folders = Vec::new();
            if view == 0 {
                items.push(item(
                    2,
                    1,
                    Rect::new(map.x, map.y, half, map.h),
                    ItemKind::FolderTitled,
                ));
                items.push(item(
                    3,
                    1,
                    Rect::new(map.x + half, map.y, half, map.h),
                    ItemKind::FolderTitled,
                ));
                items.push(item(
                    4,
                    2,
                    Rect::new(map.x + 10.0, map.y + 30.0, 20.0, 20.0),
                    ItemKind::FileTile,
                ));
                folders.push((2, 10));
                folders.push((3, 20));
            }
            Layout {
                items: Arc::new(items),
                folders,
            }
        }
        fn parent(&self, view: ViewId) -> Option<ViewId> {
            (view != 0).then_some(0)
        }
    }

    fn app() -> App<Fake> {
        App::new(Fake, Rect::new(0.0, 0.0, 400.0, 200.0))
    }

    fn t(ms: u64) -> Duration {
        Duration::from_millis(ms)
    }

    #[test]
    fn hover_reports_the_topmost_box_and_only_changes() {
        let mut a = app();
        let fx = a.update(InputEvent::PointerMoved { x: 15.0, y: 40.0 }, t(0));
        assert!(matches!(fx[..], [Effect::SetHover(Some(4))]));
        assert!(
            a.update(InputEvent::PointerMoved { x: 16.0, y: 41.0 }, t(1))
                .is_empty()
        );
        let fx = a.update(InputEvent::PointerLeft, t(2));
        assert!(matches!(fx[..], [Effect::SetHover(None)]));
    }

    #[test]
    fn click_selects_and_escape_clears() {
        let mut a = app();
        a.update(InputEvent::Click { x: 300.0, y: 100.0 }, t(0));
        assert_eq!(a.selected(), Some(3));
        let fx = a.update(InputEvent::Key(Key::Escape), t(1));
        assert!(matches!(fx[..], [Effect::SetSelected(None)]));
    }

    #[test]
    fn wheel_up_zooms_toward_the_pointer_and_wheel_down_returns() {
        let mut a = app();
        let fx = a.update(
            InputEvent::Wheel {
                x: 300.0,
                y: 100.0,
                steps: 1.0,
            },
            t(0),
        );
        let Effect::Zoom {
            direction, child, ..
        } = &fx[0]
        else {
            panic!("expected a zoom, got {fx:?}");
        };
        assert_eq!(*direction, ZoomDirection::In);
        assert_eq!(*child, Rect::new(200.0, 0.0, 200.0, 200.0));
        assert_eq!(a.current_view(), 20);
        let fx = a.update(
            InputEvent::Wheel {
                x: 10.0,
                y: 10.0,
                steps: -1.0,
            },
            t(500),
        );
        let Effect::Zoom {
            direction, child, ..
        } = &fx[0]
        else {
            panic!("expected a zoom out, got {fx:?}");
        };
        assert_eq!(*direction, ZoomDirection::Out);
        assert_eq!(*child, Rect::new(200.0, 0.0, 200.0, 200.0));
        assert_eq!(a.current_view(), 0);
    }

    #[test]
    fn input_during_an_animation_skips_it() {
        let mut a = app();
        a.update(
            InputEvent::Wheel {
                x: 300.0,
                y: 100.0,
                steps: 1.0,
            },
            t(0),
        );
        a.set_animating(true);
        let fx = a.update(
            InputEvent::Wheel {
                x: 10.0,
                y: 10.0,
                steps: -1.0,
            },
            t(50),
        );
        assert!(matches!(fx[0], Effect::SkipAnimation));
        assert!(matches!(
            fx[1],
            Effect::Zoom {
                direction: ZoomDirection::Out,
                ..
            }
        ));
    }

    #[test]
    fn the_renderer_decides_whether_a_skip_is_needed() {
        let wheel_in = InputEvent::Wheel {
            x: 300.0,
            y: 100.0,
            steps: 1.0,
        };
        let wheel_out = InputEvent::Wheel {
            x: 10.0,
            y: 10.0,
            steps: -1.0,
        };
        // Long after the nominal 150 ms the renderer may still be animating
        // (it has not painted its last frame yet): the app must skip.
        let mut a = app();
        a.update(wheel_in, t(0));
        a.set_animating(true);
        let fx = a.update(wheel_out, t(400));
        assert!(matches!(fx[0], Effect::SkipAnimation));
        // When the renderer says it has finished, no skip is sent, however
        // little time has passed.
        let mut b = app();
        b.update(wheel_in, t(0));
        b.set_animating(false);
        let fx = b.update(wheel_out, t(10));
        assert!(!fx.iter().any(|e| matches!(e, Effect::SkipAnimation)));
    }

    #[test]
    fn wheel_at_the_root_does_nothing_downward() {
        let mut a = app();
        assert!(
            a.update(
                InputEvent::Wheel {
                    x: 1.0,
                    y: 1.0,
                    steps: -1.0
                },
                t(0)
            )
            .is_empty()
        );
    }

    #[test]
    fn history_walks_back_and_forward() {
        let mut a = app();
        a.update(InputEvent::DoubleClick { x: 10.0, y: 10.0 }, t(0));
        assert_eq!(a.current_view(), 10);
        a.update(InputEvent::Key(Key::AltLeft), t(1000));
        assert_eq!(a.current_view(), 0);
        a.update(InputEvent::Key(Key::AltRight), t(2000));
        assert_eq!(a.current_view(), 10);
        a.update(InputEvent::Key(Key::Home), t(3000));
        assert_eq!(a.current_view(), 0);
        assert_eq!(a.depth(), 1);
    }

    #[test]
    fn enter_opens_the_selected_folder() {
        let mut a = app();
        a.update(InputEvent::Click { x: 300.0, y: 100.0 }, t(0));
        let fx = a.update(InputEvent::Key(Key::Enter), t(1));
        assert!(matches!(
            fx[0],
            Effect::Zoom {
                direction: ZoomDirection::In,
                ..
            }
        ));
        assert_eq!(a.current_view(), 20);
    }

    #[test]
    fn resize_relays_out_the_current_view_without_restarting() {
        let mut a = app();
        a.update(InputEvent::DoubleClick { x: 10.0, y: 10.0 }, t(0));
        let fx = a.update(
            InputEvent::Resized {
                map: Rect::new(0.0, 0.0, 800.0, 400.0),
            },
            t(10),
        );
        assert!(matches!(fx[..], [Effect::SetView(_)]));
        assert_eq!(a.current_view(), 10);
        assert_eq!(a.items()[0].rect.w, 800.0);
    }
}
