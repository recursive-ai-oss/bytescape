//! Presentation details of the map: the context menu, the hover card, keyboard
//! multi-select, what a screen reader is told about actions, and the About
//! box.

use std::sync::Arc;
use std::time::Duration;

use bs_layout::Metrics;
use bs_render::{CardPart, Item, ItemKind, Rect, TextEngine};
use bs_tree::{Aggregator, Batch, Category, DirKey, Meta, NodeKind, Snapshot};
use bs_ui::chrome::TextMeasure;
use bs_ui::notice::AboutInfo;
use bs_ui::shell::{
    Look, Shell,
    access::{AccessTarget, AccessTree},
};
use bs_ui::tree::TreeSource;
use bs_ui::{App, Direction, InputEvent, Key};

struct Fixed(f32);

impl TextMeasure for Fixed {
    fn width(&self, text: &str) -> Option<f32> {
        Some(text.chars().count() as f32 * self.0)
    }
}

fn t(ms: u64) -> Duration {
    Duration::from_millis(ms)
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

fn tree(folders: u32) -> Arc<Snapshot> {
    let mut agg = Aggregator::new();
    let mut b = Batch::new();
    b.dir(DirKey(0), DirKey::NONE, b"/data", &meta(4096))
        .unwrap();
    for i in 0..folders {
        let key = DirKey(i + 1);
        b.dir(
            key,
            DirKey(0),
            format!("folder{}", i + 1).as_bytes(),
            &meta(4096),
        )
        .unwrap();
        for f in 0..10u32 {
            b.file(
                NodeKind::File,
                key,
                format!("f{f}").as_bytes(),
                &meta(1_000_000 * u64::from(folders - i) + u64::from(f)),
            )
            .unwrap();
        }
        b.listing_done(key);
    }
    b.listing_done(DirKey(0));
    agg.apply(&b);
    agg.publish()
}

fn shell_of(size: (u32, u32), scale: f32, folders: u32) -> Shell<TreeSource> {
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
    .with_measure(Box::new(Fixed(13.0 * scale * 0.56)));
    let map = shell.map_rect();
    let source = TreeSource::new(tree(folders), Metrics::from_logical(13.0, scale)).expect("root");
    shell.set_app(App::new(source, map));
    shell
}

fn folders(shell: &Shell<TreeSource>) -> Vec<Item> {
    let mut v: Vec<Item> = shell
        .app()
        .unwrap()
        .items()
        .iter()
        .filter(|i| i.depth == 1 && i.kind == ItemKind::FolderTitled)
        .cloned()
        .collect();
    v.sort_by(|a, b| {
        (a.rect.x, a.rect.y)
            .partial_cmp(&(b.rect.x, b.rect.y))
            .unwrap()
    });
    v
}

fn center(r: Rect) -> (f32, f32) {
    (r.x + r.w / 2.0, r.y + r.h / 2.0)
}

/// A point on a folder's title bar, which is the folder itself.
fn title_point(r: Rect) -> (f32, f32) {
    (r.x + r.w / 2.0, r.y + 3.0)
}

/// The smallest box that holds the point: what the pointer is over.
fn box_at(shell: &Shell<TreeSource>, p: (f32, f32)) -> Option<Rect> {
    shell
        .app()?
        .items()
        .iter()
        .filter(|i| i.rect.contains(p.0, p.1))
        .min_by(|a, b| {
            (a.rect.w * a.rect.h)
                .partial_cmp(&(b.rect.w * b.rect.h))
                .unwrap()
        })
        .map(|i| i.rect)
}

// ---- the context menu ------------------------------------------------------------------------

#[test]
fn only_zoom_into_carries_the_enter_key() {
    let mut s = shell_of((1200, 800), 1.0, 3);
    let (x, y) = title_point(folders(&s)[0].rect);
    s.update(InputEvent::RightClick { x, y }, t(10));
    let items = s.menu_items();
    let with_enter: Vec<&str> = items
        .iter()
        .filter(|(_, hint, _)| hint == "Enter")
        .map(|(l, _, _)| l.as_str())
        .collect();
    assert_eq!(with_enter, ["Zoom into"], "{items:?}");
    let open = items.iter().find(|(l, _, _)| l == "Open").expect("Open");
    assert_eq!(open.1, "", "Open has no key of its own");
}

// ---- the hover card ----------------------------------------------------------------------------

fn overlaps(a: Rect, b: Rect) -> bool {
    a.x < b.right() && b.x < a.right() && a.y < b.bottom() && b.y < a.bottom()
}

#[test]
fn the_card_stays_off_the_capacity_bar_and_status_bar_and_off_its_box() {
    for scale in [1.0f32, 2.0] {
        let mut s = shell_of(((1200.0 * scale) as u32, (800.0 * scale) as u32), scale, 4);
        let map = s.map_rect();
        let edge = (bs_render::card::CARD_EDGE * scale).ceil();
        for (n, folder) in folders(&s).iter().enumerate() {
            // The top-left corner of the box, its center and its bottom edge.
            for (k, p) in [
                (folder.rect.x + 4.0, folder.rect.y + 3.0),
                center(folder.rect),
                (folder.rect.right() - 4.0, folder.rect.bottom() - 4.0),
            ]
            .into_iter()
            .enumerate()
            {
                s.update(
                    InputEvent::PointerLeft,
                    t(1_000 * (n as u64 * 3 + k as u64)),
                );
                let at = 1_000 * (n as u64 * 3 + k as u64) + 10;
                s.update(InputEvent::PointerMoved { x: p.0, y: p.1 }, t(at));
                s.tick(t(at + 700));
                let Some(card) = s.card() else {
                    continue;
                };
                let r = card.rect;
                assert!(
                    r.y - edge >= map.y - 0.01 && r.bottom() + edge <= map.bottom() + 0.01,
                    "scale {scale} box {n} point {k}: {r:?} not inside {map:?}"
                );
                assert!(
                    r.x - edge >= map.x - 0.01 && r.right() + edge <= map.right() + 0.01,
                    "scale {scale}: {r:?}"
                );
                // Never over the box it describes, when that leaves room.
                if let Some(hovered) = box_at(&s, p)
                    && hovered.w < map.w / 2.0
                    && hovered.h < map.h / 2.0
                {
                    assert!(!overlaps(r, hovered), "{r:?} over {hovered:?}");
                }
            }
        }
    }
}

// ---- keyboard multi-select -------------------------------------------------------------------------

fn selection(s: &Shell<TreeSource>) -> Vec<u32> {
    s.app().unwrap().selection().to_vec()
}

#[test]
fn shift_and_arrows_extend_the_selection_and_ctrl_space_toggles() {
    let mut s = shell_of((1200, 800), 1.0, 4);
    // One box first, the way a click or an arrow selects it.
    s.update(InputEvent::Key(Key::Arrow(Direction::Right)), t(10));
    let first = selection(&s);
    assert_eq!(first.len(), 1);
    // Shift and an arrow add the neighbour in that direction.
    let mut grew = false;
    for (n, dir) in [
        Direction::Right,
        Direction::Down,
        Direction::Right,
        Direction::Down,
    ]
    .into_iter()
    .enumerate()
    {
        let before = selection(&s);
        s.update(InputEvent::Key(Key::ExtendArrow(dir)), t(20 + n as u64));
        let after = selection(&s);
        assert!(after.len() >= before.len());
        assert!(before.iter().all(|id| after.contains(id)), "nothing lost");
        grew |= after.len() > before.len();
    }
    assert!(grew, "the selection grew");
    let many = selection(&s);
    assert!(many.len() >= 2, "{many:?}");
    assert!(many.contains(&first[0]), "the first box stays selected");
    // The shell's info bar says how many.
    let info = s.info_line();
    assert!(
        format!("{}{}", info.name, info.details).contains(&format!("{} items", many.len())),
        "{} {}",
        info.name,
        info.details
    );

    // Ctrl and Space take the box the keyboard is on out, and put it back.
    let before = selection(&s);
    s.update(InputEvent::Key(Key::ToggleSelected), t(100));
    let out = selection(&s);
    assert_eq!(out.len(), before.len() - 1, "{before:?} -> {out:?}");
    s.update(InputEvent::Key(Key::ToggleSelected), t(110));
    let back = selection(&s);
    assert_eq!(back.len(), before.len());
    for id in &before {
        assert!(back.contains(id));
    }
}

#[test]
fn ctrl_space_with_nothing_selected_selects_the_first_box() {
    let mut s = shell_of((1200, 800), 1.0, 3);
    assert!(selection(&s).is_empty());
    s.update(InputEvent::Key(Key::ToggleSelected), t(10));
    assert_eq!(selection(&s).len(), 1);
    s.update(InputEvent::Key(Key::ExtendArrow(Direction::Right)), t(20));
    assert!(!selection(&s).is_empty());
}

#[test]
fn shift_arrow_with_nothing_beyond_changes_nothing() {
    let mut s = shell_of((1200, 800), 1.0, 1);
    s.update(InputEvent::Key(Key::Arrow(Direction::Right)), t(10));
    let one = selection(&s);
    for dir in [Direction::Left, Direction::Up] {
        s.update(InputEvent::Key(Key::ExtendArrow(dir)), t(20));
    }
    assert_eq!(selection(&s), one);
}

// ---- what a screen reader is told ----------------------------------------------------------------------

fn description(tree: &AccessTree, target: AccessTarget) -> Option<String> {
    let id = tree.node_for(target)?;
    tree.get(id)?.description().map(str::to_string)
}

#[test]
fn menu_rows_say_what_they_do_and_disabled_ones_say_why() {
    let mut s = shell_of((1200, 800), 1.0, 3);
    let (x, y) = title_point(folders(&s)[0].rect);
    s.update(InputEvent::RightClick { x, y }, t(10));
    let rows = s.menu_items();
    let mut tree = AccessTree::new();
    s.access_update(&mut tree, t(20));
    let mut checked = 0;
    for (i, (label, hint, enabled)) in rows.iter().enumerate() {
        let desc = description(&tree, AccessTarget::Popup(i)).unwrap_or_default();
        assert!(!desc.is_empty(), "{label} has no description");
        if *enabled {
            // What it does, and the key when there is one.
            assert!(
                desc.contains(' '),
                "{label}: a sentence, not a bare word: {desc}"
            );
            if !hint.is_empty() {
                assert!(desc.contains(hint.as_str()), "{label}: {desc}");
            }
        } else {
            assert_eq!(&desc, hint, "{label}: the reason");
        }
        checked += 1;
    }
    assert!(checked >= 8);
    let zoom = rows.iter().position(|r| r.0 == "Zoom into").unwrap();
    assert_eq!(
        description(&tree, AccessTarget::Popup(zoom)).as_deref(),
        Some("Goes into this folder. Key: Enter")
    );
}

#[test]
fn card_buttons_say_what_they_do_and_disabled_ones_say_why() {
    let mut s = shell_of((1200, 800), 1.0, 3);
    let (x, y) = title_point(folders(&s)[0].rect);
    s.update(InputEvent::PointerMoved { x, y }, t(10));
    s.tick(t(700));
    assert!(s.card().is_some());
    let mut tree = AccessTree::new();
    s.access_update(&mut tree, t(710));
    // The card's nodes are its lines, its path and its buttons, in order.
    let parts: Vec<(usize, String)> = s
        .card()
        .unwrap()
        .parts
        .iter()
        .filter(|p| {
            matches!(
                p,
                CardPart::Line { .. } | CardPart::Path { .. } | CardPart::Button { .. }
            )
        })
        .enumerate()
        .filter_map(|(i, p)| match p {
            CardPart::Button { label, .. } => Some((i, label.to_string())),
            _ => None,
        })
        .collect();
    assert!(!parts.is_empty());
    let mut described = 0;
    for (i, label) in &parts {
        // The card's parts are numbered after the text and path parts too; a
        // button node exists for every working one.
        if let Some(desc) = description(&tree, AccessTarget::CardPart(*i)) {
            assert!(!desc.is_empty(), "{label}");
            described += 1;
        }
    }
    assert!(described >= 2, "working buttons carry a description");
    let delete = parts.iter().find(|(_, l)| l == "Delete").expect("Delete");
    let desc = description(&tree, AccessTarget::CardPart(delete.0)).expect("a description");
    assert!(
        desc.starts_with("Moves it to the "),
        "what Delete does: {desc}"
    );
}

#[test]
fn every_action_has_a_description_without_banned_words() {
    use bs_ui::action::Action;
    for action in [
        Action::Open,
        Action::ShowInFileManager,
        Action::OpenTerminal,
        Action::CopyPath,
        Action::Properties,
        Action::ZoomInto,
        Action::Rescan,
        Action::OpenInSoftware,
        Action::CopyRemovalCommand,
        Action::Delete,
        Action::DeletePermanently,
        Action::Undo,
        Action::ContextMenu,
        Action::About,
        Action::CheckForUpdates,
    ] {
        let text = action.describe();
        assert!(text.contains(' '), "{action:?}");
        let lower = text.to_lowercase();
        for banned in ["safe", "junk", "reclaim", "clean up", "freed"] {
            assert!(!lower.contains(banned), "{text}");
        }
    }
}

// ---- the About box ------------------------------------------------------------------------------------------

#[test]
fn the_about_box_shows_how_the_window_is_drawn_on_a_line_of_its_own() {
    let mut s = shell_of((1200, 800), 1.0, 3);
    let drawing = "Drawing: software, because no graphics adapter could be used on this computer and the other reasons are given here";
    s.set_about(
        AboutInfo::new("0.1.0", "2026-10-14", "x86_64-unknown-linux-gnu").with_drawing(drawing),
    );
    s.update(InputEvent::Key(Key::About), t(10));
    let popup = s.popup().expect("the About box");
    let lines: Vec<String> = popup
        .card
        .parts
        .iter()
        .filter_map(|p| match p {
            CardPart::Line { text, .. } => Some(text.to_string()),
            _ => None,
        })
        .collect();
    // The target line is its own line, without the drawing text in it.
    assert!(
        lines.iter().any(|l| l == "x86_64-unknown-linux-gnu"),
        "{lines:?}"
    );
    // The drawing line is wrapped and whole: every word is there.
    let drawn: String = lines.join(" ");
    for word in drawing.split_whitespace() {
        assert!(drawn.contains(word), "{word} is missing from {lines:?}");
    }
    let at = lines
        .iter()
        .position(|l| l.starts_with("Drawing:"))
        .unwrap();
    assert!(lines[at + 1].len() > 3, "wrapped over lines: {lines:?}");
    // Without it the box is as before.
    let mut s = shell_of((1200, 800), 1.0, 3);
    s.set_about(AboutInfo::new(
        "0.1.0",
        "2026-10-14",
        "x86_64-unknown-linux-gnu",
    ));
    s.update(InputEvent::Key(Key::About), t(10));
    let text = s.popup().expect("About").card.parts.len();
    assert!(text > 3);
}

#[test]
fn an_empty_drawing_line_is_not_shown() {
    let about = AboutInfo::new("0.1.0", "2026-10-14", "t").with_drawing("  ");
    assert_eq!(about.drawing, None);
    let about = AboutInfo::new("0.1.0", "2026-10-14", "t").with_drawing("Drawing: GPU (x)");
    assert_eq!(about.drawing.as_deref(), Some("Drawing: GPU (x)"));
}
