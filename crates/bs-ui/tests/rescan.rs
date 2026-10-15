//! Reading one folder of a finished map again, in place: the folder keeps its
//! place, the view and selection stay, actions on and in the folder are off
//! until it is counted, a view inside the replaced part moves to a folder
//! that is still there, and a read that failed says so on the folder.

use std::sync::Arc;
use std::time::Duration;

use accesskit_consumer::Tree;
use bs_layout::Metrics;
use bs_render::{CardPart, Item, ItemKind, Rect, TextEngine};
use bs_tree::{Aggregator, Batch, Category, DirKey, Meta, NodeId, NodeKind, Snapshot};
use bs_ui::access::AccessTree;
use bs_ui::chrome::TextMeasure;
use bs_ui::elevation::Offer;
use bs_ui::shell::{Look, Shell, ShellEffect};
use bs_ui::tree::TreeSource;
use bs_ui::{App, InputEvent, Key, ViewSource};

struct Fixed(f32);

impl TextMeasure for Fixed {
    fn width(&self, text: &str) -> Option<f32> {
        Some(text.chars().count() as f32 * self.0)
    }
}

const SIZE: (u32, u32) = (1200, 800);
const MTIME: i64 = 1_709_596_800;

fn t(ms: u64) -> Duration {
    Duration::from_millis(ms)
}

fn meta(size: u64) -> Meta {
    Meta {
        logical: size,
        allocated: size,
        category: Category(1),
        mtime: MTIME,
        ..Meta::default()
    }
}

/// `/data` holding `a` (a file, and the folder `x` with three files) and `b`
/// (twenty files). Fully counted and finished, so it can be rescanned.
fn finished_tree() -> Aggregator {
    let mut agg = Aggregator::new();
    let mut b = Batch::new();
    b.dir(DirKey(0), DirKey::NONE, b"/data", &meta(4096))
        .unwrap();
    b.dir(DirKey(1), DirKey(0), b"a", &meta(4096)).unwrap();
    b.dir(DirKey(2), DirKey(1), b"x", &meta(4096)).unwrap();
    b.dir(DirKey(3), DirKey(0), b"b", &meta(4096)).unwrap();
    b.file(NodeKind::File, DirKey(1), b"inside.bin", &meta(30_000_000))
        .unwrap();
    for f in 0..3u32 {
        b.file(
            NodeKind::File,
            DirKey(2),
            format!("deep{f}.bin").as_bytes(),
            &meta(20_000_000 + u64::from(f)),
        )
        .unwrap();
    }
    for f in 0..20u32 {
        b.file(
            NodeKind::File,
            DirKey(3),
            format!("file{f}").as_bytes(),
            &meta(1_000_000 + u64::from(f)),
        )
        .unwrap();
    }
    for k in 0..4 {
        b.listing_done(DirKey(k));
    }
    agg.apply(&b);
    agg.finish();
    agg
}

fn find(snapshot: &Snapshot, path: &[&str]) -> NodeId {
    let mut at = snapshot.root().expect("root");
    for name in path {
        at = at
            .children()
            .find(|c| c.name() == name.as_bytes())
            .unwrap_or_else(|| panic!("no {name}"));
    }
    at.id()
}

/// Begins reading `/data/a` again and delivers its first records: the folder
/// itself, a file and a new folder `y`, with the listing still open.
fn start_reading_a(agg: &mut Aggregator) -> NodeId {
    let snap = agg.publish();
    let a = find(&snap, &["a"]);
    agg.begin_rescan(a, Some(b"/data/a")).unwrap();
    let mut b = Batch::new();
    b.dir(DirKey(0), DirKey::NONE, b"/data/a", &meta(4096))
        .unwrap();
    b.dir(DirKey(1), DirKey(0), b"y", &meta(4096)).unwrap();
    b.file(NodeKind::File, DirKey(1), b"new.bin", &meta(25_000_000))
        .unwrap();
    b.listing_done(DirKey(1));
    assert_eq!(agg.apply(&b).rejected, 0);
    a
}

/// The rest of the listing of `/data/a`, then the end of the scan.
fn finish_reading_a(agg: &mut Aggregator) {
    let mut b = Batch::new();
    b.file(NodeKind::File, DirKey(0), b"more.bin", &meta(5_000_000))
        .unwrap();
    b.listing_done(DirKey(0));
    assert_eq!(agg.apply(&b).rejected, 0);
    assert_eq!(agg.finish().unresolved, 0);
}

fn shell_over(snapshot: Arc<Snapshot>) -> Shell<TreeSource> {
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
    .with_measure(Box::new(Fixed(2.0)));
    let map = shell.map_rect();
    let source = TreeSource::new(snapshot, Metrics::from_logical(13.0, 1.0)).expect("root");
    shell.set_app(App::new(source, map));
    shell
}

fn item_named(shell: &Shell<TreeSource>, name: &str) -> Item {
    shell
        .app()
        .unwrap()
        .items()
        .iter()
        .find(|i| i.label.as_deref().is_some_and(|l| &*l.name == name))
        .cloned()
        .unwrap_or_else(|| panic!("no box called {name}"))
}

/// Gives the shell the newer snapshot and lays the view out again, the way
/// the window does after a scan tick.
fn show(shell: &mut Shell<TreeSource>, snapshot: Arc<Snapshot>, now: u64) -> Vec<bs_ui::Effect> {
    let app = shell.app_mut().unwrap();
    app.source_mut().set_snapshot(snapshot);
    let mut fx = app.leave_gone_views();
    fx.extend(app.update(InputEvent::DataChanged, t(now)));
    fx
}

fn point_on(item: &Item) -> (f32, f32) {
    if item.kind == ItemKind::FolderTitled {
        (item.rect.x + item.rect.w / 2.0, item.rect.y + 3.0)
    } else {
        (
            item.rect.x + item.rect.w / 2.0,
            item.rect.y + item.rect.h / 2.0,
        )
    }
}

fn show_card(shell: &mut Shell<TreeSource>, item: &Item, at: u64) {
    let p = point_on(item);
    shell.update(InputEvent::PointerMoved { x: p.0, y: p.1 }, t(at));
    shell.tick(t(at + 500));
    assert!(shell.card().is_some(), "no card for {:?}", item.label);
}

fn card_texts(shell: &Shell<TreeSource>) -> Vec<String> {
    shell
        .card()
        .expect("a card")
        .parts
        .iter()
        .filter_map(|p| match p {
            CardPart::Line { text, .. } | CardPart::Path { text, .. } => Some(text.to_string()),
            CardPart::Button { label, .. } => Some(format!("[{label}]")),
            _ => None,
        })
        .collect()
}

fn buttons(shell: &Shell<TreeSource>) -> Vec<(String, bool)> {
    shell
        .card()
        .expect("a card")
        .parts
        .iter()
        .filter_map(|p| match p {
            CardPart::Button { label, enabled, .. } => Some((label.to_string(), *enabled)),
            _ => None,
        })
        .collect()
}

fn enabled(shell: &Shell<TreeSource>, label: &str) -> bool {
    buttons(shell)
        .into_iter()
        .find(|(l, _)| l == label)
        .unwrap_or_else(|| panic!("no button {label}"))
        .1
}

// ---- the folder keeps its place ---------------------------------------------

#[test]
fn a_folder_being_read_again_keeps_its_place_view_and_selection() {
    let mut agg = finished_tree();
    let mut shell = shell_over(agg.publish());
    let before = item_named(&shell, "a");
    let view = shell.app().unwrap().current_view();
    let crumb = shell.app().unwrap().source().breadcrumb(view);
    shell.app_mut().unwrap().select(vec![before.id]);

    let a = start_reading_a(&mut agg);
    assert_eq!(a.index(), before.id);
    shell
        .app_mut()
        .unwrap()
        .source_mut()
        .set_rescanning(Some(a));
    show(&mut shell, agg.publish(), 100);

    let app = shell.app().unwrap();
    assert_eq!(app.current_view(), view, "the view stays");
    assert_eq!(
        app.source().breadcrumb(view),
        crumb,
        "so does the breadcrumb"
    );
    assert_eq!(app.selection(), &[before.id], "and the selection");
    let during = item_named(&shell, "a");
    assert_eq!(during.id, before.id, "the folder is the same box");
    let label = during.label.as_deref().expect("a label");
    assert!(
        label.size.contains("Scanning…"),
        "the folder says it is being counted: {}",
        label.size
    );
    assert!(label.size.contains('≥'), "{}", label.size);

    finish_reading_a(&mut agg);
    shell.app_mut().unwrap().source_mut().set_rescanning(None);
    show(&mut shell, agg.publish(), 400);
    let after = item_named(&shell, "a");
    assert_eq!(after.id, before.id);
    assert!(!after.label.as_deref().unwrap().size.contains("Scanning…"));
    assert_eq!(shell.app().unwrap().selection(), &[before.id]);
}

// ---- actions are off until it is counted --------------------------------------

#[test]
fn actions_on_and_in_the_folder_are_off_until_it_is_counted_again() {
    let mut agg = finished_tree();
    let mut shell = shell_over(agg.publish());
    let a = start_reading_a(&mut agg);
    shell
        .app_mut()
        .unwrap()
        .source_mut()
        .set_rescanning(Some(a));
    show(&mut shell, agg.publish(), 100);

    // The folder itself.
    let folder = item_named(&shell, "a");
    show_card(&mut shell, &folder, 200);
    assert!(!enabled(&shell, "Delete"), "Delete is off on the folder");
    let texts = card_texts(&shell).join("\n");
    assert!(texts.contains("unavailable: Still scanning"), "{texts}");
    // So is reading it again (Shift+F5 on the folder).
    shell.app_mut().unwrap().select(vec![folder.id]);
    let fx = shell.update(InputEvent::Key(Key::Rescan), t(300));
    assert!(fx.is_empty(), "{fx:?}");
    assert_eq!(shell.info_line().name, "Rescan this folder: Still scanning");

    // A folder inside it that already has its whole listing: the scan has
    // not ended, so it is off too.
    let inside = item_named(&shell, "y");
    shell.update(InputEvent::PointerMoved { x: 1.0, y: 1.0 }, t(1000));
    shell.tick(t(1100));
    show_card(&mut shell, &inside, 1200);
    assert!(!enabled(&shell, "Delete"), "off inside the folder");

    // A folder beside it is untouched.
    let beside = item_named(&shell, "b");
    shell.update(InputEvent::PointerMoved { x: 1.0, y: 1.0 }, t(2000));
    shell.tick(t(2100));
    show_card(&mut shell, &beside, 2200);
    assert!(enabled(&shell, "Delete"), "a folder outside is not held");

    // Once the folder is counted again, everything is back.
    finish_reading_a(&mut agg);
    shell.app_mut().unwrap().source_mut().set_rescanning(None);
    show(&mut shell, agg.publish(), 3000);
    let folder = item_named(&shell, "a");
    shell.update(InputEvent::PointerMoved { x: 1.0, y: 1.0 }, t(3100));
    shell.tick(t(3200));
    show_card(&mut shell, &folder, 3300);
    assert!(enabled(&shell, "Delete"));
    shell.app_mut().unwrap().select(vec![folder.id]);
    let fx = shell.update(InputEvent::Key(Key::Rescan), t(3400));
    assert!(
        fx.iter().any(|e| matches!(e, ShellEffect::Action { .. })),
        "{fx:?}"
    );
}

#[test]
fn rescanning_waits_for_a_scan_in_progress_and_for_a_full_scan() {
    let mut shell = shell_over(finished_tree().publish());
    let folder = item_named(&shell, "b");
    shell.app_mut().unwrap().select(vec![folder.id]);

    // A scan is running: the tree is not free to take a rescan.
    shell.set_status("Scanning…".into(), true);
    let fx = shell.update(InputEvent::Key(Key::Rescan), t(10));
    assert!(
        !fx.iter().any(|e| matches!(e, ShellEffect::Action { .. })),
        "{fx:?}"
    );
    assert_eq!(shell.info_line().name, "Rescan this folder: Still scanning");

    // Done: the key works, and Shift+F5 is the same key.
    shell.set_status(String::new(), false);
    let fx = shell.update(InputEvent::Key(Key::Rescan), t(20_000));
    assert!(
        fx.iter().any(|e| matches!(
            e,
            ShellEffect::Action { action, items, .. }
                if *action == bs_ui::action::Action::Rescan && items == &vec![folder.id]
        )),
        "{fx:?}"
    );

    // A full scan holds it too, with its own reason.
    shell.set_offer(Offer::Running);
    let fx = shell.update(InputEvent::Key(Key::Rescan), t(40_000));
    assert!(!fx.iter().any(|e| matches!(e, ShellEffect::Action { .. })));
    assert_eq!(
        shell.info_line().name,
        "Rescan this folder: A full scan is running"
    );
}

// ---- views inside the replaced part ------------------------------------------

#[test]
fn a_view_inside_the_replaced_part_moves_to_the_nearest_live_folder() {
    let mut agg = finished_tree();
    let mut shell = shell_over(agg.publish());
    let a_item = item_named(&shell, "a");
    let root_view = shell.app().unwrap().current_view();

    // Into a, then into a/x.
    shell.app_mut().unwrap().open(a_item.id);
    let a_view = shell.app().unwrap().current_view();
    assert_ne!(a_view, root_view);
    let x_item = item_named(&shell, "x");
    shell.app_mut().unwrap().open(x_item.id);
    let x_view = shell.app().unwrap().current_view();
    assert_ne!(x_view, a_view);
    assert_eq!(shell.app().unwrap().depth(), 3);
    let deep = item_named(&shell, "deep0.bin").id;
    shell.app_mut().unwrap().select(vec![deep]);

    let a = start_reading_a(&mut agg);
    shell
        .app_mut()
        .unwrap()
        .source_mut()
        .set_rescanning(Some(a));
    let fx = show(&mut shell, agg.publish(), 100);

    let app = shell.app().unwrap();
    assert!(
        fx.iter()
            .any(|e| matches!(e, bs_ui::Effect::ViewChanged(_))),
        "the view changed"
    );
    assert_eq!(app.current_view(), a_view, "one level up, to a");
    assert_eq!(app.depth(), 2);
    assert!(
        !app.history().contains(&x_view),
        "history no longer reaches the cut-off folder"
    );
    assert!(app.selection().is_empty(), "the old selection was inside");
    // Nothing on screen is a cut-off node.
    let snapshot = app.source().snapshot().clone();
    for item in app.items().iter() {
        if let Some(node) = snapshot.node(NodeId::from_index(item.id)) {
            assert!(!node.is_detached(), "{:?} is cut off", item.label);
        }
    }
    // The breadcrumb is a/ now, not a/x.
    assert_eq!(app.source().breadcrumb(a_view), "/data  ›  a");
    assert!(app.source().view_is_gone(x_view));
    assert!(!app.source().view_is_gone(a_view));
}

#[test]
fn the_folder_you_are_in_stays_when_it_is_the_one_read_again() {
    let mut agg = finished_tree();
    let mut shell = shell_over(agg.publish());
    let a_item = item_named(&shell, "a");
    shell.app_mut().unwrap().open(a_item.id);
    let a_view = shell.app().unwrap().current_view();

    let a = start_reading_a(&mut agg);
    shell
        .app_mut()
        .unwrap()
        .source_mut()
        .set_rescanning(Some(a));
    let fx = show(&mut shell, agg.publish(), 100);
    assert!(
        !fx.iter()
            .any(|e| matches!(e, bs_ui::Effect::ViewChanged(_))),
        "the view is the folder itself, which keeps its id"
    );
    assert_eq!(shell.app().unwrap().current_view(), a_view);
    // It shows what the scan has found so far.
    let names: Vec<String> = shell
        .app()
        .unwrap()
        .items()
        .iter()
        .filter_map(|i| i.label.as_ref().map(|l| l.name.to_string()))
        .collect();
    assert!(names.iter().any(|n| n == "y"), "{names:?}");
    assert!(!names.iter().any(|n| n == "inside.bin"), "{names:?}");
}

// ---- a read that did not work --------------------------------------------------

#[test]
fn a_failed_read_says_so_on_the_folder_and_keeps_its_numbers() {
    let mut agg = finished_tree();
    let snap = agg.publish();
    let a = find(&snap, &["a"]);
    let before = snap.node(a).unwrap().allocated();
    let mut shell = shell_over(snap);

    // Reading it again changed nothing (the folder could not be opened).
    shell.app_mut().unwrap().source_mut().set_folder_problem(
        a,
        Some("it can't be opened (No such file or directory (os error 2))".into()),
    );
    let item = item_named(&shell, "a");
    show_card(&mut shell, &item, 100);
    let texts = card_texts(&shell).join("\n");
    assert!(
        texts.contains("Couldn't read this folder again: it can't be opened"),
        "{texts}"
    );
    assert!(
        texts.contains("The sizes shown are from the earlier scan"),
        "{texts}"
    );
    assert_eq!(
        shell
            .app()
            .unwrap()
            .source()
            .snapshot()
            .node(a)
            .unwrap()
            .allocated(),
        before
    );

    // Reading it again starts afresh: the old complaint goes.
    let source = shell.app_mut().unwrap().source_mut();
    source.set_rescanning(Some(a));
    source.set_rescanning(None);
    shell.update(InputEvent::PointerMoved { x: 1.0, y: 1.0 }, t(1000));
    shell.tick(t(1100));
    show_card(&mut shell, &item, 1200);
    let texts = card_texts(&shell).join("\n");
    assert!(
        !texts.contains("Couldn't read this folder again"),
        "{texts}"
    );
}

#[test]
fn a_rescan_that_names_another_folder_is_refused_and_the_tree_stays() {
    let mut agg = finished_tree();
    let snap = agg.publish();
    let a = find(&snap, &["a"]);
    let before = snap.node(a).unwrap().allocated();
    agg.begin_rescan(a, Some(b"/data/a")).unwrap();
    // The path now leads somewhere else, so the scan's root has another name.
    let mut b = Batch::new();
    b.dir(DirKey(0), DirKey::NONE, b"/elsewhere", &meta(4096))
        .unwrap();
    b.listing_done(DirKey(0));
    agg.apply(&b);
    assert!(agg.rescan_refused());
    agg.finish();
    let report = agg.rescan_report().expect("a report");
    assert!(report.refused && !report.replaced);
    let after = agg.publish();
    assert_eq!(after.node(a).unwrap().allocated(), before);
    assert!(after.scan_complete());
}

// ---- no date for a time of 0 ---------------------------------------------------

fn tree_with_unknown_time() -> Arc<Snapshot> {
    let mut agg = Aggregator::new();
    let mut b = Batch::new();
    b.dir(DirKey(0), DirKey::NONE, b"/data", &meta(4096))
        .unwrap();
    let unknown = Meta {
        mtime: 0,
        ..meta(4096)
    };
    b.dir(DirKey(1), DirKey(0), b"stand-in", &unknown).unwrap();
    b.dir(DirKey(2), DirKey(0), b"dated", &meta(4096)).unwrap();
    for k in [1u32, 2] {
        for f in 0..20u32 {
            b.file(
                NodeKind::File,
                DirKey(k),
                format!("file{f}").as_bytes(),
                &meta(1_000_000 + u64::from(f)),
            )
            .unwrap();
        }
    }
    for k in 0..3 {
        b.listing_done(DirKey(k));
    }
    agg.apply(&b);
    agg.publish()
}

fn accessible_text(shell: &mut Shell<TreeSource>, now: u64) -> String {
    let mut tree = AccessTree::new();
    let update = shell.access_update(&mut tree, t(now));
    let consumer = Tree::new(update, true);
    let mut out = Vec::new();
    fn walk(n: accesskit_consumer::NodeRef<'_>, out: &mut Vec<String>) {
        for text in [n.label(), n.value(), n.description()]
            .into_iter()
            .flatten()
        {
            out.push(text);
        }
        for c in n.children() {
            walk(c, out);
        }
    }
    walk(consumer.state().root(), &mut out);
    out.join("\n")
}

#[test]
fn a_time_of_zero_shows_no_date_on_the_card_or_to_a_screen_reader() {
    let mut shell = shell_over(tree_with_unknown_time());
    let unknown = item_named(&shell, "stand-in");
    show_card(&mut shell, &unknown, 100);
    let texts = card_texts(&shell).join("\n");
    assert!(!texts.contains("modified"), "{texts}");
    assert!(!texts.contains("1970"), "{texts}");
    let spoken = accessible_text(&mut shell, 200);
    assert!(!spoken.contains("1970"), "{spoken}");
    assert!(!spoken.contains("modified"), "{spoken}");

    // A folder with a real time still shows its date.
    shell.update(InputEvent::PointerMoved { x: 1.0, y: 1.0 }, t(1000));
    shell.tick(t(1100));
    let dated = item_named(&shell, "dated");
    show_card(&mut shell, &dated, 1200);
    let texts = card_texts(&shell).join("\n");
    assert!(texts.contains("modified 2024-03-0"), "{texts}");
}

#[test]
fn the_card_never_formats_a_time_of_zero() {
    // The card's own rule, whatever the source says.
    let _ = Rect::new(0.0, 0.0, 1.0, 1.0);
    let snapshot = tree_with_unknown_time();
    let source = TreeSource::new(snapshot, Metrics::from_logical(13.0, 1.0)).unwrap();
    let view = source.root();
    let stand_in = {
        let mut source = source;
        let layout = source.layout(view, Rect::new(0.0, 0.0, 1000.0, 700.0));
        let id = layout
            .items
            .iter()
            .find(|i| i.label.as_deref().is_some_and(|l| &*l.name == "stand-in"))
            .unwrap()
            .id;
        source.card(view, id).unwrap()
    };
    assert_eq!(stand_in.modified, None);
}

// ---- an empty folder says so ---------------------------------------------------

fn chrome_words(shell: &mut Shell<TreeSource>) -> Vec<(String, Rect)> {
    shell
        .chrome(t(0))
        .widgets
        .iter()
        .filter_map(|w| match w {
            bs_render::Widget::Text { rect, text, .. } => Some((text.to_string(), *rect)),
            _ => None,
        })
        .collect()
}

fn says_empty(shell: &mut Shell<TreeSource>) -> bool {
    chrome_words(shell)
        .iter()
        .any(|(t, _)| t == bs_ui::shell::EMPTY_FOLDER)
}

#[test]
fn an_empty_folder_says_so_in_the_middle_of_the_map() {
    let mut agg = Aggregator::new();
    let mut b = Batch::new();
    b.dir(DirKey(0), DirKey::NONE, b"/empty", &meta(4096))
        .unwrap();
    b.listing_done(DirKey(0));
    agg.apply(&b);
    agg.finish();
    let mut shell = shell_over(agg.publish());
    let map = shell.map_rect();
    let words = chrome_words(&mut shell);
    let (_, rect) = words
        .iter()
        .find(|(t, _)| t == bs_ui::shell::EMPTY_FOLDER)
        .expect("the words for an empty folder");
    assert_eq!(bs_ui::shell::EMPTY_FOLDER, "This folder is empty");
    let (cx, cy) = (rect.x + rect.w / 2.0, rect.y + rect.h / 2.0);
    assert!((cx - (map.x + map.w / 2.0)).abs() <= 2.0, "centred across");
    assert!((cy - (map.y + map.h / 2.0)).abs() <= 2.0, "centred down");

    // A folder with contents says nothing of the kind, and neither does one
    // still being counted.
    let mut full = shell_over(finished_tree().publish());
    assert!(!says_empty(&mut full));
    let mut counting = Aggregator::new();
    let mut b = Batch::new();
    b.dir(DirKey(0), DirKey::NONE, b"/busy", &meta(4096))
        .unwrap();
    counting.apply(&b);
    let mut busy = shell_over(counting.publish());
    assert!(!says_empty(&mut busy));
}
