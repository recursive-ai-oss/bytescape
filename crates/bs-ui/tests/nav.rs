//! Navigation through the input core: wheel, touchpad and pinch steps,
//! double click, history with selections, Up, Home, the arrow keys, Enter and
//! the clickable breadcrumb. Every test feeds the same events the window
//! does, with the clock in the event stream.

use std::sync::Arc;
use std::time::Duration;

use bs_render::{Flags, Item, ItemKind, Rect, ZoomDirection};
use bs_ui::wheel::MAX_LEVELS_PER_GESTURE;
use bs_ui::{App, Direction, Effect, InputEvent, Key, Layout, ViewId, ViewSource};

fn t(ms: u64) -> Duration {
    Duration::from_millis(ms)
}

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

const MAP: Rect = Rect {
    x: 0.0,
    y: 0.0,
    w: 300.0,
    h: 200.0,
};

/// ```text
/// view 0   three folders A (id 2), B (3), C (4) side by side. B holds a
///          subfolder (11) and two files (7, 8); A holds files 5 and 6.
/// view 10  A: a folder (50, opens view 100) and a file (51)
/// view 20  B
/// view 30  C
/// view 40  B's subfolder, entered directly from the root
/// view 100 reached from view 10 first and from view 20 as well: it keeps
///          its first parent, 10
/// ```
struct Tree {
    /// Views that disappeared (a source may forget views it pruned).
    gone: Vec<ViewId>,
    /// Boxes that disappeared from every layout.
    missing: Vec<u32>,
}

impl ViewSource for Tree {
    fn root(&self) -> ViewId {
        0
    }

    fn layout(&mut self, view: ViewId, map: Rect) -> Layout {
        if self.gone.contains(&view) {
            return Layout {
                items: Arc::new(Vec::new()),
                folders: Vec::new(),
            };
        }
        let mut items = vec![item(1, 0, map, ItemKind::FolderTitled)];
        let mut folders = Vec::new();
        match view {
            0 => {
                for (n, id) in [2u32, 3, 4].into_iter().enumerate() {
                    items.push(item(
                        id,
                        1,
                        Rect::new(map.x + n as f32 * 100.0, map.y, 100.0, map.h),
                        ItemKind::FolderTitled,
                    ));
                }
                let file = |id, x, y| item(id, 2, Rect::new(x, y, 40.0, 40.0), ItemKind::FileTile);
                items.push(file(5, 5.0, 30.0));
                items.push(file(6, 50.0, 30.0));
                items.push(file(7, 105.0, 30.0));
                items.push(file(8, 150.0, 30.0));
                items.push(item(
                    11,
                    2,
                    Rect::new(105.0, 100.0, 80.0, 60.0),
                    ItemKind::FolderTitled,
                ));
                folders.extend([(2, 10), (3, 20), (4, 30), (11, 40)]);
            }
            10 => {
                items.push(item(
                    50,
                    1,
                    Rect::new(map.x, map.y + 20.0, 150.0, 100.0),
                    ItemKind::FolderTitled,
                ));
                items.push(item(
                    51,
                    1,
                    Rect::new(map.x + 160.0, map.y + 20.0, 60.0, 60.0),
                    ItemKind::FileTile,
                ));
                folders.push((50, 100));
            }
            20 => {
                items.push(item(
                    60,
                    1,
                    Rect::new(map.x, map.y + 20.0, 150.0, 100.0),
                    ItemKind::FolderTitled,
                ));
                folders.push((60, 100));
            }
            _ => {}
        }
        items.retain(|i| !self.missing.contains(&i.id));
        Layout {
            items: Arc::new(items),
            folders,
        }
    }

    fn parent(&self, view: ViewId) -> Option<ViewId> {
        match view {
            0 => None,
            100 => Some(10),
            40 | 10 | 20 | 30 => Some(0),
            _ => None,
        }
    }

    fn trail(&self, view: ViewId) -> Vec<String> {
        match view {
            0 => vec!["top".into()],
            100 => vec!["top".into(), "A".into(), "inner".into()],
            v => vec!["top".into(), format!("view{v}")],
        }
    }

    fn open_trail_segment(&mut self, view: ViewId, index: usize) -> Option<ViewId> {
        match (view, index) {
            (_, 0) => Some(0),
            (100, 1) => Some(10),
            // A folder with no view on the user's path.
            (40, 5) => Some(10),
            _ => None,
        }
    }
}

fn app() -> App<Tree> {
    App::new(
        Tree {
            gone: Vec::new(),
            missing: Vec::new(),
        },
        MAP,
    )
}

/// A point on a folder's title area, where the folder itself is topmost.
const A_TITLE: (f32, f32) = (50.0, 10.0);
const B_TITLE: (f32, f32) = (150.0, 10.0);

fn click(a: &mut App<Tree>, p: (f32, f32), at: u64) -> Vec<Effect> {
    a.update(InputEvent::Click { x: p.0, y: p.1 }, t(at))
}

fn zooms(fx: &[Effect]) -> Vec<ZoomDirection> {
    fx.iter()
        .filter_map(|e| match e {
            Effect::Zoom { direction, .. } => Some(*direction),
            _ => None,
        })
        .collect()
}

// ---- wheel, touchpad, pinch ------------------------------------------------

#[test]
fn a_wheel_notch_goes_one_level_toward_the_pointer() {
    let mut a = app();
    let fx = a.update(
        InputEvent::Wheel {
            x: B_TITLE.0,
            y: B_TITLE.1,
            steps: 1.0,
        },
        t(0),
    );
    assert_eq!(zooms(&fx), [ZoomDirection::In]);
    assert_eq!(a.current_view(), 20);
    // The pointer was over B, whatever is deeper below it.
    let fx = a.update(
        InputEvent::Wheel {
            x: 10.0,
            y: 10.0,
            steps: -1.0,
        },
        t(20),
    );
    assert_eq!(zooms(&fx), [ZoomDirection::Out]);
    assert_eq!(a.current_view(), 0);
}

#[test]
fn notches_inside_the_lockout_do_not_stack_up() {
    let mut a = app();
    // Three notches within 100 ms: one level.
    for ms in [0, 40, 80] {
        a.update(
            InputEvent::Wheel {
                x: A_TITLE.0,
                y: A_TITLE.1,
                steps: 1.0,
            },
            t(ms),
        );
    }
    assert_eq!(a.depth(), 2);
    assert_eq!(a.current_view(), 10);
}

/// A source whose every view holds one folder, so there is always a level
/// deeper.
struct Deep;

impl ViewSource for Deep {
    fn root(&self) -> ViewId {
        0
    }

    fn layout(&mut self, view: ViewId, map: Rect) -> Layout {
        let inner = Rect::new(map.x + 2.0, map.y + 2.0, map.w - 4.0, map.h - 4.0);
        Layout {
            items: Arc::new(vec![
                item(1, 0, map, ItemKind::FolderTitled),
                item(2, 1, inner, ItemKind::FolderTitled),
            ]),
            folders: vec![(2, view * 10 + 1)],
        }
    }

    fn parent(&self, view: ViewId) -> Option<ViewId> {
        (view > 0).then(|| (view - 1) / 10)
    }
}

fn deep() -> App<Deep> {
    App::new(Deep, MAP)
}

const MID: (f32, f32) = (150.0, 100.0);

/// A touchpad fling: `n` events 8 ms apart, each worth `steps`.
fn fling(a: &mut App<Deep>, start: u64, n: u64, steps: f32) -> usize {
    let mut levels = 0;
    for i in 0..n {
        let fx = a.update(
            InputEvent::Touchpad {
                x: MID.0,
                y: MID.1,
                steps,
            },
            t(start + i * 8),
        );
        levels += zooms(&fx).len();
    }
    levels
}

#[test]
fn a_touchpad_fling_moves_a_bounded_number_of_levels() {
    let mut a = deep();
    let levels = fling(&mut a, 0, 150, 0.4);
    assert_eq!(levels, MAX_LEVELS_PER_GESTURE as usize);
    assert_eq!(a.depth(), 1 + MAX_LEVELS_PER_GESTURE as usize);
    // After a pause the next gesture moves again, and coming back out is as
    // bounded as going in.
    let more = fling(&mut a, 1_600, 150, 0.4);
    assert_eq!(more, 3);
    let out = fling(&mut a, 3_000, 150, -0.4);
    assert_eq!(out, 3);
    assert_eq!(a.depth(), 1 + 6 - 3);
}

#[test]
fn small_touchpad_deltas_add_up_and_a_tiny_drag_does_nothing() {
    let mut a = deep();
    // 0.1 per event: the tenth event completes a step.
    let mut at = None;
    for i in 0..9u64 {
        let fx = a.update(
            InputEvent::Touchpad {
                x: MID.0,
                y: MID.1,
                steps: 0.12,
            },
            t(i * 10),
        );
        if !zooms(&fx).is_empty() {
            at = Some(i);
            break;
        }
    }
    assert_eq!(at, Some(8));
    let mut b = deep();
    let fx = b.update(
        InputEvent::Touchpad {
            x: MID.0,
            y: MID.1,
            steps: 0.3,
        },
        t(0),
    );
    assert!(fx.is_empty());
    assert_eq!(b.depth(), 1);
}

#[test]
fn a_mouse_wheel_can_go_as_deep_as_the_user_likes() {
    let mut a = deep();
    for i in 0..6u64 {
        a.update(
            InputEvent::Wheel {
                x: MID.0,
                y: MID.1,
                steps: 1.0,
            },
            t(i * 200),
        );
    }
    assert_eq!(a.depth(), 7);
}

#[test]
fn pinch_out_goes_deeper_and_pinch_in_comes_back() {
    let mut a = deep();
    let pinch = |a: &mut App<Deep>, delta: f32, at: u64| {
        a.update(
            InputEvent::Pinch {
                x: MID.0,
                y: MID.1,
                delta,
            },
            t(at),
        )
    };
    assert!(pinch(&mut a, 0.1, 0).is_empty());
    assert!(pinch(&mut a, 0.1, 16).is_empty());
    let fx = pinch(&mut a, 0.1, 32);
    assert_eq!(zooms(&fx), [ZoomDirection::In]);
    assert_eq!(a.depth(), 2);
    // One big pinch in is one level up.
    let fx = pinch(&mut a, -0.6, 400);
    assert_eq!(zooms(&fx), [ZoomDirection::Out]);
    assert_eq!(a.depth(), 1);
    // A long pinch is bounded like a fling.
    let mut levels = 0;
    for i in 0..60u64 {
        levels += zooms(&pinch(&mut a, 0.1, 1_000 + i * 16)).len();
    }
    assert_eq!(levels, 3);
}

#[test]
fn input_that_arrives_during_a_zoom_skips_it_first() {
    let mut a = deep();
    a.update(
        InputEvent::Wheel {
            x: MID.0,
            y: MID.1,
            steps: 1.0,
        },
        t(0),
    );
    a.set_animating(true);
    let fx = a.update(
        InputEvent::Wheel {
            x: MID.0,
            y: MID.1,
            steps: -1.0,
        },
        t(30),
    );
    assert!(matches!(fx[0], Effect::SkipAnimation));
    assert_eq!(zooms(&fx), [ZoomDirection::Out]);
}

// ---- double click and Enter -----------------------------------------------

#[test]
fn double_click_enters_the_folder_under_the_pointer() {
    let mut a = app();
    let fx = a.update(
        InputEvent::DoubleClick {
            x: B_TITLE.0,
            y: B_TITLE.1,
        },
        t(0),
    );
    assert_eq!(zooms(&fx), [ZoomDirection::In]);
    assert_eq!(a.current_view(), 20);
}

#[test]
fn double_click_jumps_straight_into_a_nested_folder() {
    let mut a = app();
    // The subfolder of B: two levels below the top, one stack level.
    let fx = a.update(InputEvent::DoubleClick { x: 150.0, y: 105.0 }, t(0));
    assert_eq!(zooms(&fx), [ZoomDirection::In]);
    assert_eq!(a.current_view(), 40);
    assert_eq!(a.depth(), 2);
}

#[test]
fn double_click_on_a_file_selects_it_asks_for_the_card_and_never_opens_it() {
    let mut a = app();
    let fx = a.update(InputEvent::DoubleClick { x: 20.0, y: 45.0 }, t(0));
    assert!(zooms(&fx).is_empty());
    assert!(fx.iter().any(|e| matches!(e, Effect::SetSelected(Some(5)))));
    assert_eq!(a.selected(), Some(5));
    assert_eq!(a.take_card_request(), Some(5));
    // The request is taken once.
    assert_eq!(a.take_card_request(), None);
    assert_eq!(a.current_view(), 0);
}

#[test]
fn enter_goes_into_a_folder_and_shows_the_card_of_a_file() {
    let mut a = app();
    click(&mut a, B_TITLE, 0);
    let fx = a.update(InputEvent::Key(Key::Enter), t(1));
    assert_eq!(zooms(&fx), [ZoomDirection::In]);
    assert_eq!(a.current_view(), 20);

    let mut b = app();
    click(&mut b, (20.0, 45.0), 0);
    let fx = b.update(InputEvent::Key(Key::Enter), t(1));
    assert!(zooms(&fx).is_empty());
    assert_eq!(b.take_card_request(), Some(5));
    // Nothing selected: Enter does nothing.
    let mut c = app();
    assert!(c.update(InputEvent::Key(Key::Enter), t(0)).is_empty());
}

// ---- history, Up, Home ------------------------------------------------------

#[test]
fn back_and_forward_keep_the_selection_of_each_view() {
    let mut a = app();
    click(&mut a, B_TITLE, 0);
    assert_eq!(a.selected(), Some(3));
    a.update(InputEvent::Key(Key::Enter), t(1));
    assert_eq!(a.current_view(), 20);
    // Select something inside B's view, then go back.
    click(&mut a, (10.0, 40.0), 500);
    assert_eq!(a.selected(), Some(60));
    let fx = a.update(InputEvent::Key(Key::AltLeft), t(1_000));
    assert_eq!(a.current_view(), 0);
    assert_eq!(a.selection(), [3]);
    assert!(
        fx.iter().any(|e| matches!(e, Effect::SetSelected(Some(3)))),
        "{fx:?}"
    );
    // Forward returns to B with the box that was selected there.
    a.update(InputEvent::Key(Key::AltRight), t(2_000));
    assert_eq!(a.current_view(), 20);
    assert_eq!(a.selection(), [60]);
}

#[test]
fn a_selection_whose_box_is_gone_is_not_restored() {
    let mut a = app();
    click(&mut a, A_TITLE, 0);
    a.update(InputEvent::Key(Key::Enter), t(1));
    assert_eq!(a.current_view(), 10);
    click(&mut a, (200.0, 60.0), 600);
    assert_eq!(a.selected(), Some(51));
    a.update(InputEvent::Key(Key::Home), t(1_200));
    // Back with the box still there gives the selection back...
    a.update(InputEvent::Key(Key::AltLeft), t(1_300));
    assert_eq!(a.selection(), [51]);
    a.update(InputEvent::Key(Key::Home), t(1_900));
    // ...and without it, restores nothing and reports nothing.
    a.source_mut().missing.push(51);
    let fx = a.update(InputEvent::Key(Key::AltLeft), t(2_000));
    assert_eq!(a.current_view(), 10);
    assert!(a.selection().is_empty());
    assert!(
        !fx.iter()
            .any(|e| matches!(e, Effect::SetSelected(Some(_)) | Effect::SetSelection(_))),
        "{fx:?}"
    );
}

#[test]
fn mouse_history_buttons_are_the_same_keys() {
    let mut a = app();
    a.update(
        InputEvent::DoubleClick {
            x: A_TITLE.0,
            y: A_TITLE.1,
        },
        t(0),
    );
    assert!(a.can_go_back() && !a.can_go_forward());
    a.update(InputEvent::Key(Key::AltLeft), t(500));
    assert!(!a.can_go_back() && a.can_go_forward());
    a.update(InputEvent::Key(Key::AltRight), t(900));
    assert_eq!(a.current_view(), 10);
}

#[test]
fn up_selects_the_folder_the_user_came_out_of() {
    let mut a = app();
    a.update(
        InputEvent::DoubleClick {
            x: B_TITLE.0,
            y: B_TITLE.1,
        },
        t(0),
    );
    let fx = a.update(InputEvent::Key(Key::Backspace), t(500));
    assert_eq!(zooms(&fx), [ZoomDirection::Out]);
    assert_eq!(a.selection(), [3]);
    // The zoom comes first and the selection after it, so the renderer
    // keeps the selection the zoom would otherwise drop.
    let zoom_at = fx.iter().position(|e| matches!(e, Effect::Zoom { .. }));
    let sel_at = fx
        .iter()
        .position(|e| matches!(e, Effect::SetSelected(Some(3))));
    assert!(zoom_at < sel_at, "{fx:?}");
    // Enter goes straight back in.
    let fx = a.update(InputEvent::Key(Key::Enter), t(900));
    assert_eq!(zooms(&fx), [ZoomDirection::In]);
    assert_eq!(a.current_view(), 20);
}

#[test]
fn alt_up_is_up_and_up_at_the_top_does_nothing() {
    let mut a = app();
    a.update(
        InputEvent::DoubleClick {
            x: A_TITLE.0,
            y: A_TITLE.1,
        },
        t(0),
    );
    a.update(InputEvent::Key(Key::AltUp), t(500));
    assert_eq!(a.current_view(), 0);
    assert!(a.update(InputEvent::Key(Key::AltUp), t(900)).is_empty());
    assert!(a.update(InputEvent::Key(Key::Backspace), t(900)).is_empty());
}

#[test]
fn home_goes_to_the_top_and_back_returns_to_where_the_user_was() {
    let mut a = app();
    a.update(
        InputEvent::DoubleClick {
            x: A_TITLE.0,
            y: A_TITLE.1,
        },
        t(0),
    );
    a.update(InputEvent::DoubleClick { x: 20.0, y: 40.0 }, t(600));
    assert_eq!(a.current_view(), 100);
    assert_eq!(a.depth(), 3);
    a.update(InputEvent::Key(Key::Home), t(1_200));
    assert_eq!(a.depth(), 1);
    a.update(InputEvent::Key(Key::AltLeft), t(1_300));
    assert_eq!(a.current_view(), 100);
}

#[test]
fn a_view_reached_through_history_goes_up_to_its_first_parent() {
    // View 100 is offered by views 10 and 20 and belongs to 10. Entered
    // through 20, Up returns to 20, the way the user came.
    let mut a = app();
    a.update(
        InputEvent::DoubleClick {
            x: B_TITLE.0,
            y: B_TITLE.1,
        },
        t(0),
    );
    a.update(InputEvent::DoubleClick { x: 20.0, y: 40.0 }, t(600));
    assert_eq!(a.current_view(), 100);
    a.update(InputEvent::Key(Key::AltUp), t(1_200));
    assert_eq!(a.current_view(), 20);
    a.update(InputEvent::DoubleClick { x: 20.0, y: 40.0 }, t(1_800));
    assert_eq!(a.current_view(), 100);
    // After Home and Back it is rebuilt from the source's parent chain, so
    // its stack is the one the source reports, and Up goes to 10.
    a.update(InputEvent::Key(Key::Home), t(2_400));
    a.update(InputEvent::Key(Key::AltLeft), t(2_500));
    assert_eq!(a.current_view(), 100);
    assert_eq!(a.depth(), 3);
    a.update(InputEvent::Key(Key::AltUp), t(3_000));
    assert_eq!(a.current_view(), 10);
}

#[test]
fn a_view_that_no_longer_exists_in_history_leaves_the_top_in_place() {
    let mut a = app();
    a.update(
        InputEvent::DoubleClick {
            x: A_TITLE.0,
            y: A_TITLE.1,
        },
        t(0),
    );
    a.update(InputEvent::Key(Key::Home), t(500));
    a.source_mut().gone.push(10);
    a.update(InputEvent::Key(Key::AltLeft), t(600));
    assert_eq!(a.current_view(), 0);
    assert_eq!(a.depth(), 1);
    assert!(!a.items().is_empty());
}

#[test]
fn history_is_bounded() {
    let mut a = app();
    for i in 0..400u64 {
        a.update(
            InputEvent::DoubleClick {
                x: A_TITLE.0,
                y: A_TITLE.1,
            },
            t(i * 1000),
        );
        a.update(InputEvent::Key(Key::Backspace), t(i * 1000 + 500));
    }
    assert!(a.history().len() <= 256);
}

// ---- arrow keys -------------------------------------------------------------

#[test]
fn arrows_start_at_the_first_box_and_walk_across_the_row() {
    let mut a = app();
    let fx = a.update(InputEvent::Key(Key::Arrow(Direction::Right)), t(0));
    assert!(fx.iter().any(|e| matches!(e, Effect::SetSelected(Some(2)))));
    a.update(InputEvent::Key(Key::Arrow(Direction::Right)), t(1));
    assert_eq!(a.selected(), Some(3));
    a.update(InputEvent::Key(Key::Arrow(Direction::Right)), t(2));
    assert_eq!(a.selected(), Some(4));
    // At the edge nothing happens, and nothing is reported.
    assert!(
        a.update(InputEvent::Key(Key::Arrow(Direction::Right)), t(3))
            .is_empty()
    );
    a.update(InputEvent::Key(Key::Arrow(Direction::Left)), t(4));
    assert_eq!(a.selected(), Some(3));
    assert!(
        a.update(InputEvent::Key(Key::Arrow(Direction::Up)), t(5))
            .is_empty()
    );
}

#[test]
fn arrows_among_files_stay_at_the_files_depth() {
    let mut a = app();
    click(&mut a, (20.0, 45.0), 0);
    assert_eq!(a.selected(), Some(5));
    a.update(InputEvent::Key(Key::Arrow(Direction::Right)), t(1));
    assert_eq!(a.selected(), Some(6));
    // Past A's last file the next one is B's.
    a.update(InputEvent::Key(Key::Arrow(Direction::Right)), t(2));
    assert_eq!(a.selected(), Some(7));
}

#[test]
fn escape_clears_the_selection() {
    let mut a = app();
    click(&mut a, B_TITLE, 0);
    let fx = a.update(InputEvent::Key(Key::Escape), t(1));
    assert!(matches!(fx[..], [Effect::SetSelected(None)]));
    assert!(a.update(InputEvent::Key(Key::Escape), t(2)).is_empty());
}

// ---- breadcrumb -------------------------------------------------------------

#[test]
fn clicking_a_breadcrumb_segment_goes_up_to_it() {
    let mut a = app();
    a.update(
        InputEvent::DoubleClick {
            x: A_TITLE.0,
            y: A_TITLE.1,
        },
        t(0),
    );
    a.update(InputEvent::DoubleClick { x: 20.0, y: 40.0 }, t(600));
    assert_eq!(a.depth(), 3);
    // The middle segment is on the stack: one animated step up.
    let fx = a.update(InputEvent::CrumbClick { index: 1 }, t(1_200));
    assert_eq!(zooms(&fx), [ZoomDirection::Out]);
    assert_eq!(a.current_view(), 10);
    assert_eq!(a.depth(), 2);
    // The first segment is the top.
    let fx = a.update(InputEvent::CrumbClick { index: 0 }, t(1_800));
    assert_eq!(zooms(&fx), [ZoomDirection::Out]);
    assert_eq!(a.current_view(), 0);
    // Back returns to where the click came from.
    a.update(InputEvent::Key(Key::AltLeft), t(2_400));
    assert_eq!(a.current_view(), 10);
}

#[test]
fn a_breadcrumb_segment_the_source_cannot_open_does_nothing() {
    let mut a = app();
    a.update(
        InputEvent::DoubleClick {
            x: B_TITLE.0,
            y: B_TITLE.1,
        },
        t(0),
    );
    assert!(
        a.update(InputEvent::CrumbClick { index: 7 }, t(600))
            .is_empty()
    );
    assert_eq!(a.current_view(), 20);
}

#[test]
fn a_breadcrumb_segment_off_the_stack_jumps_and_back_returns() {
    // The user entered B's subfolder directly (stack: top, view 40); the
    // source can open a segment, view 10, that is not on that stack.
    let mut a = app();
    a.update(InputEvent::DoubleClick { x: 150.0, y: 105.0 }, t(0));
    assert_eq!(a.current_view(), 40);
    let fx = a.update(InputEvent::CrumbClick { index: 5 }, t(600));
    assert!(zooms(&fx).is_empty(), "a jump is not an animated step");
    assert!(fx.iter().any(|e| matches!(e, Effect::SetView(_))));
    assert_eq!(a.current_view(), 10);
    assert_eq!(a.depth(), 2);
    a.update(InputEvent::Key(Key::AltLeft), t(1_200));
    assert_eq!(a.current_view(), 40);
}

// ---- layout bookkeeping ------------------------------------------------------

#[test]
fn the_generation_changes_whenever_the_layout_is_replaced() {
    let mut a = app();
    let g0 = a.generation();
    a.update(InputEvent::DataChanged, t(0));
    let g1 = a.generation();
    assert!(g1 > g0);
    a.update(InputEvent::Key(Key::Refresh), t(1));
    let g2 = a.generation();
    assert!(g2 > g1);
    a.update(
        InputEvent::DoubleClick {
            x: A_TITLE.0,
            y: A_TITLE.1,
        },
        t(2),
    );
    assert!(a.generation() > g2);
    // Hovering changes nothing about the layout.
    let g = a.generation();
    a.update(InputEvent::PointerMoved { x: 10.0, y: 10.0 }, t(3));
    assert_eq!(a.generation(), g);
}

#[test]
fn hover_on_twenty_thousand_boxes_stays_cheap() {
    struct Many;
    impl ViewSource for Many {
        fn root(&self) -> ViewId {
            0
        }
        fn layout(&mut self, _view: ViewId, map: Rect) -> Layout {
            let mut items = vec![item(1, 0, map, ItemKind::FolderTitled)];
            for i in 0..20_000u32 {
                let (col, row) = (i % 200, i / 200);
                items.push(item(
                    2 + i,
                    1,
                    Rect::new(col as f32 * 19.0, row as f32 * 10.0, 18.0, 9.0),
                    ItemKind::FileTile,
                ));
            }
            Layout {
                items: Arc::new(items),
                folders: Vec::new(),
            }
        }
        fn parent(&self, _view: ViewId) -> Option<ViewId> {
            None
        }
    }
    let mut a = App::new(Many, Rect::new(0.0, 0.0, 3800.0, 1000.0));
    let start = std::time::Instant::now();
    for i in 0..2_000u32 {
        a.update(
            InputEvent::PointerMoved {
                x: (i % 3700) as f32,
                y: (i % 990) as f32,
            },
            t(u64::from(i)),
        );
    }
    let per_move = start.elapsed() / 2_000;
    // A debug build on a busy machine is the slow case; the point is that a
    // move over twenty thousand boxes is microseconds, not frames.
    assert!(per_move < Duration::from_millis(5), "{per_move:?}");
}
