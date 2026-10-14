//! The tree-backed view source driving the app: real layouts, navigation by
//! the same events a user produces, and a scan that keeps adding data.

use std::sync::Arc;
use std::time::Duration;

use bs_layout::Metrics;
use bs_render::{ItemKind, Rect, ZoomDirection};
use bs_tree::{Aggregator, Batch, Category, DirKey, Meta, NodeKind, Snapshot};
use bs_ui::tree::{TreeSource, scan_status};
use bs_ui::{App, Effect, InputEvent, Key, ViewSource};

fn meta(size: u64) -> Meta {
    Meta {
        logical: size,
        allocated: size,
        category: Category(1),
        ..Meta::default()
    }
}

/// root (key 0) with `dirs` folders (keys 1..) holding `files` files each.
struct Builder {
    agg: Aggregator,
    dirs: u32,
    files: u32,
}

impl Builder {
    fn new(dirs: u32, files: u32) -> Builder {
        Builder {
            agg: Aggregator::new(),
            dirs,
            files,
        }
    }

    fn dir_batch(&mut self, only: Option<u32>, finish: bool) {
        let mut b = Batch::new();
        if only.is_none() {
            b.dir(DirKey(0), DirKey::NONE, b"/data", &meta(4096))
                .unwrap();
        }
        for d in 1..=self.dirs {
            if only.is_some_and(|o| o != d) {
                continue;
            }
            let name = format!("folder{d}");
            b.dir(DirKey(d), DirKey(0), name.as_bytes(), &meta(4096))
                .unwrap();
            for f in 0..self.files {
                let size = 1_000_000 * u64::from(d) + u64::from(f);
                let name = format!("file{f}");
                b.file(NodeKind::File, DirKey(d), name.as_bytes(), &meta(size))
                    .unwrap();
            }
            if finish {
                b.listing_done(DirKey(d));
            }
        }
        if finish && only.is_none() {
            b.listing_done(DirKey(0));
        }
        self.agg.apply(&b);
    }

    fn snapshot(&mut self) -> Arc<Snapshot> {
        self.agg.publish()
    }
}

const MAP: Rect = Rect::new(0.0, 30.0, 1200.0, 700.0);

fn metrics() -> Metrics {
    Metrics::from_logical(13.0, 1.0)
}

fn app_for(snapshot: Arc<Snapshot>) -> App<TreeSource> {
    let source = TreeSource::new(snapshot, metrics()).expect("root");
    App::new(source, MAP)
}

fn t(ms: u64) -> Duration {
    Duration::from_millis(ms)
}

/// The center of the first nested folder box under the root.
fn folder_center(app: &App<TreeSource>) -> (f32, f32) {
    let items = app.items();
    let it = items
        .iter()
        .find(|i| i.depth == 1 && i.kind == ItemKind::FolderTitled)
        .expect("a nested folder");
    (it.rect.x + it.rect.w / 2.0, it.rect.y + it.rect.h / 2.0)
}

/// A point on the title bar of the first nested folder: the folder itself is
/// under the pointer there, not a file inside it.
fn folder_title(app: &App<TreeSource>) -> (f32, f32) {
    let items = app.items();
    let it = items
        .iter()
        .find(|i| i.depth == 1 && i.kind == ItemKind::FolderTitled)
        .expect("a nested folder");
    (it.rect.x + it.rect.w / 2.0, it.rect.y + 3.0)
}

#[test]
fn an_empty_snapshot_has_no_source_yet() {
    let agg = Aggregator::new();
    assert!(TreeSource::new(agg.reader().load(), metrics()).is_none());
}

#[test]
fn the_root_view_is_real_layout_output() {
    let mut b = Builder::new(3, 20);
    b.dir_batch(None, true);
    let app = app_for(b.snapshot());
    let items = app.items();
    assert_eq!(items[0].rect, MAP);
    assert_eq!(items[0].kind, ItemKind::FolderTitled);
    assert_eq!(items[0].label.as_ref().unwrap().name.as_ref(), "/data");
    assert!(
        items
            .iter()
            .filter(|i| i.depth == 1)
            .all(|i| i.kind == ItemKind::FolderTitled)
    );
}

#[test]
fn wheel_goes_into_a_folder_and_back_out_with_a_breadcrumb() {
    let mut b = Builder::new(3, 20);
    b.dir_batch(None, true);
    let mut app = app_for(b.snapshot());
    let (x, y) = folder_center(&app);
    let fx = app.update(InputEvent::Wheel { x, y, steps: 1.0 }, t(0));
    let Some(Effect::Zoom {
        direction, items, ..
    }) = fx.first()
    else {
        panic!("expected a zoom, got {fx:?}");
    };
    assert_eq!(*direction, ZoomDirection::In);
    assert!(items[0].label.as_ref().unwrap().name.starts_with("folder"));
    assert_eq!(app.depth(), 2);
    let view = app.current_view();
    let crumb = app.source_mut().breadcrumb(view);
    assert!(crumb.starts_with("/data  ›  folder"), "{crumb}");
    let fx = app.update(InputEvent::Key(Key::Backspace), t(500));
    assert!(matches!(
        fx.first(),
        Some(Effect::Zoom {
            direction: ZoomDirection::Out,
            ..
        })
    ));
    assert_eq!(app.depth(), 1);
}

#[test]
fn history_survives_pruning_and_returns_to_the_same_views() {
    let mut b = Builder::new(3, 20);
    b.dir_batch(None, true);
    let mut app = app_for(b.snapshot());
    let (x, y) = folder_title(&app);
    app.update(InputEvent::DoubleClick { x, y }, t(0));
    let inside = app.current_view();
    app.update(InputEvent::Key(Key::Home), t(100));
    assert_eq!(app.depth(), 1);
    app.update(InputEvent::Key(Key::AltLeft), t(200));
    assert_eq!(app.current_view(), inside);
    assert_eq!(app.depth(), 2);
    assert!(!app.items().is_empty());
}

#[test]
fn new_data_relays_out_the_current_view_and_keeps_selection() {
    let mut b = Builder::new(4, 0);
    // Folders exist but are not yet filled: the scan is mid-flight.
    b.dir_batch(None, false);
    let first = b.snapshot();
    let mut app = app_for(first);
    assert!(!app.items().is_empty());
    let root_label = |app: &App<TreeSource>| app.items()[0].label.as_ref().unwrap().size.clone();
    let before = root_label(&app);
    assert!(before.starts_with("Scanning… · ≥"), "{before}");

    // Select the root box, then more of the scan arrives.
    app.update(InputEvent::Click { x: 5.0, y: 35.0 }, t(0));
    assert_eq!(app.selected(), Some(0));
    b.files = 30;
    for d in 1..=4 {
        b.dir_batch(Some(d), true);
    }
    let mut next = Batch::new();
    next.listing_done(DirKey(0));
    b.agg.apply(&next);
    let done = b.snapshot();
    app.source_mut().set_snapshot(Arc::clone(&done));
    let fx = app.update(InputEvent::DataChanged, t(300));
    assert!(matches!(fx.first(), Some(Effect::SetView(_))));
    assert!(
        fx.iter().any(|e| matches!(e, Effect::SetSelected(Some(0)))),
        "selection is given back: {fx:?}"
    );
    let after = root_label(&app);
    assert!(!after.contains("Scanning"), "{after}");
    assert!(app.items().iter().any(|i| i.depth == 1 && i.rect.w > 100.0));
    assert!(
        scan_status(&done).contains("in 120 files"),
        "{}",
        scan_status(&done)
    );
}

#[test]
fn a_view_left_behind_by_new_data_is_refreshed_when_it_comes_back() {
    let mut b = Builder::new(2, 40);
    b.dir_batch(None, false);
    let mut app = app_for(b.snapshot());
    let (x, y) = folder_title(&app);
    app.update(InputEvent::DoubleClick { x, y }, t(0));
    assert_eq!(app.depth(), 2);
    // The scan finishes while the user is inside the folder.
    let mut done = Batch::new();
    for d in 1..=2 {
        done.listing_done(DirKey(d));
    }
    done.listing_done(DirKey(0));
    b.agg.apply(&done);
    app.source_mut().set_snapshot(b.snapshot());
    app.update(InputEvent::DataChanged, t(100));
    let fx = app.update(InputEvent::Key(Key::Backspace), t(600));
    let Some(Effect::Zoom { items, .. }) = fx.first() else {
        panic!("expected a zoom out, got {fx:?}");
    };
    assert!(
        !items[0].label.as_ref().unwrap().size.contains("Scanning"),
        "the parent view shows the finished scan"
    );
}

#[test]
fn hostile_names_never_break_layout_or_the_breadcrumb() {
    let mut b = Builder::new(0, 0);
    let mut batch = Batch::new();
    batch
        .dir(DirKey(0), DirKey::NONE, b"/data", &meta(4096))
        .unwrap();
    let mut long = vec![b'a'; 4000];
    long.extend_from_slice("\u{202e}\u{200b}e\u{301}\u{301}".as_bytes());
    let names: [&[u8]; 3] = [&long, b"bad\xff\xfename", "שלום עולם".as_bytes()];
    for (i, name) in names.iter().enumerate() {
        let key = DirKey(i as u32 + 1);
        batch.dir(key, DirKey(0), name, &meta(4096)).unwrap();
        for f in 0..5u64 {
            batch
                .file(NodeKind::File, key, b"x", &meta(1_000_000 + f))
                .unwrap();
        }
        batch.listing_done(key);
    }
    batch.listing_done(DirKey(0));
    b.agg.apply(&batch);
    let mut app = app_for(b.snapshot());
    let (x, y) = folder_title(&app);
    app.update(InputEvent::DoubleClick { x, y }, t(0));
    let view = app.current_view();
    let crumb = app.source_mut().breadcrumb(view);
    assert!(crumb.contains('›'));
    assert!(!app.items().is_empty());
}

#[test]
fn an_unknown_view_lays_out_to_nothing() {
    let mut b = Builder::new(1, 3);
    b.dir_batch(None, true);
    let mut source = TreeSource::new(b.snapshot(), metrics()).unwrap();
    assert!(source.layout(9_999, MAP).items.is_empty());
}

#[test]
fn status_text_keeps_unfinished_counts_honest() {
    let empty = Aggregator::new().reader().load();
    assert_eq!(scan_status(&empty), "Scanning…");

    // Folders still being counted: the size is a lower bound.
    let mut b = Builder::new(2, 5);
    b.dir_batch(None, false);
    let running = scan_status(&b.snapshot());
    assert!(running.starts_with("Scanning…"), "{running}");
    assert!(running.contains("≥ "), "{running}");

    // A stopped scan says its results are incomplete.
    b.agg.mark_stopped();
    let stopped = scan_status(&b.agg.publish());
    assert!(
        stopped.starts_with("Scan stopped — results incomplete"),
        "{stopped}"
    );

    // None of the wording the product avoids.
    for text in [&running, &stopped] {
        let lower = text.to_lowercase();
        for banned in ["freed", "safe", "junk", "reclaim", "clean up"] {
            assert!(!lower.contains(banned), "{text}");
        }
    }
}
