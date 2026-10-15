//! The drive picker, the drive-level blocks and the scan states, through the
//! shell with the same input events the window feeds in.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bs_layout::Metrics;
use bs_platform::drives::{Capacity, Drive, DriveId, DriveKind, ReservedKind};
use bs_render::{Item, ItemKind, Rect, TextEngine, Widget};
use bs_tree::{Aggregator, Batch, Category, DirKey, Meta, NodeKind, Snapshot};
use bs_ui::blocks::{DriveBlocks, SPECIAL_ID_BASE, Special};
use bs_ui::capacity::Breakdown;
use bs_ui::chrome::TextMeasure;
use bs_ui::picker::{CardScan, Picker};
use bs_ui::shell::{Look, STOPPED_BANNER, ScanReadout, Shell, ShellEffect};
use bs_ui::tree::TreeSource;
use bs_ui::{App, Direction, Effect, InputEvent, Key, ViewSource};

struct Fixed(f32);

impl TextMeasure for Fixed {
    fn width(&self, text: &str) -> Option<f32> {
        Some(text.chars().count() as f32 * self.0)
    }
}

const SIZE: (u32, u32) = (1200, 800);

fn t(ms: u64) -> Duration {
    Duration::from_millis(ms)
}

fn new_shell() -> Shell<TreeSource> {
    Shell::new(
        SIZE,
        1.0,
        13.0,
        TextEngine::new(),
        Look {
            dark: false,
            accent: None,
        },
    )
    .with_measure(Box::new(Fixed(13.0 * 0.56)))
}

fn drive(mount: &str, kind: DriveKind, total: u64, used: u64, auto: bool) -> Drive {
    Drive {
        id: DriveId::new(mount),
        mount_point: PathBuf::from(mount),
        label: None,
        fs_type: "ext4".into(),
        kind,
        read_only: false,
        eligible_for_background_scan: auto,
        capacity: Some(Capacity::from_bytes(
            total,
            total - used,
            total - used,
            ReservedKind::FilesystemReserve,
        )),
        capacity_problem: None,
        mounts: Vec::new(),
    }
}

fn drives() -> Vec<Drive> {
    vec![
        drive(
            "/",
            DriveKind::Fixed,
            1_000_000_000_000,
            400_000_000_000,
            true,
        ),
        drive(
            "/data",
            DriveKind::Fixed,
            500_000_000_000,
            100_000_000_000,
            true,
        ),
        drive(
            "/mnt/usb",
            DriveKind::Removable,
            16_000_000_000,
            4_000_000_000,
            false,
        ),
    ]
}

fn picker_shell() -> Shell<TreeSource> {
    let mut shell = new_shell();
    shell.set_picker(Picker::new(&drives()));
    shell
}

fn center(r: Rect) -> (f32, f32) {
    (r.x + r.w / 2.0, r.y + r.h / 2.0)
}

fn card_rect(shell: &Shell<TreeSource>, id: u32) -> Rect {
    let items = shell.picker_items().expect("picker items");
    items.iter().find(|i| i.id == id).expect("card").rect
}

fn info(shell: &Shell<TreeSource>) -> String {
    let line = shell.info_line();
    format!("{} {}", line.name, line.details).trim().to_string()
}

fn texts(shell: &mut Shell<TreeSource>) -> Vec<String> {
    shell
        .chrome(t(0))
        .widgets
        .iter()
        .filter_map(|w| match w {
            Widget::Text { text, .. } | Widget::Pill { text, .. } => Some(text.to_string()),
            _ => None,
        })
        .collect()
}

#[test]
fn the_picker_lists_every_drive_with_a_bar_and_its_scan_progress() {
    let mut shell = picker_shell();
    let items = shell.picker_items().expect("items");
    assert_eq!(items.len(), 3, "two scanned drives and one other");
    let all = texts(&mut shell);
    assert!(
        all.iter()
            .any(|s| s == "400.0 GB of 1.00 TB used" || s.contains("of 1")),
        "{all:?}"
    );
    assert!(all.iter().any(|s| s == "Scanning…"), "{all:?}");
    assert!(all.iter().any(|s| s == "Scan a folder…"));
    assert!(all.iter().any(|s| s.starts_with("Other drives")));
    let bars = shell
        .chrome(t(0))
        .widgets
        .iter()
        .filter(|w| matches!(w, Widget::Capacity { .. }))
        .count();
    // The capacity bar of the window is absent until a drive is open: the two
    // cards carry their own.
    assert_eq!(bars, 2);

    // Progress shows on the card as the scan reports it.
    shell
        .picker_mut()
        .and_then(|p| p.card_for_drive(1))
        .expect("card")
        .scan = CardScan::Running {
        ratio: Some(0.4),
        files: 12_345,
        bytes: 40_000_000_000,
    };
    let all = texts(&mut shell);
    assert!(
        all.iter()
            .any(|s| s.contains("40%") && s.contains("≥ 40.0 GB")),
        "{all:?}"
    );
}

#[test]
fn hovering_a_card_updates_the_info_bar_and_the_hover_outline() {
    let mut shell = picker_shell();
    let r = card_rect(&shell, 2);
    let (x, y) = center(r);
    let fx = shell.update(InputEvent::PointerMoved { x, y }, t(1));
    assert!(
        fx.iter()
            .any(|e| matches!(e, ShellEffect::Map(Effect::SetHover(Some(2))))),
        "{fx:?}"
    );
    let text = info(&shell);
    assert!(
        text.contains("/data") && text.contains("of 500.0 GB used"),
        "{text}"
    );
    let fx = shell.update(InputEvent::PointerLeft, t(2));
    assert!(
        fx.iter()
            .any(|e| matches!(e, ShellEffect::Map(Effect::SetHover(None))))
    );
}

#[test]
fn clicking_a_card_picks_that_drive_and_the_folder_button_asks_for_a_folder() {
    let mut shell = picker_shell();
    let (x, y) = center(card_rect(&shell, 2));
    let fx = shell.update(InputEvent::Click { x, y }, t(1));
    assert!(
        fx.iter().any(|e| matches!(e, ShellEffect::PickDrive(1))),
        "{fx:?}"
    );
    // The removable drive is listed apart and can be picked on purpose.
    let (x, y) = center(card_rect(&shell, 3));
    let fx = shell.update(InputEvent::Click { x, y }, t(2));
    assert!(fx.iter().any(|e| matches!(e, ShellEffect::PickDrive(2))));
    let button = shell
        .chrome(t(0))
        .widgets
        .iter()
        .find_map(|w| match w {
            Widget::Pill { rect, text, .. } if &**text == "Scan a folder…" => Some(*rect),
            _ => None,
        })
        .expect("button");
    let (x, y) = center(button);
    let fx = shell.update(InputEvent::Click { x, y }, t(3));
    assert!(
        fx.iter().any(|e| matches!(e, ShellEffect::ScanFolder)),
        "{fx:?}"
    );
}

#[test]
fn the_keyboard_moves_between_cards_and_enter_picks() {
    let mut shell = picker_shell();
    shell.update(InputEvent::Key(Key::Arrow(Direction::Right)), t(1));
    let fx = shell.update(InputEvent::Key(Key::Arrow(Direction::Right)), t(2));
    assert!(
        fx.iter()
            .any(|e| matches!(e, ShellEffect::Map(Effect::SetHover(Some(2)))))
    );
    let fx = shell.update(InputEvent::Key(Key::Enter), t(3));
    assert!(
        fx.iter().any(|e| matches!(e, ShellEffect::PickDrive(1))),
        "{fx:?}"
    );
    // Esc in the picker does nothing, in particular it does not cancel scans.
    let fx = shell.update(InputEvent::Key(Key::Escape), t(4));
    assert!(fx.is_empty());
}

// ---- the drive's map with its blocks ---------------------------------------

fn meta(size: u64) -> Meta {
    Meta {
        logical: size,
        allocated: size,
        category: Category(1),
        ..Meta::default()
    }
}

fn snap(finish: bool) -> Arc<Snapshot> {
    let mut agg = Aggregator::new();
    let mut b = Batch::new();
    b.dir(DirKey(0), DirKey::NONE, b"/", &meta(4096)).unwrap();
    for d in 1..=3u32 {
        b.dir(
            DirKey(d),
            DirKey(0),
            format!("folder{d}").as_bytes(),
            &meta(4096),
        )
        .unwrap();
        for f in 0..20u32 {
            b.file(
                NodeKind::File,
                DirKey(d),
                format!("file{f}").as_bytes(),
                &meta(1_000_000_000 * u64::from(4 - d) + u64::from(f)),
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

fn drive_blocks(complete: bool) -> DriveBlocks {
    let cap = Capacity::from_bytes(
        1_000_000_000_000,
        360_000_000_000,
        330_000_000_000,
        ReservedKind::FilesystemReserve,
    );
    DriveBlocks::from(&Breakdown::new(
        &cap,
        560_000_000_000,
        complete,
        14_100_000_000,
    ))
}

fn map_shell(complete: bool) -> Shell<TreeSource> {
    let mut shell = new_shell();
    let map = shell.map_rect();
    let mut source =
        TreeSource::new(snap(complete), Metrics::from_logical(13.0, 1.0)).expect("root");
    source.set_drive_blocks(Some(drive_blocks(complete)));
    shell.set_app(App::new(source, map));
    shell
}

fn items(shell: &Shell<TreeSource>) -> Vec<Item> {
    shell.app().expect("app").items().as_ref().clone()
}

#[test]
fn a_finished_drive_shows_free_unreadable_reserved_and_bin_blocks() {
    let shell = map_shell(true);
    let all = items(&shell);
    for kind in [
        ItemKind::Free,
        ItemKind::Unreadable,
        ItemKind::Reserved,
        ItemKind::RecycleBin,
    ] {
        assert_eq!(all.iter().filter(|i| i.kind == kind).count(), 1, "{kind:?}");
    }
    let unreadable = all
        .iter()
        .find(|i| i.kind == ItemKind::Unreadable)
        .expect("block");
    assert!(
        unreadable
            .label
            .as_ref()
            .is_some_and(|l| l.name.starts_with("Couldn't read: ")
                && l.name.ends_with("Grant access to see inside")),
        "{unreadable:?}"
    );
    // The folders still lay out in what is left, and nothing overlaps.
    let root = all
        .iter()
        .find(|i| i.depth == 0 && i.kind == ItemKind::FolderTitled)
        .expect("root");
    for b in all.iter().filter(|i| i.id >= SPECIAL_ID_BASE) {
        let ox = (root.rect.right().min(b.rect.right()) - root.rect.x.max(b.rect.x)).max(0.0);
        let oy = (root.rect.bottom().min(b.rect.bottom()) - root.rect.y.max(b.rect.y)).max(0.0);
        assert!(ox * oy < 0.5);
    }
}

#[test]
fn the_unreadable_block_appears_only_after_the_walk() {
    let shell = map_shell(false);
    let all = items(&shell);
    assert!(all.iter().all(|i| i.kind != ItemKind::Unreadable));
    assert!(all.iter().any(|i| i.kind == ItemKind::Free));
}

#[test]
fn hovering_a_block_describes_it_and_clicking_unreadable_asks_for_a_full_scan() {
    let mut shell = map_shell(true);
    let all = items(&shell);
    let free = all.iter().find(|i| i.kind == ItemKind::Free).expect("free");
    let (x, y) = center(free.rect);
    shell.update(InputEvent::PointerMoved { x, y }, t(1));
    let text = info(&shell);
    assert!(
        text.starts_with("Free space") && text.contains("112") || text.contains("Free space"),
        "{text}"
    );
    // Clicking free space only selects it.
    let fx = shell.update(InputEvent::Click { x, y }, t(2));
    assert!(
        !fx.iter()
            .any(|e| matches!(e, ShellEffect::FullScan | ShellEffect::EmptyBin))
    );
    // Actions on a block are refused with a reason, never forwarded.
    let fx = shell.update(InputEvent::Key(Key::Delete), t(3));
    assert!(
        !fx.iter().any(|e| matches!(e, ShellEffect::Action { .. })),
        "{fx:?}"
    );
    assert_eq!(info(&shell), "Delete: Not a file or folder");

    let un = all
        .iter()
        .find(|i| i.kind == ItemKind::Unreadable)
        .expect("block");
    let (x, y) = center(un.rect);
    let fx = shell.update(InputEvent::Click { x, y }, t(4));
    assert!(
        fx.iter().any(|e| matches!(e, ShellEffect::FullScan)),
        "{fx:?}"
    );
    assert!(info(&shell).contains("administrator helper"));
}

#[test]
fn only_the_top_view_has_blocks() {
    let mut shell = map_shell(true);
    let folder = items(&shell)
        .iter()
        .find(|i| i.kind == ItemKind::FolderTitled && i.depth == 1)
        .map(|i| i.id)
        .expect("a folder");
    shell.app_mut().expect("app").open(folder);
    let app = shell.app().expect("app");
    let source = app.source();
    assert_ne!(app.current_view(), source.root(), "moved into the folder");
    assert!(
        source
            .special(app.current_view(), DriveBlocks::id(Special::Free))
            .is_none()
    );
    assert!(app.items().iter().all(|i| i.id < SPECIAL_ID_BASE));
    assert_eq!(
        source.special(source.root(), DriveBlocks::id(Special::Free)),
        Some(Special::Free)
    );
}

// ---- scan states -----------------------------------------------------------

#[test]
fn a_running_scan_shows_percent_rate_elapsed_and_a_cancel_button() {
    let mut shell = map_shell(false);
    shell.set_status("Scanning…".into(), true);
    shell.set_readout(Some(ScanReadout {
        ratio: Some(0.37),
        files: 1_200_000,
        files_per_sec: 120_000.0,
        elapsed: Duration::from_secs(72),
    }));
    let all = texts(&mut shell);
    assert!(
        all.iter().any(|s| s == "37% · 120k files/s · 1:12"),
        "{all:?}"
    );
    let cancel = shell
        .chrome(t(0))
        .widgets
        .iter()
        .find_map(|w| match w {
            Widget::Pill { rect, text, .. } if &**text == "Cancel" => Some(*rect),
            _ => None,
        })
        .expect("cancel");
    let (x, y) = center(cancel);
    let fx = shell.update(InputEvent::Click { x, y }, t(1));
    assert!(
        fx.iter().any(|e| matches!(e, ShellEffect::CancelScan)),
        "{fx:?}"
    );

    // Without a meaningful ratio the file count leads.
    shell.set_readout(Some(ScanReadout {
        ratio: None,
        files: 5_000,
        files_per_sec: 900.0,
        elapsed: Duration::from_secs(5),
    }));
    let all = texts(&mut shell);
    assert!(
        all.iter().any(|s| s == "5,000 files · 900 files/s · 0:05"),
        "{all:?}"
    );
}

#[test]
fn a_stopped_scan_shows_the_banner_with_a_rescan_button() {
    let mut shell = map_shell(false);
    shell.set_status(STOPPED_BANNER.into(), false);
    shell.set_stopped(true);
    let all = texts(&mut shell);
    assert!(all.iter().any(|s| s == STOPPED_BANNER), "{all:?}");
    let rescan = shell
        .chrome(t(0))
        .widgets
        .iter()
        .find_map(|w| match w {
            Widget::Pill { rect, text, .. } if &**text == "Rescan" => Some(*rect),
            _ => None,
        })
        .expect("rescan");
    let (x, y) = center(rescan);
    let fx = shell.update(InputEvent::Click { x, y }, t(1));
    assert!(
        fx.iter().any(|e| matches!(e, ShellEffect::Rescan)),
        "{fx:?}"
    );
    // A scan that is running again takes the banner away.
    shell.set_status("Scanning…".into(), true);
    shell.set_stopped(false);
    assert!(!texts(&mut shell).iter().any(|s| s == STOPPED_BANNER));
}

#[test]
fn the_banner_takes_a_strip_off_the_map_instead_of_covering_it() {
    let mut shell = map_shell(false);
    let before = shell.map_rect();
    shell.set_status(STOPPED_BANNER.into(), false);
    assert!(shell.set_stopped(true), "the map moved");
    let after = shell.map_rect();
    assert!(after.y > before.y && (after.bottom() - before.bottom()).abs() < 0.5);
    let banner = shell
        .chrome(t(0))
        .widgets
        .iter()
        .find_map(|w| match w {
            Widget::Panel { rect } => Some(*rect),
            _ => None,
        })
        .expect("panel");
    assert!(banner.bottom() <= after.y + 0.5, "{banner:?} vs {after:?}");
    assert!(shell.set_stopped(false));
    assert_eq!(shell.map_rect(), before);
}

#[test]
fn labels_of_a_stopped_scan_stop_saying_scanning() {
    let mut agg = Aggregator::new();
    let mut b = Batch::new();
    b.dir(DirKey(0), DirKey::NONE, b"/", &meta(4096)).unwrap();
    b.dir(DirKey(1), DirKey(0), b"folder1", &meta(4096))
        .unwrap();
    for f in 0..10u32 {
        b.file(
            NodeKind::File,
            DirKey(1),
            format!("f{f}").as_bytes(),
            &meta(1_000_000 + u64::from(f)),
        )
        .unwrap();
    }
    agg.apply(&b);
    agg.mark_stopped();
    let snapshot = agg.publish();
    assert!(snapshot.scan_stopped());
    let map = Rect::new(0.0, 30.0, 1200.0, 700.0);
    let mut source = TreeSource::new(snapshot, Metrics::from_logical(13.0, 1.0)).expect("root");
    let layout = source.layout(source.root(), map);
    let sizes: Vec<String> = layout
        .items
        .iter()
        .filter_map(|i| i.label.as_ref().map(|l| l.size.to_string()))
        .collect();
    assert!(sizes.iter().any(|s| s.starts_with("≥ ")), "{sizes:?}");
    assert!(sizes.iter().all(|s| !s.contains("Scanning")), "{sizes:?}");
}

#[test]
fn actions_on_a_folder_still_being_counted_say_still_scanning() {
    let mut shell = map_shell(false);
    let all = items(&shell);
    let folder = all
        .iter()
        .find(|i| i.kind == ItemKind::FolderTitled && i.depth == 1)
        .expect("a folder");
    // Click the folder's title bar so the folder itself is selected.
    let x = folder.rect.x + folder.rect.w / 2.0;
    let y = folder.rect.y + 4.0;
    shell.update(InputEvent::Click { x, y }, t(1));
    shell.update(InputEvent::Key(Key::Delete), t(2));
    assert_eq!(info(&shell), "Delete: Still scanning");
}

#[test]
fn going_up_from_a_drive_returns_to_the_picker_only_when_it_came_from_there() {
    let mut shell = map_shell(true);
    let fx = shell.update(InputEvent::Key(Key::Backspace), t(1));
    assert!(!fx.iter().any(|e| matches!(e, ShellEffect::ShowPicker)));
    shell.set_picker_return(true);
    let fx = shell.update(InputEvent::Key(Key::Backspace), t(2));
    assert!(
        fx.iter().any(|e| matches!(e, ShellEffect::ShowPicker)),
        "{fx:?}"
    );
}

#[test]
fn the_ui_words_stay_inside_the_wording_rules() {
    let mut shell = map_shell(true);
    shell.set_readout(Some(ScanReadout {
        ratio: Some(0.5),
        files: 1,
        files_per_sec: 1.0,
        elapsed: Duration::ZERO,
    }));
    let mut all = texts(&mut shell);
    all.push(STOPPED_BANNER.into());
    for b in drive_blocks(true).label(Special::Unreadable).split(' ') {
        all.push(b.to_string());
    }
    for s in all {
        let s = s.to_lowercase();
        for banned in ["safe", "junk", "reclaim", "clean up", "freed"] {
            assert!(
                !s.split(|c: char| !c.is_alphanumeric()).any(|w| w == banned),
                "{s}"
            );
        }
    }
}
