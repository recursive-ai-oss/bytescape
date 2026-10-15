//! The hover card, the keys that act on boxes, the Esc ladder and the
//! clickable breadcrumb, through the shell with real layouts. Time is part of
//! the events, so nothing here sleeps.

use std::sync::Arc;
use std::time::Duration;

use bs_layout::Metrics;
use bs_render::{CardPart, Item, ItemKind, Rect, TextEngine, Widget, ZoomDirection};
use bs_tree::{Aggregator, Batch, Category, DirKey, Meta, NodeKind, Snapshot};
use bs_ui::chrome::TextMeasure;
use bs_ui::shell::{CANCEL_CONFIRM_AFTER, CARD_REST, Look, Shell, ShellEffect};
use bs_ui::tree::TreeSource;
use bs_ui::{App, Direction, Effect, InputEvent, Key};

/// The path the tree builds for `rest` under the fixture's root `/data`. The
/// tree joins names with the platform's separator, so the expectation does too;
/// the root itself is only a fixture name.
fn under_data(rest: &str) -> String {
    format!("/data{}{rest}", std::path::MAIN_SEPARATOR)
}

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
        mtime: 1_709_596_800,
        ..Meta::default()
    }
}

/// A root named `root` with one folder per name in `names`, each holding
/// `files` files; `finish` marks every listing done.
fn snap(root: &[u8], names: &[&[u8]], files: u32, finish: bool) -> Arc<Snapshot> {
    let mut agg = Aggregator::new();
    let mut b = Batch::new();
    b.dir(DirKey(0), DirKey::NONE, root, &meta(4096)).unwrap();
    for (i, name) in names.iter().enumerate() {
        let key = DirKey(i as u32 + 1);
        b.dir(key, DirKey(0), name, &meta(4096)).unwrap();
        for f in 0..files {
            // The first folder is the biggest, so it is laid out first.
            let size = 1_000_000 * (names.len() as u64 - i as u64) + u64::from(f);
            b.file(
                NodeKind::File,
                key,
                format!("file{f}").as_bytes(),
                &meta(size),
            )
            .unwrap();
        }
        if finish {
            b.listing_done(key);
        }
    }
    if finish {
        b.listing_done(DirKey(0));
    }
    agg.apply(&b);
    agg.publish()
}

fn plain(finish: bool) -> Arc<Snapshot> {
    snap(b"/data", &[b"folder1", b"folder2", b"folder3"], 20, finish)
}

const SIZE: (u32, u32) = (1200, 800);

fn shell_for(snapshot: Arc<Snapshot>, scale: f32) -> Shell<TreeSource> {
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
    let source = TreeSource::new(snapshot, Metrics::from_logical(13.0, scale)).expect("root");
    shell.set_app(App::new(source, map));
    shell
}

fn t(ms: u64) -> Duration {
    Duration::from_millis(ms)
}

fn center(r: Rect) -> (f32, f32) {
    (r.x + r.w / 2.0, r.y + r.h / 2.0)
}

fn folders(shell: &Shell<TreeSource>) -> Vec<Item> {
    shell
        .app()
        .expect("app")
        .items()
        .iter()
        .filter(|i| i.depth == 1 && i.kind == ItemKind::FolderTitled)
        .cloned()
        .collect()
}

/// A point on a folder's title bar: the folder itself is under the pointer.
fn title_point(r: Rect) -> (f32, f32) {
    (r.x + r.w / 2.0, r.y + 3.0)
}

fn a_file(shell: &Shell<TreeSource>) -> Item {
    shell
        .app()
        .expect("app")
        .items()
        .iter()
        .find(|i| i.depth == 2 && i.kind == ItemKind::FileTile && i.rect.w > 12.0)
        .expect("a file tile")
        .clone()
}

fn move_to(shell: &mut Shell<TreeSource>, p: (f32, f32), at: u64) -> Vec<ShellEffect> {
    shell.update(InputEvent::PointerMoved { x: p.0, y: p.1 }, t(at))
}

fn click_at(shell: &mut Shell<TreeSource>, p: (f32, f32), at: u64) -> Vec<ShellEffect> {
    shell.update(InputEvent::Click { x: p.0, y: p.1 }, t(at))
}

fn key(shell: &mut Shell<TreeSource>, k: Key, at: u64) -> Vec<ShellEffect> {
    shell.update(InputEvent::Key(k), t(at))
}

fn card_texts(shell: &Shell<TreeSource>) -> Vec<String> {
    shell.card().map_or_else(Vec::new, |c| {
        c.parts
            .iter()
            .filter_map(|p| match p {
                CardPart::Line { text, .. } | CardPart::Path { text, .. } => Some(text.to_string()),
                CardPart::Button { label, .. }
                | CardPart::Row { label, .. }
                | CardPart::Hold { label, .. } => Some(label.to_string()),
                CardPart::Rule { .. } => None,
            })
            .collect()
    })
}

fn path_rect(shell: &Shell<TreeSource>) -> Rect {
    shell
        .card()
        .and_then(|c| {
            c.parts.iter().find_map(|p| match p {
                CardPart::Path { rect, .. } => Some(*rect),
                _ => None,
            })
        })
        .expect("a path row")
}

fn button_rect(shell: &Shell<TreeSource>, name: &str) -> Option<(Rect, bool)> {
    shell.card().and_then(|c| {
        c.parts.iter().find_map(|p| match p {
            CardPart::Button {
                rect,
                label,
                enabled,
                ..
            } if &**label == name => Some((*rect, *enabled)),
            _ => None,
        })
    })
}

fn copied(shell: &Shell<TreeSource>) -> bool {
    shell.card().is_some_and(|c| {
        c.parts
            .iter()
            .any(|p| matches!(p, CardPart::Path { copied: true, .. }))
    })
}

fn info(shell: &Shell<TreeSource>) -> String {
    shell.info_line().name
}

// ---- when the card shows -----------------------------------------------------

#[test]
fn the_card_shows_after_the_pointer_rests_and_not_before() {
    let mut shell = shell_for(plain(true), 1.0);
    let folder = folders(&shell)[0].clone();
    move_to(&mut shell, title_point(folder.rect), 0);
    assert!(!shell.tick(t(449)));
    assert!(shell.card().is_none());
    assert_eq!(shell.next_deadline(t(449)), Some(t(1)));
    assert!(shell.tick(CARD_REST));
    let card = shell.card().expect("card");
    assert!(!card.rect.intersects(&folder.rect), "covers its box");
    let texts = card_texts(&shell);
    assert_eq!(texts[0], "folder1");
    assert!(texts.contains(&under_data("folder1")), "{texts:?}");
    assert!(texts.iter().any(|s| s.contains("on disk")));
    // Showing it needs no further deadline.
    assert_eq!(shell.next_deadline(t(500)), None);
}

#[test]
fn moving_restarts_the_rest() {
    let mut shell = shell_for(plain(true), 1.0);
    let folder = folders(&shell)[0].clone();
    let p = title_point(folder.rect);
    move_to(&mut shell, p, 0);
    move_to(&mut shell, (p.0 + 12.0, p.1), 300);
    assert!(!shell.tick(t(600)));
    assert_eq!(shell.next_deadline(t(300)), Some(t(450)));
    assert!(shell.tick(t(750)));
    assert!(shell.card().is_some());
    // A tremor of a pixel or two does not count as moving.
    let mut shell = shell_for(plain(true), 1.0);
    move_to(&mut shell, p, 0);
    move_to(&mut shell, (p.0 + 1.0, p.1 + 1.0), 300);
    assert!(shell.tick(t(450)));
}

#[test]
fn the_card_goes_when_the_pointer_moves_to_another_box_or_leaves() {
    let mut shell = shell_for(plain(true), 1.0);
    let fs = folders(&shell);
    move_to(&mut shell, title_point(fs[0].rect), 0);
    shell.tick(t(500));
    assert!(shell.card().is_some());
    // The card sits over the next box, so aim at the last one.
    let far = fs.last().expect("folder");
    move_to(&mut shell, title_point(far.rect), 600);
    assert!(shell.card().is_none(), "another box");
    shell.tick(t(1_100));
    assert_eq!(card_texts(&shell)[0], "folder3");
    shell.update(InputEvent::PointerLeft, t(1_200));
    assert!(shell.card().is_none(), "left the window");
    assert_eq!(shell.next_deadline(t(1_200)), None);
    // Moving onto the toolbar also ends it.
    move_to(&mut shell, title_point(fs[0].rect), 2_000);
    shell.tick(t(2_500));
    assert!(shell.card().is_some());
    move_to(&mut shell, (10.0, 5.0), 2_600);
    assert!(shell.card().is_none());
}

#[test]
fn the_pointer_can_travel_onto_the_card_without_losing_it() {
    let mut shell = shell_for(plain(true), 1.0);
    let folder = folders(&shell)[0].clone();
    move_to(&mut shell, title_point(folder.rect), 0);
    shell.tick(t(500));
    let rect = shell.card().expect("card").rect;
    // Through the gap beside the box, then onto the card.
    let gap = (
        if rect.x > folder.rect.x {
            rect.x - 3.0
        } else {
            rect.right() + 3.0
        },
        rect.y + 20.0,
    );
    move_to(&mut shell, gap, 520);
    assert!(shell.card().is_some(), "gap");
    move_to(&mut shell, center(rect), 540);
    assert!(shell.card().is_some(), "card");
    // The box it describes stays hovered meanwhile.
    assert!(shell.app().expect("app").hover().is_some());
    assert!(info(&shell).starts_with("folder1"));
}

#[test]
fn a_click_on_the_map_hides_the_card_and_restarts_the_rest() {
    let mut shell = shell_for(plain(true), 1.0);
    let folder = folders(&shell)[0].clone();
    let p = title_point(folder.rect);
    move_to(&mut shell, p, 0);
    shell.tick(t(500));
    click_at(&mut shell, p, 600);
    assert!(shell.card().is_none());
    assert!(!shell.tick(t(900)));
    assert!(shell.tick(t(1_050)));
}

#[test]
fn wheel_navigation_removes_the_card() {
    let mut shell = shell_for(plain(true), 1.0);
    let folder = folders(&shell)[0].clone();
    let p = title_point(folder.rect);
    move_to(&mut shell, p, 0);
    shell.tick(t(500));
    let fx = shell.update(
        InputEvent::Wheel {
            x: p.0,
            y: p.1,
            steps: 1.0,
        },
        t(600),
    );
    assert!(fx.iter().any(|e| matches!(
        e,
        ShellEffect::Map(Effect::Zoom {
            direction: ZoomDirection::In,
            ..
        })
    )));
    assert!(shell.card().is_none());
    assert_eq!(shell.next_deadline(t(600)), None);
}

#[test]
fn double_click_on_a_file_shows_its_card_at_once_and_opens_nothing() {
    let mut shell = shell_for(plain(true), 1.0);
    let file = a_file(&shell);
    let p = center(file.rect);
    let view = shell.app().expect("app").current_view();
    shell.update(InputEvent::DoubleClick { x: p.0, y: p.1 }, t(0));
    assert!(shell.card().is_some());
    assert_eq!(shell.app().expect("app").current_view(), view);
    assert!(card_texts(&shell)[0].starts_with("file"));
    // A file has no file count and no folder wording.
    assert!(!card_texts(&shell).iter().any(|s| s.contains(" files")));
}

#[test]
fn enter_on_a_selected_file_shows_the_card_too() {
    let mut shell = shell_for(plain(true), 1.0);
    let file = a_file(&shell);
    let p = center(file.rect);
    click_at(&mut shell, p, 0);
    key(&mut shell, Key::Enter, 10);
    assert!(shell.card().is_some());
}

#[test]
fn new_data_rebuilds_the_open_card_with_the_new_numbers() {
    let mut shell = shell_for(plain(true), 1.0);
    let folder = folders(&shell)[0].clone();
    move_to(&mut shell, title_point(folder.rect), 0);
    shell.tick(t(500));
    assert!(card_texts(&shell).iter().any(|s| s.contains("20 files")));
    let bigger = snap(b"/data", &[b"folder1", b"folder2", b"folder3"], 40, true);
    shell
        .app_mut()
        .expect("app")
        .source_mut()
        .set_snapshot(bigger);
    shell.update(InputEvent::DataChanged, t(600));
    assert!(shell.card().is_some(), "same box, still hovered");
    assert!(card_texts(&shell).iter().any(|s| s.contains("40 files")));
}

// ---- what the card says ------------------------------------------------------

#[test]
fn clicking_the_path_copies_it_and_says_copied_for_a_moment() {
    let mut shell = shell_for(plain(true), 1.0);
    let folder = folders(&shell)[0].clone();
    move_to(&mut shell, title_point(folder.rect), 0);
    shell.tick(t(500));
    let r = path_rect(&shell);
    assert!(!copied(&shell));
    move_to(&mut shell, center(r), 510);
    assert!(shell.wants_pointer_cursor());
    let fx = click_at(&mut shell, center(r), 520);
    let copies: Vec<&String> = fx
        .iter()
        .filter_map(|e| match e {
            ShellEffect::CopyText(s) => Some(s),
            _ => None,
        })
        .collect();
    assert_eq!(copies, [under_data("folder1").as_str()]);
    assert!(copied(&shell));
    assert!(shell.next_deadline(t(520)).is_some());
    assert!(!shell.tick(t(2_000)) || copied(&shell));
    shell.tick(t(2_100));
    assert!(!copied(&shell));
    assert!(shell.card().is_some());
}

#[test]
fn a_folder_still_being_counted_says_so_and_cannot_be_deleted() {
    let mut shell = shell_for(plain(false), 1.0);
    let folder = folders(&shell)[0].clone();
    move_to(&mut shell, title_point(folder.rect), 0);
    shell.tick(t(500));
    let texts = card_texts(&shell);
    assert!(
        texts.iter().any(|s| s.starts_with("≥ ")),
        "sizes are lower bounds: {texts:?}"
    );
    assert!(
        texts
            .iter()
            .any(|s| s == "Delete unavailable: Still scanning")
    );
    let (rect, enabled) = button_rect(&shell, "Delete").expect("delete button");
    assert!(!enabled);
    // Clicking it does nothing.
    let fx = click_at(&mut shell, center(rect), 600);
    assert!(
        !fx.iter().any(|e| matches!(e, ShellEffect::Action { .. })),
        "{fx:?}"
    );
}

#[test]
fn an_enabled_button_asks_for_its_action_on_that_box() {
    let mut shell = shell_for(plain(true), 1.0);
    let folder = folders(&shell)[0].clone();
    move_to(&mut shell, title_point(folder.rect), 0);
    shell.tick(t(500));
    let (rect, enabled) = button_rect(&shell, "Open").expect("open");
    assert!(enabled);
    let fx = click_at(&mut shell, center(rect), 600);
    let asked: Vec<_> = fx
        .iter()
        .filter_map(|e| match e {
            ShellEffect::Action { action, items, .. } => Some((*action, items.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(asked, [(bs_ui::action::Action::Open, vec![folder.id])]);
    assert!(shell.card().is_none(), "the card closes after an action");
}

#[cfg(unix)]
#[test]
fn a_recognized_folder_shows_its_note() {
    let mut shell = shell_for(snap(b"/var/cache", &[b"apt"], 10, true), 1.0);
    let folder = folders(&shell)[0].clone();
    move_to(&mut shell, title_point(folder.rect), 0);
    shell.tick(t(500));
    let all = card_texts(&shell).join(" ");
    assert!(all.contains("Package cache managed by APT."), "{all}");
    // The note is words about what it is; the card never judges.
    let lower = all.to_lowercase();
    for banned in ["safe", "junk", "reclaim", "clean up", "freed"] {
        assert!(!lower.contains(banned), "{all}");
    }
}

#[cfg(unix)]
#[test]
fn a_folder_of_the_operating_system_cannot_be_deleted_and_says_why() {
    let mut shell = shell_for(snap(b"/usr", &[b"lib"], 10, true), 1.0);
    let folder = folders(&shell)[0].clone();
    move_to(&mut shell, title_point(folder.rect), 0);
    shell.tick(t(500));
    let all = card_texts(&shell).join(" ");
    assert!(all.contains("Part of the operating system"), "{all}");
    assert!(!button_rect(&shell, "Delete").expect("button").1);
    // The key says the same and does nothing.
    click_at(&mut shell, title_point(folder.rect), 700);
    let fx = key(&mut shell, Key::Delete, 710);
    assert!(fx.is_empty(), "{fx:?}");
    assert_eq!(info(&shell), "Delete: Part of the operating system");
}

#[test]
fn hostile_names_build_a_card_inside_the_map() {
    let mut long = vec![b'a'; 4000];
    long.extend_from_slice("\u{202e}\u{200b}e\u{301}\u{301}".as_bytes());
    let names: [&[u8]; 3] = [
        &long,
        b"bad\xff\xfename\nwith\tcontrols",
        "שלום עולם".as_bytes(),
    ];
    let mut shell = shell_for(snap(b"/data", &names, 6, true), 1.0);
    let map = shell.map_rect();
    for (i, folder) in folders(&shell).into_iter().enumerate() {
        let at = 10_000 * (i as u64 + 1);
        move_to(&mut shell, title_point(folder.rect), at);
        shell.tick(t(at + 500));
        let card = shell.card().expect("card").rect;
        assert!(
            card.x >= map.x && card.right() <= map.right() + 0.5,
            "{card:?}"
        );
        assert!(
            card.y >= map.y && card.bottom() <= map.bottom() + 0.5,
            "{card:?}"
        );
        for text in card_texts(&shell) {
            assert!(!text.chars().any(|c| c == '\n' || c == '\t'), "{text:?}");
        }
        // The chrome with its breadcrumb and info bar draws too.
        let _ = shell.chrome(t(at + 600));
    }
}

// ---- keys and actions ------------------------------------------------------

#[test]
fn arrows_move_the_selection_and_the_info_bar_follows() {
    let mut shell = shell_for(plain(true), 1.0);
    let fs = folders(&shell);
    let fx = key(&mut shell, Key::Arrow(Direction::Right), 0);
    assert!(fx.iter().any(|e| matches!(
        e,
        ShellEffect::Map(Effect::SetSelected(Some(id))) if *id == fs[0].id
    )));
    assert!(info(&shell).starts_with("folder1"), "{}", info(&shell));
    key(&mut shell, Key::Arrow(Direction::Right), 1);
    assert_eq!(shell.app().expect("app").selected(), Some(fs[1].id));
    assert!(info(&shell).starts_with("folder2"));
}

#[test]
fn ctrl_c_copies_the_path_of_the_selection() {
    let mut shell = shell_for(plain(true), 1.0);
    let fs = folders(&shell);
    let fx = key(&mut shell, Key::CopyPath, 0);
    assert!(fx.is_empty());
    assert_eq!(info(&shell), "Copy path: Nothing selected");
    click_at(&mut shell, title_point(fs[1].rect), 10);
    let fx = key(&mut shell, Key::CopyPath, 20);
    assert!(matches!(&fx[..], [ShellEffect::CopyText(p)] if *p == under_data("folder2")));
    assert_eq!(info(&shell), "Copied the path");
    // The message goes after a few seconds.
    shell.tick(t(10_000));
    assert_ne!(info(&shell), "Copied the path");
    // Several boxes: one path per line.
    shell.update(
        InputEvent::ClickWith {
            x: title_point(fs[0].rect).0,
            y: title_point(fs[0].rect).1,
            mods: bs_ui::Modifiers {
                ctrl: true,
                shift: false,
            },
        },
        t(10_100),
    );
    let fx = key(&mut shell, Key::CopyPath, 10_200);
    assert!(
        matches!(&fx[..], [ShellEffect::CopyText(p)] if *p == format!("{}\n{}", under_data("folder2"), under_data("folder1"))),
        "{fx:?}"
    );
}

#[test]
fn delete_keys_without_a_selection_say_why_and_do_nothing() {
    let mut shell = shell_for(plain(true), 1.0);
    for (k, label) in [
        (Key::Delete, "Delete"),
        (Key::DeletePermanently, "Delete permanently"),
        (Key::Rescan, "Rescan this folder"),
    ] {
        assert!(key(&mut shell, k, 0).is_empty());
        assert_eq!(info(&shell), format!("{label}: Nothing selected"));
    }
}

#[test]
fn keys_that_act_on_files_are_handed_on_and_the_rest_are_handled_in_the_shell() {
    let mut shell = shell_for(plain(true), 1.0);
    let fs = folders(&shell);
    click_at(&mut shell, title_point(fs[0].rect), 0);
    // Scanning a folder again belongs to the host, which owns the scan.
    let fx = key(&mut shell, Key::Rescan, 10);
    assert!(
        matches!(&fx[..], [ShellEffect::Action { action, items, .. }]
            if *action == bs_ui::action::Action::Rescan && items == &vec![fs[0].id]),
        "{fx:?}"
    );
    // Deleting, undoing and the menu are the shell's own (see tests/actions.rs);
    // with no platform service behind it, deleting says so and does nothing.
    let fx = key(&mut shell, Key::Delete, 20);
    assert!(fx.is_empty());
    assert_eq!(info(&shell), "Delete: not available here");
    let fx = key(&mut shell, Key::Undo, 30);
    assert!(fx.is_empty());
    assert_eq!(info(&shell), "Nothing to undo");
    assert!(key(&mut shell, Key::ContextMenu, 40).is_empty());
    assert!(shell.modal_open());
}

#[test]
fn a_folder_still_being_counted_refuses_delete_with_its_reason() {
    let mut shell = shell_for(plain(false), 1.0);
    let fs = folders(&shell);
    click_at(&mut shell, title_point(fs[0].rect), 0);
    let fx = key(&mut shell, Key::Delete, 10);
    assert!(fx.is_empty());
    assert_eq!(info(&shell), "Delete: Still scanning");
    let fx = key(&mut shell, Key::DeletePermanently, 20);
    assert!(fx.is_empty());
}

// ---- Esc ---------------------------------------------------------------------

#[test]
fn escape_peels_the_card_then_the_selection_then_the_scan() {
    let mut shell = shell_for(plain(false), 1.0);
    shell.set_status("Scanning…".into(), true);
    shell.begin_scan(t(0));
    let fs = folders(&shell);
    click_at(&mut shell, title_point(fs[0].rect), 100);
    move_to(&mut shell, title_point(fs[1].rect), 200);
    shell.tick(t(700));
    assert!(shell.card().is_some());
    // First the card.
    assert!(key(&mut shell, Key::Escape, 800).is_empty());
    assert!(shell.card().is_none());
    assert_eq!(shell.app().expect("app").selection().len(), 1);
    // Then the selection.
    let fx = key(&mut shell, Key::Escape, 900);
    assert!(matches!(
        &fx[..],
        [ShellEffect::Map(Effect::SetSelected(None))]
    ));
    // Then the scan, at once while it is young.
    let fx = key(&mut shell, Key::Escape, 1_000);
    assert!(matches!(&fx[..], [ShellEffect::CancelScan]), "{fx:?}");
}

#[test]
fn an_old_scan_needs_a_second_escape() {
    let mut shell = shell_for(plain(false), 1.0);
    shell.set_status("Scanning…".into(), true);
    shell.begin_scan(t(0));
    let late = CANCEL_CONFIRM_AFTER.as_millis() as u64 + 1_000;
    let fx = key(&mut shell, Key::Escape, late);
    assert!(fx.is_empty());
    assert_eq!(info(&shell), "Press Esc again to stop the scan");
    let fx = key(&mut shell, Key::Escape, late + 1_000);
    assert!(matches!(&fx[..], [ShellEffect::CancelScan]), "{fx:?}");

    // A second Esc that comes too late asks again.
    let mut shell = shell_for(plain(false), 1.0);
    shell.set_status("Scanning…".into(), true);
    shell.begin_scan(t(0));
    assert!(key(&mut shell, Key::Escape, late).is_empty());
    shell.tick(t(late + 4_000));
    assert!(key(&mut shell, Key::Escape, late + 4_000).is_empty());
    let fx = key(&mut shell, Key::Escape, late + 5_000);
    assert!(matches!(&fx[..], [ShellEffect::CancelScan]));
}

#[test]
fn escape_without_a_scan_does_nothing() {
    let mut shell = shell_for(plain(true), 1.0);
    assert!(key(&mut shell, Key::Escape, 0).is_empty());
}

// ---- breadcrumb --------------------------------------------------------------

fn crumb_texts(shell: &mut Shell<TreeSource>, at: u64) -> Vec<(String, Rect)> {
    let g = shell.geometry().clone();
    shell
        .chrome(t(at))
        .widgets
        .iter()
        .filter_map(|w| match w {
            Widget::Text { rect, text, .. } if g.crumb.contains(rect.x + 1.0, rect.y + 1.0) => {
                Some((text.to_string(), *rect))
            }
            _ => None,
        })
        .collect()
}

#[test]
fn the_breadcrumb_has_a_segment_per_folder_and_the_upper_ones_navigate() {
    let mut shell = shell_for(plain(true), 1.0);
    let folder = folders(&shell)[0].clone();
    let p = title_point(folder.rect);
    shell.update(InputEvent::DoubleClick { x: p.0, y: p.1 }, t(0));
    let parts = crumb_texts(&mut shell, 10);
    let names: Vec<&str> = parts.iter().map(|(s, _)| s.as_str()).collect();
    assert_eq!(names, ["/data", "›", "folder1"], "{names:?}");
    let (_, root_rect) = parts[0].clone();
    // Over the upper segment: a hand and a tooltip.
    move_to(&mut shell, center(root_rect), 20);
    assert!(shell.wants_pointer_cursor());
    assert_eq!(info(&shell), "Go to /data");
    // The current folder is not a link.
    let (_, cur) = parts[2].clone();
    move_to(&mut shell, center(cur), 30);
    assert!(!shell.wants_pointer_cursor());
    // Clicking the upper one zooms out.
    let fx = click_at(&mut shell, center(root_rect), 700);
    assert!(
        fx.iter().any(|e| matches!(
            e,
            ShellEffect::Map(Effect::Zoom {
                direction: ZoomDirection::Out,
                ..
            })
        )),
        "{fx:?}"
    );
    assert_eq!(shell.app().expect("app").depth(), 1);
    let names: Vec<String> = crumb_texts(&mut shell, 710)
        .into_iter()
        .map(|(s, _)| s)
        .collect();
    assert_eq!(names, ["/data"]);
}

#[test]
fn a_long_breadcrumb_keeps_the_current_folder_and_the_ellipsis_is_not_a_link() {
    let long = "a-rather-long-folder-name-".repeat(6);
    let mut shell = shell_for(
        snap(long.as_bytes(), &[long.as_bytes(), b"inner"], 10, true),
        1.0,
    );
    let fs = folders(&shell);
    let p = title_point(fs.last().expect("folder").rect);
    shell.update(InputEvent::DoubleClick { x: p.0, y: p.1 }, t(0));
    let parts = crumb_texts(&mut shell, 10);
    let last = parts.last().expect("segment");
    assert!(
        last.0 == "inner" || last.0.starts_with("a-rather"),
        "{parts:?}"
    );
    let area = shell.geometry().crumb;
    for (_, r) in &parts {
        assert!(r.right() <= area.right() + 0.5);
    }
    if let Some((_, r)) = parts.iter().find(|(s, _)| s == "…") {
        move_to(&mut shell, center(*r), 20);
        assert!(!shell.wants_pointer_cursor());
    }
}

#[test]
fn groups_and_the_scan_placeholder_have_a_breadcrumb_too() {
    // Before the first snapshot there is no map; the crumb set by the window
    // shows as one plain segment.
    let mut shell: Shell<TreeSource> = Shell::new(
        SIZE,
        1.0,
        13.0,
        TextEngine::new(),
        Look {
            dark: false,
            accent: None,
        },
    )
    .with_measure(Box::new(Fixed(7.0)));
    shell.set_crumb("/home/alex".into());
    let parts = crumb_texts(&mut shell, 0);
    assert_eq!(parts.len(), 1);
    assert_eq!(parts[0].0, "/home/alex");
}
