//! The window shell: chrome events, the info bar, the legend hover, the link
//! slot and layout work off the UI thread, all through the same input events
//! the window feeds in.

use std::sync::Arc;
use std::time::Duration;

use bs_layout::Metrics;
use bs_platform::drives::{Capacity, ReservedKind};
use bs_render::{Item, ItemKind, Rect, SegmentKind, TextEngine, Widget};
use bs_tree::{Aggregator, Batch, Category, DirKey, Meta, NodeKind, Snapshot};
use bs_ui::capacity::Breakdown;
use bs_ui::chrome::{LINK_TEXT, LINK_TEXT_SHORT, LINK_URL, TextMeasure};
use bs_ui::shell::{Look, Shell, ShellEffect};
use bs_ui::tree::TreeSource;
use bs_ui::{App, Effect, InputEvent, Key, Modifiers};

struct Fixed(f32);

impl TextMeasure for Fixed {
    fn width(&self, text: &str) -> Option<f32> {
        Some(text.chars().count() as f32 * self.0)
    }
}

fn meta(size: u64) -> Meta {
    Meta {
        logical: size,
        allocated: size,
        category: Category(1),
        ..Meta::default()
    }
}

/// root `/data` with `dirs` folders of `files` files each; `finish` marks every
/// listing done, otherwise the scan looks like it is still running.
fn snapshot(dirs: u32, files: u32, finish: bool, name_of_first: Option<&[u8]>) -> Arc<Snapshot> {
    let mut agg = Aggregator::new();
    let mut b = Batch::new();
    b.dir(DirKey(0), DirKey::NONE, b"/data", &meta(4096))
        .unwrap();
    for d in 1..=dirs {
        let name = match (d, name_of_first) {
            (d, Some(n)) if d == dirs => n.to_vec(),
            _ => format!("folder{d}").into_bytes(),
        };
        b.dir(DirKey(d), DirKey(0), &name, &meta(4096)).unwrap();
        for f in 0..files {
            let size = 1_000_000 * u64::from(d) + u64::from(f);
            b.file(
                NodeKind::File,
                DirKey(d),
                format!("file{f}").as_bytes(),
                &meta(size),
            )
            .unwrap();
        }
        if finish {
            b.listing_done(DirKey(d));
        }
    }
    if finish {
        b.listing_done(DirKey(0));
    }
    agg.apply(&b);
    agg.publish()
}

const SIZE: (u32, u32) = (1200, 800);

fn shell_for(snap: Arc<Snapshot>, scale: f32) -> Shell<TreeSource> {
    let size = (
        (SIZE.0 as f32 * scale) as u32,
        (SIZE.1 as f32 * scale) as u32,
    );
    let em = 13.0 * scale;
    let mut shell: Shell<TreeSource> = Shell::new(
        size,
        scale,
        13.0,
        TextEngine::new(),
        Look {
            dark: false,
            accent: None,
        },
    )
    .with_measure(Box::new(Fixed(em * 0.56)));
    let map = shell.map_rect();
    let source = TreeSource::new(snap, Metrics::from_logical(13.0, scale)).expect("root");
    shell.set_app(App::new(source, map));
    shell
}

fn t(ms: u64) -> Duration {
    Duration::from_millis(ms)
}

fn center(r: Rect) -> (f32, f32) {
    (r.x + r.w / 2.0, r.y + r.h / 2.0)
}

fn folder_item(shell: &Shell<TreeSource>, nth: usize) -> Item {
    shell
        .app()
        .expect("app")
        .items()
        .iter()
        .filter(|i| i.depth == 1 && i.kind == ItemKind::FolderTitled)
        .nth(nth)
        .expect("a nested folder")
        .clone()
}

/// A point on a folder's title bar, where the folder itself is under the
/// pointer rather than a file inside it.
fn title_point(r: Rect) -> (f32, f32) {
    (r.x + r.w / 2.0, r.y + 3.0)
}

fn move_to(shell: &mut Shell<TreeSource>, p: (f32, f32), at: u64) -> Vec<ShellEffect> {
    shell.update(InputEvent::PointerMoved { x: p.0, y: p.1 }, t(at))
}

fn texts(shell: &mut Shell<TreeSource>) -> Vec<String> {
    shell
        .chrome(t(0))
        .widgets
        .iter()
        .filter_map(|w| match w {
            Widget::Text { text, .. } => Some(text.to_string()),
            _ => None,
        })
        .collect()
}

#[test]
fn hovering_a_box_answers_in_the_same_event() {
    let mut shell = shell_for(snapshot(3, 20, true, None), 1.0);
    shell.set_drive_total(Some(1_000_000_000));
    let folder = folder_item(&shell, 0);
    let fx = move_to(&mut shell, title_point(folder.rect), 0);
    assert!(
        fx.iter()
            .any(|e| matches!(e, ShellEffect::Map(Effect::SetHover(Some(_)))))
    );
    // No second call is needed: the very next chrome has the facts.
    let line = shell.info_line();
    assert!(line.name.starts_with("folder"), "{line:?}");
    assert!(line.details.contains("of parent"), "{}", line.details);
    assert!(line.details.contains("of drive"), "{}", line.details);
    assert!(line.details.ends_with("20 files"), "{}", line.details);
    let all = texts(&mut shell);
    assert!(all.contains(&line.name) && all.contains(&line.details));
}

#[test]
fn an_unfinished_scan_marks_sizes_as_lower_bounds() {
    let mut shell = shell_for(snapshot(3, 20, false, None), 1.0);
    shell.set_drive_total(Some(1_000_000_000));
    let folder = folder_item(&shell, 0);
    move_to(&mut shell, title_point(folder.rect), 0);
    let line = shell.info_line();
    assert!(line.details.starts_with("≥ "), "{}", line.details);
}

#[test]
fn the_info_bar_follows_the_selection_when_nothing_is_hovered() {
    let mut shell = shell_for(snapshot(3, 20, true, None), 1.0);
    shell.set_status("Scanning… 50%".into(), true);
    assert_eq!(shell.info_line().name, "Scanning… 50%");
    let folder = folder_item(&shell, 1);
    let p = title_point(folder.rect);
    move_to(&mut shell, p, 0);
    shell.update(InputEvent::Click { x: p.0, y: p.1 }, t(1));
    // Pointer leaves the map: the selection is described.
    shell.update(InputEvent::PointerLeft, t(2));
    assert!(shell.info_line().name.starts_with("folder"));
    shell.update(InputEvent::Key(Key::Escape), t(3));
    assert_eq!(shell.info_line().name, "Scanning… 50%");
}

#[test]
fn ctrl_toggles_and_shift_extends_the_selection() {
    let mut shell = shell_for(snapshot(5, 20, true, None), 1.0);
    let p = |shell: &Shell<TreeSource>, n| title_point(folder_item(shell, n).rect);
    let (a, b, c) = (p(&shell, 0), p(&shell, 1), p(&shell, 3));
    shell.update(InputEvent::Click { x: a.0, y: a.1 }, t(0));
    let ctrl = Modifiers {
        ctrl: true,
        shift: false,
    };
    shell.update(
        InputEvent::ClickWith {
            x: b.0,
            y: b.1,
            mods: ctrl,
        },
        t(1),
    );
    assert_eq!(shell.app().unwrap().selection().len(), 2);
    shell.update(InputEvent::PointerLeft, t(2));
    assert_eq!(shell.info_line().name, "2 items selected");
    // Ctrl on a selected box takes it out again.
    shell.update(
        InputEvent::ClickWith {
            x: b.0,
            y: b.1,
            mods: ctrl,
        },
        t(3),
    );
    assert_eq!(shell.app().unwrap().selection().len(), 1);
    // Shift from the anchor selects everything between, in the same folder.
    let shift = Modifiers {
        ctrl: false,
        shift: true,
    };
    let fx = shell.update(
        InputEvent::ClickWith {
            x: c.0,
            y: c.1,
            mods: shift,
        },
        t(4),
    );
    assert_eq!(shell.app().unwrap().selection().len(), 4);
    assert!(
        fx.iter()
            .any(|e| matches!(e, ShellEffect::Map(Effect::SetSelection(ids)) if ids.len() == 4))
    );
    // The clicked box is the primary one.
    let clicked = shell
        .app()
        .unwrap()
        .items()
        .iter()
        .rev()
        .find(|i| i.rect.contains(c.0, c.1))
        .map(|i| i.id);
    assert_eq!(shell.app().unwrap().selected(), clicked);
}

#[test]
fn hovering_a_swatch_dims_through_an_effect_not_a_relayout() {
    let mut shell = shell_for(snapshot(3, 20, true, None), 1.0);
    let before = shell.app().unwrap().source().last_layout().took;
    let g = shell.geometry().clone();
    let swatch = g.swatches[1];
    let fx = move_to(&mut shell, center(swatch.rect), 0);
    assert_eq!(fx.len(), 1);
    assert!(matches!(fx[0], ShellEffect::Dim(Some(1))));
    // Moving within the same swatch changes nothing.
    assert!(
        move_to(
            &mut shell,
            (center(swatch.rect).0 + 1.0, center(swatch.rect).1),
            1
        )
        .is_empty()
    );
    // The tooltip names the category in full.
    assert!(shell.info_line().name.starts_with("Images"));
    // Crossing to the next swatch moves the dimming.
    let next = g.swatches[2];
    let fx = move_to(&mut shell, center(next.rect), 2);
    assert!(matches!(fx[0], ShellEffect::Dim(Some(2))));
    // Leaving the legend clears it; no layout happened in between.
    let fx = move_to(&mut shell, (600.0, 400.0), 3);
    assert!(fx.iter().any(|e| matches!(e, ShellEffect::Dim(None))));
    assert!(
        !fx.iter()
            .any(|e| matches!(e, ShellEffect::Map(Effect::SetView(_))))
    );
    assert_eq!(shell.app().unwrap().source().last_layout().took, before);
}

#[test]
fn the_link_opens_the_browser_and_is_whole_at_every_width() {
    let mut shell = shell_for(snapshot(2, 5, true, None), 1.0);
    let link = shell.geometry().link;
    let fx = shell.update(
        InputEvent::Click {
            x: center(link).0,
            y: center(link).1,
        },
        t(0),
    );
    assert!(matches!(fx[..], [ShellEffect::OpenLink(url)] if url == LINK_URL));
    assert!(texts(&mut shell).iter().any(|s| s == LINK_TEXT));
    // Hot links draw as accent plus underline.
    move_to(&mut shell, center(link), 1);
    let hot = shell
        .chrome(t(1))
        .widgets
        .iter()
        .any(|w| matches!(w, Widget::Text { hot: true, text, .. } if &**text == LINK_TEXT));
    assert!(hot);
    // Narrow window: the short form, still whole.
    let narrow = shell.resize((600, 500), 1.0, t(2));
    assert!(
        narrow
            .iter()
            .all(|e| !matches!(e, ShellEffect::OpenLink(_)))
    );
    assert!(texts(&mut shell).iter().any(|s| s == LINK_TEXT_SHORT));
    let g = shell.geometry().clone();
    assert!(
        g.link.right() <= 600.0 && g.link.w >= LINK_TEXT_SHORT.chars().count() as f32 * 13.0 * 0.56
    );
}

#[test]
fn disabled_buttons_say_why_and_do_nothing() {
    let mut shell = shell_for(snapshot(3, 20, true, None), 1.0);
    let g = shell.geometry().clone();
    move_to(&mut shell, center(g.back), 0);
    assert_eq!(shell.info_line().name, "Back — nothing to go back to");
    let fx = shell.update(
        InputEvent::Click {
            x: center(g.back).0,
            y: center(g.back).1,
        },
        t(1),
    );
    assert!(fx.is_empty());
    move_to(&mut shell, center(g.up), 2);
    assert_eq!(shell.info_line().name, "Up — already at the top");
    // Go into a folder: Back and Up come alive.
    let folder = folder_item(&shell, 0);
    let p = center(folder.rect);
    shell.update(InputEvent::DoubleClick { x: p.0, y: p.1 }, t(3));
    move_to(&mut shell, center(g.back), 4);
    assert_eq!(shell.info_line().name, "Back (Alt+Left)");
    let fx = shell.update(
        InputEvent::Click {
            x: center(g.up).0,
            y: center(g.up).1,
        },
        t(1000),
    );
    assert!(
        fx.iter()
            .any(|e| matches!(e, ShellEffect::Map(Effect::Zoom { .. })))
    );
    let widgets = shell.chrome(t(1001)).widgets;
    assert!(
        widgets
            .iter()
            .any(|w| matches!(w, Widget::Button { enabled: false, .. }))
    );
}

#[test]
fn the_capacity_bar_appears_with_figures_and_glides() {
    let mut shell = shell_for(snapshot(2, 5, true, None), 1.0);
    let has_bar = |s: &mut Shell<TreeSource>, now| {
        s.chrome(t(now))
            .widgets
            .iter()
            .any(|w| matches!(w, Widget::Capacity { .. }))
    };
    assert!(!has_bar(&mut shell, 0), "no figures, no bar");
    let cap = Capacity::from_bytes(1000, 400, 300, ReservedKind::FilesystemReserve);
    shell.set_capacity(Breakdown::new(&cap, 450, false, 0), t(0));
    assert!(has_bar(&mut shell, 0));
    // The walk ends: the gap becomes the unreadable part, over 250 ms.
    shell.set_capacity(Breakdown::new(&cap, 450, true, 0), t(1000));
    assert!(shell.capacity_changing(t(1100)));
    let unreadable = |s: &mut Shell<TreeSource>, now| {
        s.chrome(t(now))
            .widgets
            .iter()
            .find_map(|w| match w {
                Widget::Capacity { parts, .. } => parts
                    .iter()
                    .find(|(k, _)| *k == SegmentKind::Unreadable)
                    .map(|(_, f)| *f),
                _ => None,
            })
            .expect("capacity")
    };
    let mid = unreadable(&mut shell, 1125);
    assert!(mid > 0.0 && mid < 0.15, "{mid}");
    assert!((unreadable(&mut shell, 1300) - 0.15).abs() < 1e-4);
    assert!(!shell.capacity_changing(t(1300)));
    // Hovering the bar tells the figures in words.
    let g = shell.geometry().clone();
    move_to(&mut shell, center(g.capacity), 2000);
    let text = shell.info_line().name;
    assert!(text.contains("Couldn't read"), "{text}");
}

#[test]
fn reduced_motion_makes_the_bar_and_the_pulse_instant() {
    let mut shell = shell_for(snapshot(2, 5, true, None), 1.0);
    shell.set_reduced_motion(true);
    shell.set_status("Scanning…".into(), true);
    let cap = Capacity::from_bytes(1000, 400, 300, ReservedKind::FilesystemReserve);
    shell.set_capacity(Breakdown::new(&cap, 450, false, 0), t(0));
    shell.set_capacity(Breakdown::new(&cap, 450, true, 0), t(1000));
    assert!(!shell.capacity_changing(t(1001)));
    assert_eq!(shell.pulse(t(250)), 1.0);
    assert!(!shell.pulsing());
}

#[test]
fn scanning_pulses_once_a_second_in_title_text_only() {
    let mut shell = shell_for(snapshot(2, 5, false, None), 1.0);
    assert_eq!(shell.pulse(t(0)), 1.0, "solid when no scan runs");
    shell.set_status("Scanning…".into(), true);
    assert!(shell.pulsing());
    let samples: Vec<f32> = (0..20).map(|i| shell.pulse(t(i * 50))).collect();
    let lo = samples.iter().copied().fold(f32::MAX, f32::min);
    let hi = samples.iter().copied().fold(f32::MIN, f32::max);
    assert!((0.5..0.7).contains(&lo), "{lo}");
    assert!(hi > 0.97 && hi <= 1.0, "{hi}");
    // One second later it is the same again.
    for i in 0..20 {
        assert_eq!(shell.pulse(t(i * 50)), shell.pulse(t(i * 50 + 1000)));
    }
}

#[test]
fn hostile_names_reach_the_info_bar_whole_and_nothing_panics() {
    let nasty: Vec<u8> = {
        let mut v = "ab\u{202e}\u{200b}e\u{0301}".repeat(700).into_bytes();
        v.extend_from_slice(&[0xff, 0xfe, b'x']);
        v
    };
    let mut shell = shell_for(snapshot(3, 20, true, Some(&nasty)), 1.0);
    let folder = folder_item(&shell, 0);
    let fx = move_to(&mut shell, title_point(folder.rect), 0);
    assert!(!fx.is_empty());
    let chrome = shell.chrome(t(0));
    // The facts still fit even though the name is thousands of characters.
    let g = shell.geometry().clone();
    for w in &chrome.widgets {
        if let Widget::Text { rect, .. } = w
            && rect.y >= g.status.y
        {
            assert!(rect.right() <= g.link.x || rect.x >= g.link.x, "{rect:?}");
        }
    }
    let line = shell.info_line();
    assert!(
        line.name.chars().count() > 100,
        "{}",
        line.name.chars().count()
    );
    assert!(line.details.contains("of parent"));
}

#[test]
fn layout_prepared_on_a_worker_replaces_the_old_one_only_when_it_is_still_current() {
    let mut shell = shell_for(snapshot(3, 20, false, None), 1.0);
    let before = shell.app().unwrap().items();
    let first_epoch_items = before.len();

    // The scan publishes more data.
    let more = snapshot(6, 20, false, None);
    let app = shell.app_mut().unwrap();
    app.source_mut().set_snapshot(more);
    let pending = app
        .begin_relayout()
        .expect("tree sources lay out on a worker");
    // Until the worker is done, the old layout stays.
    assert_eq!(app.items().len(), first_epoch_items);
    let done = std::thread::spawn(move || pending.run())
        .join()
        .expect("worker");
    let fx = app.finish_relayout(done);
    assert!(fx.iter().any(|e| matches!(e, Effect::SetView(_))));
    assert!(app.items().len() > first_epoch_items);

    // A result that arrives after the user went somewhere else is dropped.
    let more = snapshot(8, 20, false, None);
    app.source_mut().set_snapshot(more);
    let pending = app.begin_relayout().expect("job");
    let done = std::thread::spawn(move || pending.run())
        .join()
        .expect("worker");
    let folder = app
        .items()
        .iter()
        .find(|i| i.depth == 1 && i.kind == ItemKind::FolderTitled)
        .expect("folder")
        .rect;
    app.update(
        InputEvent::DoubleClick {
            x: folder.x + folder.w / 2.0,
            y: folder.y + folder.h / 2.0,
        },
        t(10),
    );
    let view = app.current_view();
    assert!(app.finish_relayout(done).is_empty());
    assert_eq!(app.current_view(), view);
}

#[test]
fn hover_and_selection_survive_a_layout_from_the_worker() {
    let mut shell = shell_for(snapshot(3, 20, false, None), 1.0);
    let folder = folder_item(&shell, 0);
    let p = center(folder.rect);
    shell.update(InputEvent::Click { x: p.0, y: p.1 }, t(0));
    move_to(&mut shell, p, 1);
    let app = shell.app_mut().unwrap();
    app.source_mut().set_snapshot(snapshot(3, 40, false, None));
    let pending = app.begin_relayout().expect("job");
    let fx = app.finish_relayout(pending.run());
    assert!(fx.iter().any(|e| matches!(e, Effect::SetSelected(Some(_)))));
    assert!(fx.iter().any(|e| matches!(e, Effect::SetHover(Some(_)))));
}

#[test]
fn a_scale_change_lays_everything_out_again_without_a_restart() {
    let mut shell = shell_for(snapshot(3, 20, true, None), 1.0);
    let g1 = shell.geometry().clone();
    let fx = shell.resize((1800, 1200), 1.5, t(1));
    assert!(
        fx.iter()
            .any(|e| matches!(e, ShellEffect::Map(Effect::SetView(_))))
    );
    let g2 = shell.geometry().clone();
    assert!(g2.map.y > g1.map.y);
    assert_eq!(g2.map, shell.map_rect());
    assert_eq!(shell.app().unwrap().items()[0].rect, g2.map);
}

#[test]
fn look_changes_report_and_the_accent_defaults() {
    let mut shell = shell_for(snapshot(2, 5, true, None), 1.0);
    let light = shell.palette();
    assert_eq!(light.accent, bs_render::Color::hex(0x3B82F6));
    let dark = Look {
        dark: true,
        accent: Some(bs_render::Color::rgb(1, 2, 3)),
    };
    assert!(shell.set_look(dark));
    assert!(!shell.set_look(dark));
    assert!(shell.palette().dark);
    assert_eq!(shell.palette().accent, bs_render::Color::rgb(1, 2, 3));
}
