//! The app state machine driving the real renderer, the way the window does.
//! These cover input and theme changes that arrive while a zoom is running.

use std::sync::Arc;
use std::time::Duration;

use bs_render::{
    Flags, Item, ItemKind, Palette, Rect, Renderer, TextEngine, ZOOM_DURATION, ZoomDirection,
};
use bs_ui::{App, Effect, InputEvent, Key, Layout, ViewId, ViewSource};

const W: u32 = 320;
const H: u32 = 200;

struct Fake;

fn item(id: u32, depth: u8, rect: Rect) -> Item {
    Item {
        rect,
        kind: ItemKind::FolderTitled,
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
        let mut items = vec![item(1, 0, map)];
        let mut folders = Vec::new();
        if view == 0 {
            items.push(item(
                2,
                1,
                Rect::new(map.x, map.y + 20.0, map.w / 2.0, map.h - 20.0),
            ));
            folders.push((2, 10));
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

/// What the window does with an event's effects.
fn apply(r: &mut Renderer, fx: Vec<Effect>, now: Duration) {
    for e in fx {
        match e {
            Effect::SetHover(id) => r.set_hover(id),
            Effect::SetSelected(id) => r.set_selected(id),
            Effect::SetView(items) => r.set_items(items),
            Effect::Zoom {
                direction,
                items,
                child,
            } => r.begin_zoom(direction, items, child, now),
            Effect::SkipAnimation => r.finish_animation(),
            Effect::ViewChanged(_) => {}
        }
    }
}

/// Feeds one event the way the window does: ask the renderer first.
fn feed(app: &mut App<Fake>, r: &mut Renderer, event: InputEvent, now: Duration) {
    app.set_animating(r.is_animating());
    let fx = app.update(event, now);
    apply(r, fx, now);
}

fn setup() -> (App<Fake>, Renderer, Vec<u32>) {
    let map = Rect::new(0.0, 0.0, W as f32, H as f32);
    let app = App::new(Fake, map);
    let mut r =
        Renderer::new(W, H, 1.0, 13.0, Palette::light(), TextEngine::new()).expect("renderer");
    r.set_threads(1);
    r.set_items(app.items());
    let mut t = vec![0u32; (W * H) as usize];
    r.paint(&mut t, Duration::ZERO, true);
    (app, r, t)
}

fn ms(v: u64) -> Duration {
    Duration::from_millis(v)
}

const WHEEL_IN: InputEvent = InputEvent::Wheel {
    x: 10.0,
    y: 50.0,
    steps: 1.0,
};
const WHEEL_OUT: InputEvent = InputEvent::Wheel {
    x: 10.0,
    y: 50.0,
    steps: -1.0,
};

fn assert_is_picture(t: &[u32]) {
    assert!(t.iter().all(|&p| p >> 24 == 0xFF), "every pixel is opaque");
}

#[test]
fn input_after_the_nominal_end_but_before_the_last_frame() {
    let (mut app, mut r, mut t) = setup();
    feed(&mut app, &mut r, WHEEL_IN, ms(0));
    assert!(r.paint(&mut t, ms(140), false).animating);
    // 150 ms have passed on the clock, but the renderer has not painted a
    // frame at or past the end yet.
    feed(&mut app, &mut r, WHEEL_OUT, ms(160));
    r.paint(&mut t, ms(170), false);
    r.paint(&mut t, ms(400), false);
    assert_is_picture(&t);
    assert_eq!(app.current_view(), 0);
}

#[test]
fn home_and_history_keys_after_the_nominal_end() {
    for key in [Key::Home, Key::AltLeft, Key::Backspace, Key::AltUp] {
        let (mut app, mut r, mut t) = setup();
        feed(&mut app, &mut r, WHEEL_IN, ms(0));
        assert!(r.paint(&mut t, ms(140), false).animating);
        feed(&mut app, &mut r, InputEvent::Key(key), ms(160));
        r.paint(&mut t, ms(170), false);
        r.paint(&mut t, ms(400), false);
        assert_is_picture(&t);
    }
}

#[test]
fn theme_change_during_a_zoom() {
    let (mut app, mut r, mut t) = setup();
    feed(&mut app, &mut r, WHEEL_IN, ms(0));
    assert!(r.paint(&mut t, ms(50), false).animating);
    r.set_palette(Palette::dark());
    assert!(!r.is_animating(), "a theme change ends the animation");
    let painted = r.paint(&mut t, ms(60), false);
    assert!(!painted.animating);
    assert_is_picture(&t);
    let mut want = Renderer::new(W, H, 1.0, 13.0, Palette::dark(), TextEngine::new()).expect("r");
    want.set_threads(1);
    want.set_items(app.items());
    let mut w = vec![0u32; (W * H) as usize];
    want.paint(&mut w, Duration::ZERO, true);
    assert_eq!(t, w, "the new view is shown in the new palette");
}

#[test]
fn set_items_during_a_zoom() {
    let (mut app, mut r, mut t) = setup();
    feed(&mut app, &mut r, WHEEL_IN, ms(0));
    assert!(r.paint(&mut t, ms(50), false).animating);
    let items = app.items();
    r.set_items(items);
    assert!(!r.is_animating());
    r.paint(&mut t, ms(60), false);
    assert_is_picture(&t);
}

#[test]
fn a_zoom_that_starts_during_a_zoom_runs_from_a_real_picture() {
    let (_app, mut r, mut t) = setup();
    let a = Arc::new(vec![item(1, 0, Rect::new(0.0, 0.0, W as f32, H as f32))]);
    r.begin_zoom(
        ZoomDirection::In,
        a.clone(),
        Rect::new(10.0, 10.0, 100.0, 80.0),
        ms(0),
    );
    assert!(r.paint(&mut t, ms(20), false).animating);
    // No skip first: the renderer copes by itself.
    r.begin_zoom(
        ZoomDirection::Out,
        a,
        Rect::new(10.0, 10.0, 100.0, 80.0),
        ms(30),
    );
    assert!(r.paint(&mut t, ms(40), false).animating);
    r.paint(&mut t, ms(30) + ZOOM_DURATION * 2, false);
    assert!(!r.is_animating());
    assert_is_picture(&t);
}

#[test]
fn resize_during_a_zoom() {
    let (mut app, mut r, mut t) = setup();
    feed(&mut app, &mut r, WHEEL_IN, ms(0));
    assert!(r.paint(&mut t, ms(50), false).animating);
    r.resize(640, 400, 2.0);
    r.set_items(app.items());
    let mut big = vec![0u32; 640 * 400];
    let painted = r.paint(&mut big, ms(60), false);
    assert!(!painted.animating);
    assert_is_picture(&big);
}

#[test]
fn hover_and_selection_set_during_a_zoom_appear_when_it_ends() {
    let (mut app, mut r, mut t) = setup();
    feed(&mut app, &mut r, WHEEL_IN, ms(0));
    assert_eq!(app.current_view(), 10);
    let items = app.items();
    let target = items.last().map(|i| i.id).expect("an item");
    r.set_hover(Some(target));
    r.set_selected(Some(target));
    assert!(r.paint(&mut t, ms(20), false).animating);
    r.paint(&mut t, ms(300), false);

    let mut want =
        Renderer::new(W, H, 1.0, 13.0, Palette::light(), TextEngine::new()).expect("renderer");
    want.set_threads(1);
    want.set_items(items);
    want.set_hover(Some(target));
    want.set_selected(Some(target));
    let mut w = vec![0u32; (W * H) as usize];
    want.paint(&mut w, Duration::ZERO, true);
    assert_eq!(t, w, "the outline is on screen after the zoom");
}

#[test]
fn degenerate_zoom_children_do_not_panic() {
    let (_app, mut r, mut t) = setup();
    let items = Arc::new(vec![item(1, 0, Rect::new(0.0, 0.0, W as f32, H as f32))]);
    for child in [
        Rect::new(10.0, 10.0, 0.0, 0.0),
        Rect::new(f32::NAN, 0.0, 5.0, 5.0),
        Rect::new(-1e9, -1e9, 1e12, 1e12),
    ] {
        for dir in [ZoomDirection::In, ZoomDirection::Out] {
            r.begin_zoom(dir, items.clone(), child, ms(1000));
            for k in 0..12 {
                r.paint(&mut t, ms(1000 + k * 15), false);
            }
        }
    }
    assert_is_picture(&t);
}
