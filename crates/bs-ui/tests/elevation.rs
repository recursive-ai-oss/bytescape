//! The Full scan button, the Unreadable block, keeping the user's place when a
//! full result arrives, and deletes that need administrator rights. All of it
//! is driven through the shell with a scripted stand-in for the platform.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bs_knowledge::{Level, Reason};
use bs_layout::Metrics;
use bs_platform::delete::{
    Decision, EmptyOutcome, Item as DItem, OpError, Plan, PlanEntry, ProtectedConfirmation,
    RefuseReason, RestoreOutcome,
};
use bs_render::{Item, ItemKind, Rect, TextEngine};
use bs_tree::{Aggregator, Batch, Category, DeleteState, DirKey, Meta, NodeKind, Snapshot};
use bs_ui::blocks::DriveBlocks;
use bs_ui::capacity::Breakdown;
use bs_ui::chrome::TextMeasure;
use bs_ui::elevation::{self, ElevatedEnd, ElevatedItem, Offer, Place, UnreadableAs};
use bs_ui::shell::{
    ActOutcome, BinStyle, Disposer, Look, Shell, ShellEffect, Token, TreeChange,
    access::{AccessTarget, AccessTree},
};
use bs_ui::tree::TreeSource;
use bs_ui::{App, InputEvent, Key, ViewSource};

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

fn t(ms: u64) -> Duration {
    Duration::from_millis(ms)
}

fn center(r: Rect) -> (f32, f32) {
    (r.x + r.w / 2.0, r.y + r.h / 2.0)
}

/// A tree of `/data` with the given folders, each holding `inner` subfolders
/// of files, so there is something to go into.
fn build(names: &[&str], extra: Option<&str>) -> (Aggregator, Arc<Snapshot>) {
    let mut agg = Aggregator::new();
    let mut b = Batch::new();
    b.dir(DirKey(0), DirKey::NONE, b"/data", &meta(4096))
        .unwrap();
    let mut next = 1u32;
    for (i, name) in names.iter().enumerate() {
        let key = DirKey(next);
        next += 1;
        b.dir(key, DirKey(0), name.as_bytes(), &meta(4096)).unwrap();
        let sub = DirKey(next);
        next += 1;
        b.dir(sub, key, b"inner", &meta(4096)).unwrap();
        for f in 0..10u32 {
            let size = 1_000_000 * (names.len() as u64 - i as u64) + u64::from(f);
            b.file(
                NodeKind::File,
                sub,
                format!("file{f}").as_bytes(),
                &meta(size),
            )
            .unwrap();
        }
        b.listing_done(sub);
        b.listing_done(key);
    }
    if let Some(extra) = extra {
        // A folder the other tree does not have.
        let key = DirKey(next);
        b.dir(key, DirKey(0), extra.as_bytes(), &meta(4096))
            .unwrap();
        b.file(NodeKind::File, key, b"only-here", &meta(9_000_000))
            .unwrap();
        b.listing_done(key);
    }
    b.listing_done(DirKey(0));
    agg.apply(&b);
    let snap = agg.publish();
    (agg, snap)
}

fn shell_for(snapshot: Arc<Snapshot>, scale: f32) -> Shell<TreeSource> {
    let size = ((1200.0 * scale) as u32, (800.0 * scale) as u32);
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

fn blocks(unreadable_as: UnreadableAs) -> DriveBlocks {
    DriveBlocks {
        free: 112_000_000_000,
        unreadable: 23_400_000_000,
        reserved: 0,
        recycle_bin: 0,
        quota: false,
        unreadable_as,
    }
}

fn show_blocks(shell: &mut Shell<TreeSource>, b: DriveBlocks) {
    let app = shell.app_mut().unwrap();
    app.source_mut().set_drive_blocks(Some(b));
    app.update(InputEvent::DataChanged, t(1));
}

fn unreadable_box(shell: &Shell<TreeSource>) -> Item {
    shell
        .app()
        .unwrap()
        .items()
        .iter()
        .find(|i| i.kind == ItemKind::Unreadable)
        .cloned()
        .expect("the Unreadable block")
}

fn info(shell: &Shell<TreeSource>) -> String {
    let l = shell.info_line();
    format!("{}{}", l.name, l.details)
}

// ---- the button -------------------------------------------------------------

fn full_scan_rect(shell: &mut Shell<TreeSource>) -> Option<Rect> {
    shell.geometry().full_scan
}

#[test]
fn the_button_sits_at_the_right_end_of_the_bar_and_takes_its_room_from_the_breadcrumb() {
    let (_, snap) = build(&["a", "b", "c"], None);
    let mut shell = shell_for(snap, 1.0);
    let g = shell.geometry().clone();
    let rect = g.full_scan.expect("offered by default");
    assert!(rect.x > g.crumb.x && rect.right() <= g.nav.right());
    assert!(
        g.crumb.right() <= rect.x,
        "the crumbs end before the button"
    );
    assert_eq!(
        g.hit(rect.x + 2.0, rect.y + 2.0),
        Some(bs_ui::chrome::Hit::FullScan)
    );
    // It reads "Full scan".
    let chrome = shell.chrome(t(0));
    let found = chrome.widgets.iter().any(|w| {
        matches!(w, bs_render::Widget::Text { text, .. } if &**text == elevation::FULL_SCAN_LABEL)
    });
    assert!(found, "no Full scan text in the bar");
}

#[test]
fn it_is_hidden_when_elevation_is_not_available_and_when_the_full_scan_is_done() {
    let (_, snap) = build(&["a", "b"], None);
    let mut shell = shell_for(snap, 1.0);
    for offer in [
        Offer::Hidden,
        Offer::Done,
        Offer::Explained("x".to_string()),
        Offer::for_unavailable(bs_platform::elevate::Unavailable::NoElevationService),
    ] {
        shell.set_offer(offer.clone());
        assert!(full_scan_rect(&mut shell).is_none(), "{offer:?}");
    }
    shell.set_offer(Offer::Available);
    assert!(full_scan_rect(&mut shell).is_some());
    shell.set_offer(Offer::Running);
    assert!(full_scan_rect(&mut shell).is_some(), "it stays, disabled");
}

#[test]
fn pressing_it_asks_the_host_for_a_full_scan_once() {
    let (_, snap) = build(&["a", "b"], None);
    let mut shell = shell_for(snap, 1.0);
    let rect = full_scan_rect(&mut shell).unwrap();
    let (x, y) = center(rect);
    let fx = shell.update(InputEvent::Click { x, y }, t(5));
    assert!(
        fx.iter().any(|e| matches!(e, ShellEffect::FullScan)),
        "{fx:?}"
    );
    assert!(
        info(&shell).contains("administrator helper"),
        "{}",
        info(&shell)
    );
    // While it runs, pressing again only says so.
    shell.set_offer(Offer::Running);
    let rect = full_scan_rect(&mut shell).unwrap();
    let (x, y) = center(rect);
    let fx = shell.update(InputEvent::Click { x, y }, t(10));
    assert!(!fx.iter().any(|e| matches!(e, ShellEffect::FullScan)));
    assert_eq!(info(&shell), "The full scan is already running");
}

#[test]
fn the_pointer_cursor_and_tooltip_follow_the_button() {
    let (_, snap) = build(&["a", "b"], None);
    let mut shell = shell_for(snap, 1.0);
    let rect = full_scan_rect(&mut shell).unwrap();
    let (x, y) = center(rect);
    shell.update(InputEvent::PointerMoved { x, y }, t(5));
    assert!(shell.wants_pointer_cursor());
    assert!(
        info(&shell).contains("administrator rights"),
        "{}",
        info(&shell)
    );
    shell.set_offer(Offer::Running);
    shell.update(InputEvent::PointerMoved { x: 1.0, y: 1.0 }, t(6));
    let rect = full_scan_rect(&mut shell).unwrap();
    let (x, y) = center(rect);
    shell.update(InputEvent::PointerMoved { x, y }, t(7));
    assert!(
        !shell.wants_pointer_cursor(),
        "a running scan can't be started again"
    );
    assert_eq!(info(&shell), "A full scan is running");
}

#[test]
fn the_button_is_an_accessible_control_with_a_reason_when_disabled() {
    let (_, snap) = build(&["a", "b"], None);
    let mut shell = shell_for(snap, 1.0);
    let mut tree = AccessTree::new();
    shell.access_update(&mut tree, t(0));
    let id = tree.node_for(AccessTarget::FullScan).expect("a node");
    let n = tree.get(id).unwrap();
    assert_eq!(n.label(), Some("Full scan"));
    assert!(!n.is_disabled());
    // Clicking it from a screen reader acts like the pointer.
    let request = accesskit::ActionRequest {
        action: accesskit::Action::Click,
        target_tree: accesskit::TreeId::ROOT,
        target_node: id,
        data: None,
    };
    let fx = shell.access_action(&tree, &request, t(5));
    assert!(
        fx.iter().any(|e| matches!(e, ShellEffect::FullScan)),
        "{fx:?}"
    );
    shell.set_offer(Offer::Running);
    shell.access_update(&mut tree, t(10));
    let id = accesskit::NodeId(31);
    assert!(tree.get(id).is_some(), "still there");
    let n = tree.get(id).unwrap();
    assert!(n.is_disabled());
    assert_eq!(n.description(), Some("A full scan is running"));
    // Hidden: no node at all.
    shell.set_offer(Offer::Hidden);
    shell.access_update(&mut tree, t(20));
    assert!(tree.get(accesskit::NodeId(31)).is_none());
}

// ---- the Unreadable block ---------------------------------------------------

#[test]
fn the_block_invites_and_a_click_starts_the_full_scan() {
    let (_, snap) = build(&["a", "b", "c"], None);
    let mut shell = shell_for(snap, 1.0);
    let ua = shell.unreadable_as();
    show_blocks(&mut shell, blocks(ua));
    let block = unreadable_box(&shell);
    let label = block.label.as_ref().unwrap().name.to_string();
    assert_eq!(label, "Couldn't read: 23.4 GB — Grant access to see inside");
    let (x, y) = center(block.rect);
    let fx = shell.update(InputEvent::Click { x, y }, t(5));
    assert!(
        fx.iter().any(|e| matches!(e, ShellEffect::FullScan)),
        "{fx:?}"
    );
}

#[test]
fn while_the_full_scan_runs_the_block_says_so_and_a_click_does_not_start_another() {
    let (_, snap) = build(&["a", "b", "c"], None);
    let mut shell = shell_for(snap, 1.0);
    assert!(
        shell.set_offer(Offer::Running),
        "the block must be reworded"
    );
    let ua = shell.unreadable_as();
    show_blocks(&mut shell, blocks(ua));
    let block = unreadable_box(&shell);
    assert!(
        block
            .label
            .as_ref()
            .unwrap()
            .name
            .contains("Reading with administrator rights"),
    );
    let (x, y) = center(block.rect);
    let fx = shell.update(InputEvent::Click { x, y }, t(5));
    assert!(!fx.iter().any(|e| matches!(e, ShellEffect::FullScan)));
}

#[test]
fn after_the_full_scan_the_gap_is_filesystem_overhead_and_not_clickable() {
    let (_, snap) = build(&["a", "b", "c"], None);
    let mut shell = shell_for(snap, 1.0);
    assert!(shell.set_offer(Offer::Done));
    let ua = shell.unreadable_as();
    show_blocks(&mut shell, blocks(ua));
    let block = unreadable_box(&shell);
    let name = block.label.as_ref().unwrap().name.to_string();
    assert!(name.starts_with("Filesystem overhead: 23.4 GB"), "{name}");
    assert!(!name.contains("Couldn't read") && !name.contains("Grant access"));
    let (x, y) = center(block.rect);
    shell.update(InputEvent::PointerMoved { x, y }, t(4));
    assert!(
        info(&shell).starts_with("Filesystem overhead"),
        "{}",
        info(&shell)
    );
    let fx = shell.update(InputEvent::Click { x, y }, t(5));
    assert!(!fx.iter().any(|e| matches!(e, ShellEffect::FullScan)));
    assert!(
        info(&shell).contains("filesystem's own overhead"),
        "{}",
        info(&shell)
    );
    // The capacity bar says the same.
    shell.set_capacity(
        Breakdown {
            used: 40_000_000_000,
            recycle_bin: 0,
            unreadable: 23_400_000_000,
            reserved: 0,
            free: 36_600_000_000,
            total: 100_000_000_000,
            quota: false,
        },
        t(0),
    );
    let g = shell.geometry().capacity;
    shell.update(
        InputEvent::PointerMoved {
            x: g.x + g.w * 0.5,
            y: g.y + g.h / 2.0,
        },
        t(10),
    );
    let tip = info(&shell);
    assert!(tip.contains("Filesystem overhead 23.4 GB"), "{tip}");
    assert!(!tip.contains("Couldn't read"), "{tip}");
}

#[test]
fn an_edition_that_cannot_elevate_explains_itself_where_the_offer_would_be() {
    let (_, snap) = build(&["a", "b", "c"], None);
    for (offer, expect) in [
        (
            Offer::walk_only_edition(),
            "System folders can't be scanned in this edition",
        ),
        (
            Offer::for_unavailable(bs_platform::elevate::Unavailable::SnapConfined),
            "System folders can't be scanned in this edition",
        ),
        (
            Offer::for_unavailable(bs_platform::elevate::Unavailable::AppImage),
            "AppImage",
        ),
    ] {
        let mut shell = shell_for(snap.clone(), 1.0);
        shell.set_offer(offer.clone());
        assert!(full_scan_rect(&mut shell).is_none(), "the button is gone");
        let ua = shell.unreadable_as();
        show_blocks(&mut shell, blocks(ua));
        let block = unreadable_box(&shell);
        assert!(
            block
                .label
                .as_ref()
                .unwrap()
                .name
                .contains("scanned in this edition"),
            "{:?}",
            block.label
        );
        let (x, y) = center(block.rect);
        let fx = shell.update(InputEvent::Click { x, y }, t(5));
        assert!(!fx.iter().any(|e| matches!(e, ShellEffect::FullScan)));
        assert!(info(&shell).contains(expect), "{}", info(&shell));
        // Clicking opens the page where the full version is, nothing else.
        assert!(
            fx.iter().any(|e| matches!(e, ShellEffect::OpenUrl(_))),
            "{fx:?}"
        );
    }
}

#[test]
fn other_reasons_hide_the_offer_and_the_block_keeps_its_plain_words() {
    let (_, snap) = build(&["a", "b", "c"], None);
    let mut shell = shell_for(snap, 1.0);
    shell.set_offer(Offer::for_unavailable(
        bs_platform::elevate::Unavailable::HelperNotInstalled,
    ));
    let ua = shell.unreadable_as();
    show_blocks(&mut shell, blocks(ua));
    let block = unreadable_box(&shell);
    assert!(
        block
            .label
            .as_ref()
            .unwrap()
            .name
            .starts_with("Couldn't read")
    );
    let (x, y) = center(block.rect);
    let fx = shell.update(InputEvent::Click { x, y }, t(5));
    assert!(!fx.iter().any(|e| matches!(e, ShellEffect::FullScan)));
    assert_eq!(
        info(&shell),
        bs_platform::elevate::Unavailable::HelperNotInstalled.explanation(),
        "the platform's own explanation is shown where the offer would be"
    );
    assert!(
        block
            .label
            .as_ref()
            .unwrap()
            .name
            .contains("isn't available here")
    );
}

// ---- keeping the place ------------------------------------------------------

fn enter_folder(shell: &mut Shell<TreeSource>, name: &str) {
    let app = shell.app().unwrap();
    let view = app.current_view();
    let id = app
        .items()
        .iter()
        .find(|i| i.label.as_ref().is_some_and(|l| &*l.name == name))
        .map(|i| i.id)
        .unwrap_or_else(|| panic!("no folder {name}"));
    let _ = view;
    shell.app_mut().unwrap().open(id);
}

fn names_of(app: &App<TreeSource>) -> Vec<String> {
    app.source().trail(app.current_view())
}

#[test]
fn the_view_and_the_selection_survive_a_swap_to_the_full_tree() {
    let (_, walk) = build(&["alpha", "beta", "gamma"], None);
    let mut shell = shell_for(walk, 1.0);
    enter_folder(&mut shell, "beta");
    // Select a file box inside beta's inner folder.
    enter_folder(&mut shell, "inner");
    let file_id = {
        let app = shell.app().unwrap();
        app.items()
            .iter()
            .find(|i| i.kind == ItemKind::FileTile)
            .map(|i| i.id)
            .expect("a file box")
    };
    shell.app_mut().unwrap().select(vec![file_id]);
    let before_name = {
        let app = shell.app().unwrap();
        let v = app.current_view();
        app.source().target(v, file_id).unwrap().name
    };
    let place = elevation::capture(shell.app().unwrap());
    assert_eq!(place.depth(), 2);
    assert_eq!(place.selected(), 1);

    // The full tree has the same places, plus a folder the walk could not read.
    let (_, full) = build(&["alpha", "beta", "gamma"], Some("system"));
    let map = shell.map_rect();
    let source = TreeSource::new(full, Metrics::from_logical(13.0, 1.0)).unwrap();
    let mut app = App::new(source, map);
    assert_eq!(app.depth(), 1, "a new map starts at the top");
    let fx = elevation::restore(&mut app, &place);
    assert!(!fx.is_empty());
    assert_eq!(
        names_of(&app),
        vec!["/data".to_string(), "beta".into(), "inner".into()],
        "the same folder is on screen"
    );
    let sel = app.selected().expect("the same file is selected");
    let v = app.current_view();
    assert_eq!(app.source().target(v, sel).unwrap().name, before_name);
}

#[test]
fn a_place_that_is_gone_falls_back_to_the_nearest_folder_that_exists() {
    let (_, walk) = build(&["alpha", "beta"], None);
    let mut shell = shell_for(walk, 1.0);
    enter_folder(&mut shell, "beta");
    enter_folder(&mut shell, "inner");
    let place = elevation::capture(shell.app().unwrap());
    // In the new tree beta has no "inner".
    let mut agg = Aggregator::new();
    let mut b = Batch::new();
    b.dir(DirKey(0), DirKey::NONE, b"/data", &meta(4096))
        .unwrap();
    b.dir(DirKey(1), DirKey(0), b"beta", &meta(4096)).unwrap();
    b.file(NodeKind::File, DirKey(1), b"f", &meta(5_000_000))
        .unwrap();
    b.listing_done(DirKey(1));
    b.listing_done(DirKey(0));
    agg.apply(&b);
    let map = shell.map_rect();
    let source = TreeSource::new(agg.publish(), Metrics::from_logical(13.0, 1.0)).unwrap();
    let mut app = App::new(source, map);
    elevation::restore(&mut app, &place);
    assert_eq!(names_of(&app), vec!["/data".to_string(), "beta".into()]);
    // And with nothing in common it stays at the top without failing.
    let mut agg = Aggregator::new();
    let mut b = Batch::new();
    b.dir(DirKey(0), DirKey::NONE, b"/data", &meta(4096))
        .unwrap();
    b.listing_done(DirKey(0));
    agg.apply(&b);
    let source = TreeSource::new(agg.publish(), Metrics::from_logical(13.0, 1.0)).unwrap();
    let mut app = App::new(source, map);
    let _ = elevation::restore(&mut app, &place);
    assert_eq!(names_of(&app), vec!["/data".to_string()]);
    assert_eq!(Place::default().depth(), 0);
}

#[test]
fn hostile_names_are_kept_by_their_bytes() {
    let weird: Vec<u8> = vec![b'x', 0xff, 0xfe, b'/', b'y'];
    let mut agg = Aggregator::new();
    let mut b = Batch::new();
    b.dir(DirKey(0), DirKey::NONE, b"/data", &meta(4096))
        .unwrap();
    b.dir(DirKey(1), DirKey(0), &weird, &meta(4096)).unwrap();
    b.file(NodeKind::File, DirKey(1), b"f", &meta(5_000_000))
        .unwrap();
    b.listing_done(DirKey(1));
    b.listing_done(DirKey(0));
    agg.apply(&b);
    let snap = agg.publish();
    let mut shell = shell_for(snap.clone(), 1.0);
    let id = shell
        .app()
        .unwrap()
        .items()
        .iter()
        .find(|i| i.kind == ItemKind::FolderTitled && i.depth == 1)
        .map(|i| i.id)
        .unwrap();
    shell.app_mut().unwrap().open(id);
    let place = elevation::capture(shell.app().unwrap());
    assert_eq!(place.depth(), 1);
    let map = shell.map_rect();
    let source = TreeSource::new(snap, Metrics::from_logical(13.0, 1.0)).unwrap();
    let mut app = App::new(source, map);
    elevation::restore(&mut app, &place);
    assert_eq!(app.depth(), 2);
}

// ---- elevated deletes -------------------------------------------------------

#[derive(Clone)]
struct Script {
    end: Arc<Mutex<ElevatedEnd>>,
    protected: Arc<Mutex<Option<&'static str>>>,
}

struct Fake {
    calls: Mutex<Vec<String>>,
    confirmed: Mutex<Vec<bool>>,
    held: Mutex<Vec<Vec<std::path::PathBuf>>>,
    seen: Mutex<Vec<Vec<Option<bs_platform::delete::Identity>>>>,
    script: Script,
    denied: Mutex<bool>,
    /// An answer for the check of a permanent delete: the item, its level and
    /// why it is refused.
    refuse_permanent: Mutex<Option<(usize, Level, RefuseReason)>>,
}

impl Fake {
    fn new(end: ElevatedEnd) -> Arc<Fake> {
        Arc::new(Fake {
            calls: Mutex::new(Vec::new()),
            confirmed: Mutex::new(Vec::new()),
            held: Mutex::new(Vec::new()),
            seen: Mutex::new(Vec::new()),
            script: Script {
                end: Arc::new(Mutex::new(end)),
                protected: Arc::new(Mutex::new(None)),
            },
            denied: Mutex::new(true),
            refuse_permanent: Mutex::new(None),
        })
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    fn name(p: &Path) -> String {
        p.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    }
}

impl Disposer for Fake {
    fn precheck(
        &self,
        items: &[DItem],
        permanent: bool,
        confirmed: Option<&ProtectedConfirmation>,
    ) -> Plan {
        let note = *self.script.protected.lock().unwrap();
        let mut entries: Vec<PlanEntry> = items
            .iter()
            .map(|_| match note {
                None => PlanEntry {
                    level: Level::Normal,
                    reason: Reason::None,
                    note: None,
                    decision: Decision::Proceed,
                },
                Some(n) => PlanEntry {
                    level: Level::Protected,
                    reason: Reason::Table,
                    note: Some(n),
                    decision: if confirmed.is_some() {
                        Decision::Proceed
                    } else {
                        Decision::Refused(RefuseReason::ProtectedNotConfirmed { note: Some(n) })
                    },
                },
            })
            .collect();
        if let Some((at, level, why)) = self
            .refuse_permanent
            .lock()
            .unwrap()
            .clone()
            .filter(|_| permanent)
        {
            for (i, e) in entries.iter_mut().enumerate() {
                if i == at {
                    e.level = level;
                    e.decision = Decision::Refused(why.clone());
                } else {
                    e.decision = Decision::Refused(RefuseReason::SelectionRefused { driver: at });
                }
            }
        }
        let strictest = entries
            .iter()
            .map(|e| e.level)
            .max()
            .unwrap_or(Level::Normal);
        let drivers = entries
            .iter()
            .enumerate()
            .filter(|(_, e)| e.level == strictest)
            .map(|(i, _)| i)
            .collect();
        Plan {
            selection_refused: false,
            entries,
            strictest,
            drivers,
        }
    }

    fn trash(&self, items: &[DItem], _c: Option<&ProtectedConfirmation>) -> Vec<ActOutcome> {
        self.calls.lock().unwrap().push(format!(
            "trash:{}",
            items
                .iter()
                .map(|i| Self::name(&i.path))
                .collect::<Vec<_>>()
                .join(",")
        ));
        let denied = *self.denied.lock().unwrap();
        items
            .iter()
            .map(|i| {
                if denied {
                    ActOutcome::Failed(OpError {
                        step: "move",
                        kind: std::io::ErrorKind::PermissionDenied,
                        code: Some(13),
                        removed_bytes: 0,
                        removed_entries: 0,
                    })
                } else {
                    ActOutcome::MovedToBin {
                        token: Token::new(i.path.clone()),
                        bytes: 1,
                        basis: bs_platform::delete::ByteBasis::Allocated,
                        shared_files: 0,
                    }
                }
            })
            .collect()
    }

    fn delete_permanently(
        &self,
        items: &[DItem],
        c: Option<&ProtectedConfirmation>,
    ) -> Vec<ActOutcome> {
        self.calls.lock().unwrap().push(format!(
            "perm:{}",
            items
                .iter()
                .map(|i| Self::name(&i.path))
                .collect::<Vec<_>>()
                .join(",")
        ));
        self.trash(items, c)
    }

    fn restore(&self, _t: &Token) -> RestoreOutcome {
        RestoreOutcome::Refused {
            reason: RefuseReason::NotInBin,
        }
    }

    fn empty_bin(&self, _m: &Path) -> EmptyOutcome {
        EmptyOutcome::Refused {
            reason: RefuseReason::NoBinOnDrive,
        }
    }

    fn delete_elevated(&self, items: &[DItem], held: &[std::path::PathBuf]) -> ElevatedEnd {
        self.held.lock().unwrap().push(held.to_vec());
        self.seen
            .lock()
            .unwrap()
            .push(items.iter().map(|i| i.seen).collect());
        self.calls.lock().unwrap().push(format!(
            "admin:{}",
            items
                .iter()
                .map(|i| Self::name(&i.path))
                .collect::<Vec<_>>()
                .join(",")
        ));
        self.confirmed.lock().unwrap().push(!held.is_empty());
        self.script.end.lock().unwrap().clone()
    }
}

struct Rig {
    shell: Shell<TreeSource>,
    fake: Arc<Fake>,
}

fn rig(style: BinStyle, end: ElevatedEnd) -> Rig {
    let (_, snap) = build(&["alpha", "beta", "gamma"], None);
    let mut shell = shell_for(snap, 1.0);
    let fake = Fake::new(end);
    shell.set_disposer(fake.clone());
    shell.set_bin_style(style);
    Rig { shell, fake }
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

fn settle(shell: &mut Shell<TreeSource>, fx: Vec<ShellEffect>, at: u64) -> Vec<ShellEffect> {
    let mut out = Vec::new();
    let mut queue = fx;
    while !queue.is_empty() {
        let mut next = Vec::new();
        for e in queue {
            match e {
                ShellEffect::Run(job) if job.is_check() => {
                    next.extend(shell.job_done(job.run(), t(at)));
                }
                other => out.push(other),
            }
        }
        queue = next;
    }
    out
}

fn key(shell: &mut Shell<TreeSource>, k: Key, at: u64) -> Vec<ShellEffect> {
    let fx = shell.update(InputEvent::Key(k), t(at));
    settle(shell, fx, at)
}

fn run_job(shell: &mut Shell<TreeSource>, fx: Vec<ShellEffect>, at: u64) -> Vec<ShellEffect> {
    let job = fx
        .into_iter()
        .find_map(|e| match e {
            ShellEffect::Run(job) => Some(job),
            _ => None,
        })
        .expect("a disk job was started");
    shell.job_done(job.run(), t(at))
}

fn has_job(fx: &[ShellEffect]) -> bool {
    fx.iter().any(|e| matches!(e, ShellEffect::Run(_)))
}

fn tree_changes(fx: &[ShellEffect]) -> Vec<TreeChange> {
    fx.iter()
        .filter_map(|e| match e {
            ShellEffect::Tree(c) => Some(c.clone()),
            _ => None,
        })
        .flatten()
        .collect()
}

fn select_first(shell: &mut Shell<TreeSource>) {
    let all = folders(shell);
    let (x, y) = (all[0].rect.x + all[0].rect.w / 2.0, all[0].rect.y + 3.0);
    shell.update(InputEvent::Click { x, y }, t(10));
}

fn button_center(shell: &Shell<TreeSource>, label: &str) -> (f32, f32) {
    let popup = shell.popup().expect("a popup");
    for part in &popup.card.parts {
        match part {
            bs_render::CardPart::Button { rect, label: l, .. }
            | bs_render::CardPart::Hold { rect, label: l, .. }
                if &**l == label =>
            {
                return center(*rect);
            }
            _ => {}
        }
    }
    panic!("no control called {label}");
}

/// Delete, let the platform refuse for want of rights, and answer the check
/// that follows: the dialog is then open.
fn denied_delete(r: &mut Rig) {
    select_first(&mut r.shell);
    let fx = key(&mut r.shell, Key::Delete, 20);
    let fx = run_job(&mut r.shell, fx, 30);
    // The refused move offers the helper: a check, then the dialog.
    assert!(has_job(&fx), "no offer of the helper: {fx:?}");
    settle(&mut r.shell, fx, 40);
}

#[test]
fn a_delete_the_users_rights_cannot_do_opens_the_administrator_dialog_with_the_items_listed() {
    for (style, line) in [
        (
            BinStyle::Trash,
            "Will be permanently deleted (system Trash isn't available for administrator deletes).",
        ),
        (
            BinStyle::Recycle,
            "Will be permanently deleted. This can't be undone.",
        ),
    ] {
        let mut r = rig(style, ElevatedEnd::Done(Vec::new()));
        denied_delete(&mut r);
        let text = r.shell.dialog_text().expect("a dialog");
        assert!(
            text.starts_with("These need administrator rights. Delete 1 item ("),
            "{text}"
        );
        assert!(text.contains("as administrator?"), "{text}");
        assert!(text.contains(line), "{text}");
        assert!(text.contains("• alpha"), "{text}");
        assert!(
            text.contains("asks for administrator rights once"),
            "{text}"
        );
        assert_eq!(
            r.shell.dialog_buttons(),
            ["Delete as administrator", "Cancel"]
        );
        // Nothing is done before the answer.
        assert!(!r.fake.calls().iter().any(|c| c.starts_with("admin")));
        // The Linux wording never appears on Windows and the other way round.
        assert_eq!(text.contains("system Trash"), style == BinStyle::Trash);
    }
}

#[test]
fn confirming_runs_one_helper_call_and_reports_each_item_honestly() {
    let mut r = rig(BinStyle::Trash, ElevatedEnd::Done(Vec::new()));
    // Three selected: one deleted, one refused, one failed part way.
    let all = folders(&r.shell);
    for (n, f) in all.iter().enumerate() {
        let ev = InputEvent::ClickWith {
            x: f.rect.x + f.rect.w / 2.0,
            y: f.rect.y + 3.0,
            mods: bs_ui::Modifiers {
                ctrl: n > 0,
                shift: false,
            },
        };
        r.shell.update(ev, t(10 + n as u64));
    }
    *r.fake.script.end.lock().unwrap() = ElevatedEnd::Done(vec![
        ElevatedItem::Deleted {
            bytes: 9_000_000_000,
            shared_files: 0,
        },
        ElevatedItem::Refused("part of the operating system".into()),
        ElevatedItem::Failed(elevation::failed_words(5, 12)),
    ]);
    let fx = key(&mut r.shell, Key::Delete, 20);
    let fx = run_job(&mut r.shell, fx, 30);
    settle(&mut r.shell, fx, 40);
    assert!(r.shell.dialog_text().unwrap().contains("Delete 3 items ("));
    let p = button_center(&r.shell, "Delete as administrator");
    let fx = r.shell.update(InputEvent::Click { x: p.0, y: p.1 }, t(50));
    let fx = run_job(&mut r.shell, fx, 60);
    assert_eq!(
        r.fake
            .calls()
            .iter()
            .filter(|c| c.starts_with("admin:"))
            .count(),
        1,
        "one prompt for the whole action"
    );
    let toast = r.shell.toast_text().unwrap();
    assert!(toast.starts_with("Deleted · 9.0 GB"), "{toast}");
    assert!(
        toast.contains("wasn't touched: part of the operating system"),
        "{toast}"
    );
    assert!(toast.contains("12 entries were already removed"), "{toast}");
    assert!(
        !toast.contains("Undo"),
        "no undo for a permanent delete: {toast}"
    );
    let changes = tree_changes(&fx);
    assert_eq!(changes.len(), 1, "only the deleted item is marked");
    assert_eq!(changes[0].to, DeleteState::Deleted);
    assert!(!toast.to_lowercase().contains("freed"));
    assert!(
        !toast.contains("Moved to"),
        "a helper delete is never a move: {toast}"
    );
}

#[test]
fn a_protected_item_needs_the_hold_and_the_confirmation_reaches_the_helper() {
    let mut r = rig(
        BinStyle::Trash,
        ElevatedEnd::Done(vec![ElevatedItem::Deleted {
            bytes: 1,
            shared_files: 0,
        }]),
    );
    *r.fake.script.protected.lock().unwrap() = Some("Kept by the system for its own use.");
    // A move to the Trash of a protected item asks for the hold first; the
    // platform then refuses the move for want of rights, which leads to the
    // administrator dialog.
    *r.fake.denied.lock().unwrap() = true;
    select_first(&mut r.shell);
    key(&mut r.shell, Key::Delete, 40);
    let hold = "Hold to move to the Trash";
    let p = button_center(&r.shell, hold);
    r.shell
        .update(InputEvent::PointerDown { x: p.0, y: p.1 }, t(1_000));
    r.shell.tick(t(2_600));
    let fx = r.shell.take_effects();
    let fx = run_job(&mut r.shell, fx, 2_700);
    settle(&mut r.shell, fx, 2_800);
    let text = r.shell.dialog_text().expect("the administrator dialog");
    assert!(
        text.contains("Kept by the system for its own use."),
        "{text}"
    );
    assert!(text.contains("Hold the button for 1.5 seconds"), "{text}");
    assert_eq!(
        r.shell.dialog_buttons(),
        ["Hold to delete as administrator", "Cancel"]
    );
    // A click does not confirm.
    let p = button_center(&r.shell, "Hold to delete as administrator");
    let fx = r
        .shell
        .update(InputEvent::Click { x: p.0, y: p.1 }, t(3_000));
    assert!(!has_job(&fx) && r.shell.modal_open());
    // Holding does.
    r.shell
        .update(InputEvent::PointerDown { x: p.0, y: p.1 }, t(4_000));
    r.shell.tick(t(5_600));
    let fx = r.shell.take_effects();
    assert!(has_job(&fx));
    run_job(&mut r.shell, fx, 5_700);
    assert_eq!(*r.fake.confirmed.lock().unwrap(), vec![true]);
    let held = r.fake.held.lock().unwrap().clone();
    assert_eq!(held.len(), 1);
    assert!(
        held[0].iter().all(|p| p.ends_with("alpha")),
        "only the path whose hold was completed is confirmed: {held:?}"
    );
}

#[test]
fn nothing_is_confirmed_for_a_plain_item_and_what_was_seen_at_selection_goes_along() {
    // A real folder, so the identity noted when it was selected is real.
    let base = std::env::var_os("TMPDIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let dir = base.join(format!("bs-ui-admin-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("alpha")).unwrap();
    let root = dir.to_string_lossy().into_owned().into_bytes();
    let mut agg = Aggregator::new();
    let mut b = Batch::new();
    b.dir(DirKey(0), DirKey::NONE, &root, &meta(4096)).unwrap();
    b.dir(DirKey(1), DirKey(0), b"alpha", &meta(4096)).unwrap();
    b.file(NodeKind::File, DirKey(1), b"f", &meta(5_000_000))
        .unwrap();
    b.listing_done(DirKey(1));
    b.listing_done(DirKey(0));
    agg.apply(&b);
    let mut shell = shell_for(agg.publish(), 1.0);
    let fake = Fake::new(ElevatedEnd::Declined);
    shell.set_disposer(fake.clone());
    shell.set_bin_style(BinStyle::Trash);
    let mut r = Rig { shell, fake };
    denied_delete(&mut r);
    let p = button_center(&r.shell, "Delete as administrator");
    let fx = r.shell.update(InputEvent::Click { x: p.0, y: p.1 }, t(50));
    run_job(&mut r.shell, fx, 60);
    let expected = bs_platform::delete::identity_of(&dir.join("alpha")).unwrap();
    let seen = r.fake.seen.lock().unwrap().clone();
    let held = r.fake.held.lock().unwrap().clone();
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(seen, vec![vec![Some(expected)]]);
    assert_eq!(
        held,
        vec![Vec::<std::path::PathBuf>::new()],
        "no hold, no confirmation"
    );
}

#[test]
fn declining_the_prompt_is_a_plain_message_and_nothing_is_marked() {
    let mut r = rig(BinStyle::Trash, ElevatedEnd::Declined);
    denied_delete(&mut r);
    let p = button_center(&r.shell, "Delete as administrator");
    let fx = r.shell.update(InputEvent::Click { x: p.0, y: p.1 }, t(50));
    let fx = run_job(&mut r.shell, fx, 60);
    assert_eq!(
        r.shell.toast_text().as_deref(),
        Some("Administrator rights weren't granted, so nothing was deleted.")
    );
    assert!(tree_changes(&fx).is_empty());
}

#[test]
fn no_account_entry_is_said_in_words_and_nothing_is_deleted() {
    let mut r = rig(BinStyle::Trash, ElevatedEnd::NoAccount);
    denied_delete(&mut r);
    let p = button_center(&r.shell, "Delete as administrator");
    let fx = r.shell.update(InputEvent::Click { x: p.0, y: p.1 }, t(50));
    let fx = run_job(&mut r.shell, fx, 60);
    let toast = r.shell.toast_text().unwrap();
    assert!(toast.contains("your account has no entry"), "{toast}");
    assert!(toast.contains("Nothing was deleted."), "{toast}");
    assert!(tree_changes(&fx).is_empty());
}

#[test]
fn a_helper_that_stops_part_way_reports_what_is_known_and_what_is_not() {
    let mut r = rig(BinStyle::Trash, ElevatedEnd::Done(Vec::new()));
    let all = folders(&r.shell);
    for (n, f) in all.iter().take(2).enumerate() {
        r.shell.update(
            InputEvent::ClickWith {
                x: f.rect.x + f.rect.w / 2.0,
                y: f.rect.y + 3.0,
                mods: bs_ui::Modifiers {
                    ctrl: n > 0,
                    shift: false,
                },
            },
            t(10 + n as u64),
        );
    }
    *r.fake.script.end.lock().unwrap() = ElevatedEnd::Failed {
        message: "the helper closed the connection".into(),
        completed: vec![(
            0,
            ElevatedItem::Deleted {
                bytes: 4_000_000_000,
                shared_files: 0,
            },
        )],
    };
    let fx = key(&mut r.shell, Key::Delete, 20);
    let fx = run_job(&mut r.shell, fx, 30);
    settle(&mut r.shell, fx, 40);
    let p = button_center(&r.shell, "Delete as administrator");
    let fx = r.shell.update(InputEvent::Click { x: p.0, y: p.1 }, t(50));
    let fx = run_job(&mut r.shell, fx, 60);
    let toast = r.shell.toast_text().unwrap();
    assert!(toast.starts_with("Deleted · 4.0 GB"), "{toast}");
    assert!(
        toast.contains("1 item may or may not be deleted; rescan the folder"),
        "{toast}"
    );
    assert_eq!(
        tree_changes(&fx).len(),
        1,
        "only the item with an outcome is marked"
    );
}

#[test]
fn without_the_helper_the_refusal_stays_a_plain_message_with_its_reason() {
    let mut r = rig(BinStyle::Trash, ElevatedEnd::Done(Vec::new()));
    r.shell.set_offer(Offer::Hidden);
    select_first(&mut r.shell);
    let fx = key(&mut r.shell, Key::Delete, 20);
    let fx = run_job(&mut r.shell, fx, 30);
    assert!(!has_job(&fx), "the helper is not offered");
    assert!(!r.shell.modal_open());
    let toast = r.shell.toast_text().unwrap();
    assert!(
        toast.contains("permission denied; it needs administrator rights"),
        "{toast}"
    );
}

#[test]
fn cancelling_the_administrator_dialog_does_nothing() {
    let mut r = rig(BinStyle::Trash, ElevatedEnd::Done(Vec::new()));
    denied_delete(&mut r);
    key(&mut r.shell, Key::Escape, 50);
    assert!(!r.shell.modal_open());
    assert!(!r.fake.calls().iter().any(|c| c.starts_with("admin")));
}

#[test]
fn the_unavailable_helper_is_explained() {
    let mut r = rig(
        BinStyle::Trash,
        ElevatedEnd::Unavailable("The helper that reads the whole drive is not installed.".into()),
    );
    denied_delete(&mut r);
    let p = button_center(&r.shell, "Delete as administrator");
    let fx = r.shell.update(InputEvent::Click { x: p.0, y: p.1 }, t(50));
    run_job(&mut r.shell, fx, 60);
    let toast = r.shell.toast_text().unwrap();
    assert!(toast.contains("not installed"), "{toast}");
    assert!(toast.contains("Nothing was deleted."), "{toast}");
}

#[test]
fn the_default_disposer_cannot_elevate() {
    struct Plain;
    impl Disposer for Plain {
        fn precheck(&self, _: &[DItem], _: bool, _: Option<&ProtectedConfirmation>) -> Plan {
            unreachable!()
        }
        fn trash(&self, _: &[DItem], _: Option<&ProtectedConfirmation>) -> Vec<ActOutcome> {
            Vec::new()
        }
        fn delete_permanently(
            &self,
            _: &[DItem],
            _: Option<&ProtectedConfirmation>,
        ) -> Vec<ActOutcome> {
            Vec::new()
        }
        fn restore(&self, _: &Token) -> RestoreOutcome {
            RestoreOutcome::Refused {
                reason: RefuseReason::NotInBin,
            }
        }
        fn empty_bin(&self, _: &Path) -> EmptyOutcome {
            EmptyOutcome::Refused {
                reason: RefuseReason::NoBinOnDrive,
            }
        }
    }
    let end = Plain.delete_elevated(&[], &[]);
    assert!(matches!(end, ElevatedEnd::Unavailable(_)));
}

#[test]
fn the_helper_s_outcomes_and_errors_become_the_interface_s_words() {
    use bs_ipc::{ClientError, ErrorCode, ErrorReport, ItemOutcome, Refusal};
    use bs_platform::elevate::{ElevateError, Unavailable};
    let item = elevation::item_from(&ItemOutcome::Deleted {
        bytes: 5,
        entries: 2,
        shared_files: 1,
    });
    assert_eq!(
        item,
        ElevatedItem::Deleted {
            bytes: 5,
            shared_files: 1
        }
    );
    assert!(matches!(
        elevation::item_from(&ItemOutcome::Refused(Refusal::Blocked)),
        ElevatedItem::Refused(t) if t == "part of the operating system"
    ));
    assert!(matches!(
        elevation::item_from(&ItemOutcome::Failed { step: 2, os_code: 13, removed_bytes: 0, removed_entries: 3 }),
        ElevatedItem::Failed(t) if t.contains("code 13") && t.contains("3 entries")
    ));
    assert_eq!(
        elevation::end_from_error(ElevateError::Declined, vec![]),
        ElevatedEnd::Declined
    );
    assert!(matches!(
        elevation::end_from_error(ElevateError::Unavailable(Unavailable::AppImage), vec![]),
        ElevatedEnd::Unavailable(t) if t.contains(".deb")
    ));
    let no_account = ElevateError::Protocol(ClientError::Helper(ErrorReport {
        code: ErrorCode::NoAccount,
        message: "x".into(),
    }));
    assert_eq!(
        elevation::end_from_error(no_account, vec![]),
        ElevatedEnd::NoAccount
    );
    let broke = ElevateError::Protocol(ClientError::Disconnected);
    let ElevatedEnd::Failed { message, completed } = elevation::end_from_error(
        broke,
        vec![(
            0,
            ItemOutcome::Deleted {
                bytes: 1,
                entries: 1,
                shared_files: 0,
            },
        )],
    ) else {
        panic!("expected a failure");
    };
    assert!(message.contains("closed the connection"));
    assert_eq!(completed.len(), 1);
}

#[test]
fn every_new_word_avoids_the_banned_ones() {
    let mut texts: Vec<String> = vec![
        elevation::EDITION_TEXT.into(),
        elevation::NO_ACCOUNT_TEXT.into(),
        elevation::DECLINED_TEXT.into(),
    ];
    for f in [
        bs_platform::elevate::Fallback::MountNotPlaced,
        bs_platform::elevate::Fallback::TooLarge,
        bs_platform::elevate::Fallback::Unavailable,
    ] {
        texts.extend(elevation::fallback_note(f));
    }
    for u in [
        bs_platform::elevate::Unavailable::AppImage,
        bs_platform::elevate::Unavailable::SnapConfined,
    ] {
        texts.push(u.explanation().into());
    }
    let mut r = rig(BinStyle::Trash, ElevatedEnd::Done(Vec::new()));
    denied_delete(&mut r);
    texts.extend(r.shell.dialog_text());
    for text in texts {
        let lower = text.to_lowercase();
        for word in ["safe", "junk", "reclaim", "clean up", "freed"] {
            assert!(!lower.contains(word), "{text:?} contains {word}");
        }
    }
}

#[test]
fn the_dialog_fits_a_small_window_at_both_scales() {
    for scale in [1.0f32, 2.0] {
        let (_, snap) = build(&["alpha", "beta", "gamma"], None);
        let mut shell = shell_for(snap, scale);
        let fake = Fake::new(ElevatedEnd::Declined);
        shell.set_disposer(fake);
        shell.set_bin_style(BinStyle::Trash);
        let all = folders(&shell);
        shell.update(
            InputEvent::Click {
                x: all[0].rect.x + all[0].rect.w / 2.0,
                y: all[0].rect.y + 3.0 * scale,
            },
            t(5),
        );
        let fx = key(&mut shell, Key::Delete, 20);
        let fx = run_job(&mut shell, fx, 30);
        settle(&mut shell, fx, 40);
        let rect = shell.popup().unwrap().card.rect;
        assert!(rect.right() <= 1200.0 * scale && rect.bottom() <= 800.0 * scale);
    }
}

// ---- the matrix for the administrator flow ------------------------------------

/// Selects every folder on screen.
fn select_all_folders(shell: &mut Shell<TreeSource>) -> usize {
    let all = folders(shell);
    for (n, f) in all.iter().enumerate() {
        shell.update(
            InputEvent::ClickWith {
                x: f.rect.x + f.rect.w / 2.0,
                y: f.rect.y + 3.0,
                mods: bs_ui::Modifiers {
                    ctrl: n > 0,
                    shift: false,
                },
            },
            t(10 + n as u64),
        );
    }
    all.len()
}

/// Deletes the selection, lets the platform refuse the move for want of
/// rights, and answers the check that follows.
fn denied_selection(r: &mut Rig) -> Vec<ShellEffect> {
    let fx = key(&mut r.shell, Key::Delete, 20);
    let fx = run_job(&mut r.shell, fx, 30);
    assert!(has_job(&fx), "no offer of the helper: {fx:?}");
    settle(&mut r.shell, fx, 40)
}

fn no_admin_call(r: &Rig) -> bool {
    !r.fake.calls().iter().any(|c| c.starts_with("admin"))
}

#[test]
fn a_blocked_item_in_a_selection_stops_the_administrator_flow() {
    for blocked_at in 0..3 {
        let mut r = rig(BinStyle::Trash, ElevatedEnd::Done(Vec::new()));
        assert_eq!(select_all_folders(&mut r.shell), 3);
        *r.fake.refuse_permanent.lock().unwrap() = Some((
            blocked_at,
            Level::Blocked,
            RefuseReason::Blocked {
                reason: Reason::Table,
                note: None,
            },
        ));
        denied_selection(&mut r);
        assert!(
            r.shell.dialog_text().is_none(),
            "no administrator dialog for a selection with a blocked item"
        );
        assert!(!r.shell.modal_open());
        assert_eq!(
            info(&r.shell),
            "Delete permanently: Part of the operating system"
        );
        assert!(no_admin_call(&r), "{:?}", r.fake.calls());
    }
}

#[test]
fn an_item_the_helper_would_refuse_for_another_reason_stops_it_too() {
    let mut r = rig(BinStyle::Trash, ElevatedEnd::Done(Vec::new()));
    select_all_folders(&mut r.shell);
    *r.fake.refuse_permanent.lock().unwrap() =
        Some((1, Level::Normal, RefuseReason::ChangedSinceScan));
    denied_selection(&mut r);
    assert!(r.shell.dialog_text().is_none());
    assert!(!r.shell.modal_open());
    assert_eq!(
        info(&r.shell),
        "Delete permanently: it changed since the scan; rescan the folder first"
    );
    assert!(no_admin_call(&r), "{:?}", r.fake.calls());
}

// ---- folder maps ----------------------------------------------------------------

#[test]
fn a_folder_map_has_no_button_but_still_offers_the_administrator_delete() {
    let mut r = rig(BinStyle::Trash, ElevatedEnd::Done(Vec::new()));
    r.shell.set_offer(Offer::DeletesOnly);
    assert!(
        full_scan_rect(&mut r.shell).is_none(),
        "no Full scan button"
    );
    denied_delete(&mut r);
    let text = r.shell.dialog_text().expect("the administrator dialog");
    assert!(text.contains("as administrator?"), "{text}");
    // Pressing a button for it is not possible, and the block is worded
    // plainly (a folder map has none, but the wording must not invite).
    assert_eq!(r.shell.unreadable_as(), UnreadableAs::Plain);
    let p = button_center(&r.shell, "Delete as administrator");
    let fx = r.shell.update(InputEvent::Click { x: p.0, y: p.1 }, t(50));
    run_job(&mut r.shell, fx, 60);
    assert_eq!(
        r.fake.calls().last().map(String::as_str),
        Some("admin:alpha")
    );
}

#[test]
fn only_a_helper_that_cannot_be_started_keeps_the_denied_message() {
    let mut r = rig(BinStyle::Trash, ElevatedEnd::Done(Vec::new()));
    r.shell.set_offer(Offer::Hidden);
    select_first(&mut r.shell);
    let fx = key(&mut r.shell, Key::Delete, 20);
    let fx = run_job(&mut r.shell, fx, 30);
    settle(&mut r.shell, fx, 40);
    assert!(r.shell.dialog_text().is_none());
}

#[test]
fn only_an_available_offer_invites_and_the_rest_name_the_gap_plainly() {
    let (_, snap) = build(&["a", "b"], None);
    let mut shell = shell_for(snap, 1.0);
    shell.set_offer(Offer::Hidden);
    let ua = shell.unreadable_as();
    show_blocks(&mut shell, blocks(ua));
    let label = unreadable_box(&shell)
        .label
        .as_ref()
        .unwrap()
        .name
        .to_string();
    assert_eq!(label, "Couldn't read: 23.4 GB");
    assert!(
        shell.set_offer(Offer::Available),
        "reworded when it can be granted"
    );
    assert_eq!(shell.unreadable_as(), UnreadableAs::Grant);
}

// ---- every item is listed ---------------------------------------------------------

fn many(count: usize) -> Rig {
    let names: Vec<String> = (0..count).map(|i| format!("item-{i:02}")).collect();
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let (_, snap) = build(&refs, None);
    let mut shell = shell_for(snap, 1.0);
    let fake = Fake::new(ElevatedEnd::Done(Vec::new()));
    shell.set_disposer(fake.clone());
    shell.set_bin_style(BinStyle::Trash);
    Rig { shell, fake }
}

/// The names on screen in the dialog now, in order.
fn listed_names(shell: &Shell<TreeSource>) -> Vec<String> {
    shell
        .popup()
        .expect("a dialog")
        .card
        .parts
        .iter()
        .filter_map(|p| match p {
            bs_render::CardPart::Line { text, .. } => text.strip_prefix("• ").map(str::to_string),
            _ => None,
        })
        .collect()
}

/// Selects up to `count` folders on screen and returns how many there were.
fn select_ids(shell: &mut Shell<TreeSource>, count: usize) -> usize {
    let ids: Vec<u32> = shell
        .app()
        .unwrap()
        .items()
        .iter()
        .filter(|i| i.depth == 1 && i.kind == ItemKind::FolderTitled)
        .map(|i| i.id)
        .take(count)
        .collect();
    let n = ids.len();
    shell.app_mut().unwrap().select(ids);
    n
}

#[test]
fn the_dialog_lists_every_item_scrolling_when_they_do_not_all_fit() {
    let mut r = many(30);
    let n = select_ids(&mut r.shell, 30);
    assert!(n >= 16, "enough folders on screen: {n}");
    denied_selection(&mut r);
    let text = r.shell.dialog_text().expect("the administrator dialog");
    assert!(text.contains(&format!("Delete {n} items (")), "{text}");
    // Read whole by a screen reader, whatever is on screen.
    assert_eq!(text.matches("\u{2022} item-").count(), n, "{text}");
    let first_page = listed_names(&r.shell);
    assert!(
        first_page.len() >= 3 && first_page.len() < n,
        "{first_page:?}"
    );
    let mut seen = std::collections::BTreeSet::new();
    seen.extend(first_page.iter().cloned());
    // Scroll with the wheel to the end; every name shows at some point.
    for k in 0..60 {
        r.shell.update(
            InputEvent::Wheel {
                x: 10.0,
                y: 10.0,
                steps: -1.0,
            },
            t(100 + k),
        );
        seen.extend(listed_names(&r.shell));
    }
    assert_eq!(seen.len(), n, "every item is reachable: {seen:?}");
    // At the end the last name is on screen, and the count line says where.
    let end = listed_names(&r.shell);
    let last = text.rsplit("\u{2022} ").next().unwrap().trim().to_string();
    assert_eq!(end.last(), Some(&last), "{end:?}");
    let parts = &r.shell.popup().unwrap().card.parts;
    let tail = format!("\u{2013}{n} of {n} ");
    assert!(parts.iter().any(|p| matches!(p,
        bs_render::CardPart::Line { text, .. } if text.contains(&tail))));
    // The arrow keys and Home move too.
    r.shell.update(InputEvent::Key(Key::Home), t(300));
    let top = listed_names(&r.shell);
    assert_eq!(top, first_page);
    r.shell
        .update(InputEvent::Key(Key::Arrow(bs_ui::Direction::Down)), t(310));
    assert_eq!(listed_names(&r.shell)[0], first_page[1]);
    // The buttons are still there and the helper gets all of them.
    let p = button_center(&r.shell, "Delete as administrator");
    let fx = r.shell.update(InputEvent::Click { x: p.0, y: p.1 }, t(400));
    run_job(&mut r.shell, fx, 410);
    let calls = r.fake.calls();
    let admin = calls
        .iter()
        .find(|c| c.starts_with("admin:"))
        .expect("a call");
    assert_eq!(admin.matches("item-").count(), n, "{admin}");
}

#[test]
fn a_few_items_need_no_scrolling_and_all_show_at_once() {
    let mut r = many(6);
    assert_eq!(select_ids(&mut r.shell, 6), 6);
    denied_selection(&mut r);
    let names = listed_names(&r.shell);
    assert_eq!(names.len(), 6, "{names:?}");
    let text = r.shell.dialog_text().unwrap();
    assert!(!text.contains("to scroll"), "{text}");
}

// ---- how the outcome is worded -----------------------------------------------

/// Three folders: the second holds a file whose size is an estimate, the
/// third could not be read completely.
fn marked_rig(end: ElevatedEnd) -> Rig {
    let mut agg = Aggregator::new();
    let mut b = Batch::new();
    b.dir(DirKey(0), DirKey::NONE, b"/data", &meta(4096))
        .unwrap();
    for i in 1..=3u32 {
        let key = DirKey(i);
        b.dir(key, DirKey(0), format!("folder{i}").as_bytes(), &meta(4096))
            .unwrap();
        let mut m = meta(1_000_000 * u64::from(4 - i));
        if i == 2 {
            m.flags = bs_tree::NodeFlags::ESTIMATED_SIZE;
        }
        b.file(NodeKind::File, key, b"file", &m).unwrap();
        if i == 3 {
            b.unreadable(
                key,
                bs_tree::ReadFailure {
                    kind: bs_tree::ReadErrorKind::PermissionDenied,
                    code: 13,
                },
            );
        } else {
            b.listing_done(key);
        }
    }
    b.listing_done(DirKey(0));
    agg.apply(&b);
    let mut shell = shell_for(agg.publish(), 1.0);
    let fake = Fake::new(end);
    shell.set_disposer(fake.clone());
    shell.set_bin_style(BinStyle::Trash);
    Rig { shell, fake }
}

fn click_folder(shell: &mut Shell<TreeSource>, name: &str) {
    let item = shell
        .app()
        .unwrap()
        .items()
        .iter()
        .find(|i| {
            i.kind == ItemKind::FolderTitled
                && i.label.as_ref().is_some_and(|l| l.name.starts_with(name))
        })
        .cloned()
        .unwrap_or_else(|| panic!("no folder {name}"));
    shell.update(
        InputEvent::Click {
            x: item.rect.x + item.rect.w / 2.0,
            y: item.rect.y + 3.0,
        },
        t(10),
    );
}

#[test]
fn the_deleted_size_carries_the_marks_of_what_was_deleted() {
    let deleted = |bytes| ElevatedItem::Deleted {
        bytes,
        shared_files: 0,
    };
    for (name, mark) in [
        ("folder1", ""),
        ("folder2", "\u{2248} "),
        ("folder3", "\u{2265} "),
    ] {
        let mut r = marked_rig(ElevatedEnd::Done(vec![deleted(12_300_000_000)]));
        click_folder(&mut r.shell, name);
        denied_selection(&mut r);
        let p = button_center(&r.shell, "Delete as administrator");
        let fx = r.shell.update(InputEvent::Click { x: p.0, y: p.1 }, t(50));
        run_job(&mut r.shell, fx, 60);
        let toast = r.shell.toast_text().unwrap();
        assert!(
            toast.starts_with(&format!("Deleted \u{b7} {mark}12.3 GB")),
            "{name}: {toast}"
        );
    }
}

#[test]
fn a_helper_that_stops_after_every_item_has_an_outcome_does_not_count_unknown_ones() {
    let mut r = rig(BinStyle::Trash, ElevatedEnd::Done(Vec::new()));
    select_first(&mut r.shell);
    *r.fake.script.end.lock().unwrap() = ElevatedEnd::Failed {
        message: "the helper closed the connection".into(),
        completed: vec![(
            0,
            ElevatedItem::Deleted {
                bytes: 4_000_000_000,
                shared_files: 0,
            },
        )],
    };
    let fx = key(&mut r.shell, Key::Delete, 20);
    let fx = run_job(&mut r.shell, fx, 30);
    settle(&mut r.shell, fx, 40);
    let p = button_center(&r.shell, "Delete as administrator");
    let fx = r.shell.update(InputEvent::Click { x: p.0, y: p.1 }, t(50));
    run_job(&mut r.shell, fx, 60);
    let toast = r.shell.toast_text().unwrap();
    assert!(toast.starts_with("Deleted \u{b7} 4.0 GB"), "{toast}");
    assert!(toast.contains("helper stopped"), "{toast}");
    assert!(!toast.contains("0 items"), "{toast}");
    assert!(!toast.contains("may or may not"), "{toast}");
}

#[test]
fn the_capacity_meter_says_overhead_after_a_full_scan() {
    let (_, snap) = build(&["a", "b"], None);
    let mut shell = shell_for(snap, 1.0);
    shell.set_capacity(
        Breakdown {
            total: 1_000_000_000_000,
            used: 500_000_000_000,
            recycle_bin: 0,
            unreadable: 23_400_000_000,
            reserved: 0,
            free: 476_600_000_000,
            quota: false,
        },
        t(0),
    );
    let value = |shell: &mut Shell<TreeSource>| {
        let mut tree = AccessTree::new();
        shell.access_update(&mut tree, t(1));
        (0..400u64)
            .filter_map(|n| tree.get(accesskit::NodeId(n)))
            .find(|n| n.label() == Some("Drive capacity"))
            .and_then(|n| n.value().map(str::to_string))
            .expect("the capacity meter")
    };
    let before = value(&mut shell);
    assert!(before.contains("Couldn't read"), "{before}");
    shell.set_offer(Offer::Done);
    let after = value(&mut shell);
    assert!(after.contains("Filesystem overhead"), "{after}");
    assert!(!after.contains("Couldn't read"), "{after}");
}

// ---- while the full scan runs -----------------------------------------------------

const RUNNING: &str = "A full scan is running";

fn no_disk_call(r: &Rig) -> bool {
    r.fake.calls().is_empty()
}

#[test]
fn nothing_that_changes_the_disk_starts_while_a_full_scan_runs() {
    let mut r = rig(BinStyle::Trash, ElevatedEnd::Done(Vec::new()));
    select_first(&mut r.shell);
    r.shell.set_offer(Offer::Running);

    // The keys say why and do nothing.
    for (k, label) in [
        (Key::Delete, "Delete"),
        (Key::DeletePermanently, "Delete permanently"),
        (Key::Undo, "Undo"),
    ] {
        let fx = key(&mut r.shell, k, 20);
        assert!(!has_job(&fx), "{label}: {fx:?}");
        assert!(!r.shell.modal_open(), "{label}");
        assert_eq!(info(&r.shell), format!("{label}: {RUNNING}"));
    }

    // The context menu offers the rows disabled, with the reason.
    let all = folders(&r.shell);
    let p = center(all[0].rect);
    r.shell.update(
        InputEvent::RightClick {
            x: p.0,
            y: p.1 - 8.0,
        },
        t(60),
    );
    assert!(r.shell.modal_open());
    let rows = r.shell.menu_items();
    for label in ["Delete", "Delete permanently"] {
        let row = rows.iter().find(|(l, _, _)| l == label).expect(label);
        assert!(!row.2, "{label} must be disabled");
        assert_eq!(row.1, RUNNING, "{label}");
    }
    key(&mut r.shell, Key::Escape, 70);

    // The hover card shows the Delete button disabled.
    r.shell.update(
        InputEvent::PointerMoved {
            x: p.0,
            y: p.1 - 8.0,
        },
        t(100),
    );
    r.shell.tick(t(700));
    let card = r.shell.card().expect("a card after a rest");
    let enabled = card.parts.iter().find_map(|part| match part {
        bs_render::CardPart::Button { label, enabled, .. } if &**label == "Delete" => {
            Some(*enabled)
        }
        _ => None,
    });
    assert_eq!(enabled, Some(false), "the card's Delete button");

    assert!(no_disk_call(&r), "{:?}", r.fake.calls());

    // Once the run is over the same key works again.
    r.shell.set_offer(Offer::Available);
    let fx = key(&mut r.shell, Key::Delete, 800);
    assert!(has_job(&fx), "{fx:?}");
}

#[test]
fn emptying_the_bin_waits_for_the_full_scan_too() {
    let mut r = rig(BinStyle::Trash, ElevatedEnd::Done(Vec::new()));
    r.shell.set_bin_drive(Some(bs_ui::shell::BinDrive {
        mount: std::path::PathBuf::from("/"),
        label: String::new(),
    }));
    r.shell.set_capacity(
        Breakdown {
            total: 100,
            free: 50,
            used: 40,
            recycle_bin: 10,
            ..Breakdown::default()
        },
        t(0),
    );
    r.shell.set_offer(Offer::Running);
    r.shell.request_empty(t(10));
    assert!(!r.shell.modal_open());
    assert_eq!(info(&r.shell), format!("Empty Trash: {RUNNING}"));
    r.shell.set_offer(Offer::Available);
    r.shell.request_empty(t(20));
    assert!(r.shell.modal_open(), "asks once the run is over");
}

#[test]
fn the_full_scan_cannot_start_while_a_delete_is_still_running() {
    let mut r = rig(BinStyle::Trash, ElevatedEnd::Done(Vec::new()));
    *r.fake.denied.lock().unwrap() = false;
    select_first(&mut r.shell);
    // The delete is under way: its job has been handed out and not answered.
    let fx = key(&mut r.shell, Key::Delete, 20);
    assert!(has_job(&fx));
    let rect = full_scan_rect(&mut r.shell).unwrap();
    let (x, y) = center(rect);
    let fx2 = r.shell.update(InputEvent::Click { x, y }, t(30));
    assert!(!fx2.iter().any(|e| matches!(e, ShellEffect::FullScan)));
    assert_eq!(
        info(&r.shell),
        "Full scan: wait for the last action to finish"
    );
    run_job(&mut r.shell, fx, 40);
    let fx = r.shell.update(InputEvent::Click { x, y }, t(5_000));
    assert!(fx.iter().any(|e| matches!(e, ShellEffect::FullScan)));
}

// ---- progress, Cancel and Esc for a full scan after the walk -------------------------

fn full_scan_readout() -> bs_ui::shell::ScanReadout {
    bs_ui::shell::ScanReadout {
        ratio: Some(0.4),
        files: 2_000_000,
        files_per_sec: 250_000.0,
        elapsed: Duration::from_secs(9),
    }
}

#[test]
fn a_full_scan_after_the_walk_has_progress_cancel_and_esc() {
    let (_, snap) = build(&["a", "b"], None);
    let mut shell = shell_for(snap, 1.0);
    // The walk is over: the shell is not scanning, and no overlay shows.
    assert!(!shell.scanning());
    assert!(
        shell
            .chrome(t(0))
            .widgets
            .iter()
            .all(|w| !matches!(w, bs_render::Widget::Pill { text, .. } if &**text == "Cancel"))
    );
    // The full scan starts.
    shell.set_offer(Offer::Running);
    shell.set_scanning(true, t(100));
    shell.set_readout(Some(full_scan_readout()));
    let widgets = shell.chrome(t(200)).widgets;
    let texts: Vec<String> = widgets
        .iter()
        .filter_map(|w| match w {
            bs_render::Widget::Text { text, .. } => Some(text.to_string()),
            _ => None,
        })
        .collect();
    assert!(
        texts
            .iter()
            .any(|s| s == "40% \u{b7} 250k files/s \u{b7} 0:09"),
        "{texts:?}"
    );
    let cancel = widgets
        .iter()
        .find_map(|w| match w {
            bs_render::Widget::Pill { rect, text, .. } if &**text == "Cancel" => Some(*rect),
            _ => None,
        })
        .expect("a Cancel button");
    let (x, y) = center(cancel);
    let fx = shell.update(InputEvent::Click { x, y }, t(300));
    assert!(
        fx.iter().any(|e| matches!(e, ShellEffect::CancelScan)),
        "{fx:?}"
    );

    // Esc does the same while the run is young.
    let fx = key(&mut shell, Key::Escape, 400);
    assert!(
        fx.iter().any(|e| matches!(e, ShellEffect::CancelScan)),
        "{fx:?}"
    );

    // When the run is over the overlay goes away and Esc does nothing.
    shell.set_offer(Offer::Available);
    shell.set_scanning(false, t(500));
    shell.set_readout(None);
    assert!(
        shell
            .chrome(t(600))
            .widgets
            .iter()
            .all(|w| !matches!(w, bs_render::Widget::Pill { text, .. } if &**text == "Cancel"))
    );
    assert!(key(&mut shell, Key::Escape, 700).is_empty());
}

#[test]
fn a_full_scan_that_has_run_a_while_asks_for_a_second_esc() {
    let (_, snap) = build(&["a", "b"], None);
    let mut shell = shell_for(snap, 1.0);
    shell.set_offer(Offer::Running);
    shell.set_scanning(true, t(0));
    shell.set_readout(Some(full_scan_readout()));
    let late = bs_ui::shell::CANCEL_CONFIRM_AFTER.as_millis() as u64 + 1_000;
    assert!(key(&mut shell, Key::Escape, late).is_empty());
    assert_eq!(info(&shell), "Press Esc again to stop the scan");
    let fx = key(&mut shell, Key::Escape, late + 500);
    assert!(fx.iter().any(|e| matches!(e, ShellEffect::CancelScan)));
}

// ---- the names in the administrator dialog ---------------------------------------------

fn many_long_names(count: usize, len: usize) -> Rig {
    let names: Vec<String> = (0..count)
        .map(|i| format!("{i:02}-{}", "word ".repeat(len / 5)))
        .collect();
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let (_, snap) = build(&refs, None);
    let mut shell = shell_for(snap, 1.0);
    let fake = Fake::new(ElevatedEnd::Done(Vec::new()));
    shell.set_disposer(fake.clone());
    shell.set_bin_style(BinStyle::Trash);
    Rig { shell, fake }
}

fn dialog_lines(shell: &Shell<TreeSource>) -> (Vec<String>, Rect, f32) {
    let popup = shell.popup().expect("a dialog");
    let lines = popup
        .card
        .parts
        .iter()
        .filter_map(|p| match p {
            bs_render::CardPart::Line { text, .. } => Some(text.to_string()),
            _ => None,
        })
        .collect();
    let buttons_bottom = popup
        .card
        .parts
        .iter()
        .filter_map(|p| match p {
            bs_render::CardPart::Button { rect, .. } | bs_render::CardPart::Hold { rect, .. } => {
                Some(rect.bottom())
            }
            _ => None,
        })
        .fold(0.0f32, f32::max);
    (lines, popup.card.rect, buttons_bottom)
}

#[test]
fn long_spaced_names_take_one_line_each_and_the_dialog_stays_in_the_window() {
    for len in [10usize, 60, 100, 140, 200, 400] {
        let mut r = many_long_names(30, len);
        let n = select_ids(&mut r.shell, 30);
        assert!(n >= 20, "{n}");
        denied_selection(&mut r);
        let text = r.shell.dialog_text().expect("a dialog");
        // A screen reader gets every name, in full.
        assert_eq!(text.matches("\u{2022} ").count(), n, "len {len}");
        let full = format!("{:02}-{}", 7, "word ".repeat(len / 5));
        assert!(
            text.contains(
                full.split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
                    .as_str()
            ),
            "len {len}: full names in the accessibility text"
        );
        let mut seen = std::collections::BTreeSet::new();
        let mut checked_end = false;
        for step in 0..80u64 {
            let (lines, card, buttons_bottom) = dialog_lines(&r.shell);
            assert!(
                card.y >= 0.0 && card.bottom() <= 800.0,
                "len {len}: {card:?}"
            );
            assert!(buttons_bottom <= card.bottom(), "len {len}");
            assert!(buttons_bottom <= 800.0, "len {len}");
            let names: Vec<&String> = lines
                .iter()
                .filter(|l| l.starts_with("\u{2022} "))
                .collect();
            // One drawn line per name: what the counter says is what shows.
            let counter = lines.iter().find(|l| l.starts_with("Items ")).cloned();
            if let Some(counter) = &counter {
                let range = counter.trim_start_matches("Items ");
                let (from, rest) = range.split_once('\u{2013}').expect("a range");
                let (to, _) = rest.split_once(" of ").expect("of n");
                let (from, to): (usize, usize) = (from.parse().unwrap(), to.parse().unwrap());
                assert_eq!(
                    to - from + 1,
                    names.len(),
                    "len {len}: {counter} vs {}",
                    names.len()
                );
            } else {
                assert_eq!(names.len(), n, "len {len}: no counter, so all show");
            }
            for l in &names {
                // Every name shows its index prefix, ellipsized in the middle.
                let idx: usize = l["\u{2022} ".len()..][..2].parse().expect("a prefix");
                seen.insert(idx);
            }
            if step == 79 {
                checked_end = true;
            }
            r.shell.update(
                InputEvent::Wheel {
                    x: 10.0,
                    y: 10.0,
                    steps: -1.0,
                },
                t(100 + step),
            );
        }
        assert!(checked_end);
        assert_eq!(seen.len(), n, "len {len}: every item is reachable");
        // The last page holds the last item, and the buttons are still there.
        let (lines, _, _) = dialog_lines(&r.shell);
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with(&format!("\u{2022} {:02}-", n - 1))),
            "len {len}: the last item shows at the end"
        );
        assert_eq!(
            r.shell.dialog_buttons(),
            ["Delete as administrator", "Cancel"]
        );
    }
}

#[test]
fn a_long_name_is_cut_in_the_middle_never_at_a_byte() {
    let mut r = many_long_names(3, 400);
    select_ids(&mut r.shell, 3);
    denied_selection(&mut r);
    let (lines, _, _) = dialog_lines(&r.shell);
    let name = lines
        .iter()
        .find(|l| l.starts_with("\u{2022} 0"))
        .expect("a name");
    assert!(name.contains('\u{2026}'), "{name}");
    assert!(name.ends_with("word"), "the tail is kept: {name}");
}

// ---- marks carried across the swap ---------------------------------------------------------

#[test]
fn marks_are_noted_by_names_and_found_in_another_tree() {
    use bs_ui::elevation::DeleteMarks;
    let (_, first) = build(&["alpha", "beta"], None);
    let (_, second) = build(&["alpha", "beta", "gamma"], Some("system"));
    let source = TreeSource::new(first, Metrics::from_logical(13.0, 1.0)).unwrap();
    let beta = source
        .folder_by_names(&[b"beta".to_vec()])
        .expect("beta in the first tree");
    let mut marks = DeleteMarks::default();
    marks.note(source.names_below_root(beta), DeleteState::InBin);
    marks.note(vec![b"gamma".to_vec()], DeleteState::Deleted);
    marks.note(vec![b"gone".to_vec()], DeleteState::Deleted);
    assert_eq!(marks.len(), 3);
    let changes = marks.changes_in(&second);
    assert_eq!(changes.len(), 2, "the tree has no \"gone\"");
    let by_name: Vec<(Vec<u8>, DeleteState)> = changes
        .iter()
        .map(|c| (second.node(c.node).unwrap().name().to_vec(), c.to))
        .collect();
    assert!(by_name.contains(&(b"beta".to_vec(), DeleteState::InBin)));
    assert!(by_name.contains(&(b"gamma".to_vec(), DeleteState::Deleted)));
    // An undo takes the note away.
    marks.note(vec![b"beta".to_vec()], DeleteState::None);
    assert_eq!(marks.len(), 2);
    marks.clear();
    assert!(marks.is_empty());
}
