//! Hand-offs on the hover card and in the menu, the sharing caveats, and the
//! folder that is not a real folder. A fake stands in for the platform's
//! lookup of which program or package owns a folder; time is part of the
//! events, so nothing here sleeps.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bs_layout::Metrics;
use bs_render::{CardPart, Item, ItemKind, Rect, TextEngine};
use bs_tree::{Aggregator, Batch, Category, DirKey, FileKey, Meta, NodeFlags, NodeKind, Snapshot};
use bs_ui::chrome::TextMeasure;
use bs_ui::owners::{Owners, Ownership, PackageInfo, ProgramInfo};
use bs_ui::shell::{Look, Shell, ShellEffect};
use bs_ui::tree::TreeSource;
use bs_ui::{App, InputEvent, Key, ViewSource};

// ---- fixture -----------------------------------------------------------------

struct Fixed(f32);

impl TextMeasure for Fixed {
    fn width(&self, text: &str) -> Option<f32> {
        Some(text.chars().count() as f32 * self.0)
    }
}

#[derive(Default)]
struct FakeOwners {
    answers: Mutex<HashMap<PathBuf, Ownership>>,
    requests: Mutex<Vec<Vec<(PathBuf, u64)>>>,
}

impl FakeOwners {
    fn set(&self, path: &str, owner: Ownership) {
        self.answers
            .lock()
            .unwrap()
            .insert(PathBuf::from(path), owner);
    }
}

impl Owners for FakeOwners {
    fn request(&self, folders: &[(PathBuf, u64)]) {
        self.requests.lock().unwrap().push(folders.to_vec());
    }

    fn lookup(&self, path: &Path) -> Ownership {
        self.answers
            .lock()
            .unwrap()
            .get(path)
            .cloned()
            .unwrap_or(Ownership::Unknown)
    }

    fn is_idle(&self) -> bool {
        true
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

fn dir(b: &mut Batch, key: u32, parent: DirKey, name: &[u8], flags: NodeFlags) {
    let m = Meta {
        flags,
        ..Meta::default()
    };
    b.dir(DirKey(key), parent, name, &m).unwrap();
}

/// `/data` with `folder1`..`folder3`, 20 files in each, all fully counted.
fn plain_tree() -> Arc<Snapshot> {
    let mut agg = Aggregator::new();
    let mut b = Batch::new();
    b.dir(DirKey(0), DirKey::NONE, b"/data", &meta(4096))
        .unwrap();
    for i in 0..3u32 {
        let key = DirKey(i + 1);
        b.dir(
            key,
            DirKey(0),
            format!("folder{}", i + 1).as_bytes(),
            &meta(4096),
        )
        .unwrap();
        for f in 0..20u32 {
            let size = 1_000_000 * (3 - u64::from(i)) + u64::from(f);
            b.file(
                NodeKind::File,
                key,
                format!("file{f}").as_bytes(),
                &meta(size),
            )
            .unwrap();
        }
        b.listing_done(key);
    }
    b.listing_done(DirKey(0));
    agg.apply(&b);
    agg.publish()
}

/// `/data` with `lost` (made up by the reader, holding `deep/file`), `real`
/// and `snap`, whose files share data.
fn odd_tree() -> Arc<Snapshot> {
    let mut agg = Aggregator::new();
    let mut b = Batch::new();
    dir(&mut b, 0, DirKey::NONE, b"/data", NodeFlags::EMPTY);
    dir(&mut b, 1, DirKey(0), b"lost", NodeFlags::SYNTHETIC);
    dir(&mut b, 2, DirKey(1), b"deep", NodeFlags::EMPTY);
    dir(&mut b, 3, DirKey(0), b"real", NodeFlags::EMPTY);
    dir(&mut b, 4, DirKey(0), b"snap", NodeFlags::EMPTY);
    dir(&mut b, 5, DirKey(0), b"links", NodeFlags::EMPTY);
    b.file(NodeKind::File, DirKey(2), b"orphan.bin", &meta(3_000_000))
        .unwrap();
    b.file(NodeKind::File, DirKey(3), b"ok.bin", &meta(5_000_000))
        .unwrap();
    // A file whose data is mostly shared with a snapshot: 1 MB of its
    // 4 MB is its own.
    let shared = Meta {
        flags: NodeFlags::SHARED,
        exclusive: 1_000_000,
        ..meta(4_000_000)
    };
    b.file(NodeKind::File, DirKey(4), b"vm.img", &shared)
        .unwrap();
    b.file(NodeKind::File, DirKey(4), b"plain.bin", &meta(2_000_000))
        .unwrap();
    // Two names of one file.
    for name in [&b"one"[..], b"two"] {
        let m = Meta {
            flags: NodeFlags::SHARED,
            exclusive: 2_000_000,
            link: Some(FileKey { volume: 1, id: 9 }),
            ..meta(2_000_000)
        };
        b.file(NodeKind::File, DirKey(5), name, &m).unwrap();
    }
    for k in 0..6 {
        b.listing_done(DirKey(k));
    }
    agg.apply(&b);
    agg.publish()
}

const SIZE: (u32, u32) = (1200, 800);

fn t(ms: u64) -> Duration {
    Duration::from_millis(ms)
}

struct Rig {
    shell: Shell<TreeSource>,
    owners: Arc<FakeOwners>,
}

fn rig(snapshot: Arc<Snapshot>) -> Rig {
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
    let mut source = TreeSource::new(snapshot, Metrics::from_logical(13.0, 1.0)).expect("root");
    let owners = Arc::new(FakeOwners::default());
    source.set_owners(Some(owners.clone()));
    shell.set_app(App::new(source, map));
    Rig { shell, owners }
}

fn named(shell: &Shell<TreeSource>, name: &str) -> Item {
    shell
        .app()
        .unwrap()
        .items()
        .iter()
        .find(|i| i.label.as_deref().is_some_and(|l| &*l.name == name))
        .cloned()
        .unwrap_or_else(|| panic!("no box called {name}"))
}

fn title_point(r: Rect) -> (f32, f32) {
    (r.x + r.w / 2.0, r.y + 3.0)
}

fn inside(r: Rect) -> (f32, f32) {
    (r.x + r.w / 2.0, r.y + r.h / 2.0)
}

/// Rests the pointer on a box until its card shows, at time `at` plus the
/// rest. Returns the card's lines.
fn show_card(shell: &mut Shell<TreeSource>, item: &Item, at: u64) {
    let p = if item.kind == ItemKind::FolderTitled {
        title_point(item.rect)
    } else {
        inside(item.rect)
    };
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

/// The card's buttons: label and whether it works.
fn card_buttons(shell: &Shell<TreeSource>) -> Vec<(String, bool)> {
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

fn click_card_button(shell: &mut Shell<TreeSource>, label: &str, at: u64) -> Vec<ShellEffect> {
    let rect = shell
        .card()
        .unwrap()
        .parts
        .iter()
        .find_map(|p| match p {
            CardPart::Button { rect, label: l, .. } if &**l == label => Some(*rect),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no button {label}"));
    let (x, y) = inside(rect);
    shell.update(InputEvent::PointerMoved { x, y }, t(at));
    shell.update(InputEvent::Click { x, y }, t(at + 5))
}

fn program(whole: bool, uninstall: bool) -> Ownership {
    Ownership::Program(ProgramInfo {
        name: "Acme Editor".into(),
        publisher: Some("Acme Corp".into()),
        whole_folder: whole,
        can_uninstall: uninstall,
    })
}

fn package(software: bool, command: Option<&str>, others: u32) -> Ownership {
    Ownership::Package(PackageInfo {
        name: "foo".into(),
        manager: "dpkg",
        other_packages: others,
        removal_command: command.map(str::to_string),
        software_entry: software,
    })
}

// ---- Windows: Uninstall… -------------------------------------------------------

#[test]
fn a_program_folder_offers_uninstall_through_its_registered_uninstaller() {
    let mut r = rig(plain_tree());
    r.owners.set("/data/folder1", program(true, true));
    let item = named(&r.shell, "folder1");
    show_card(&mut r.shell, &item, 100);
    let texts = card_texts(&r.shell);
    assert!(
        texts.contains(&"Installed program: Acme Editor by Acme Corp.".to_string()),
        "{texts:?}"
    );
    assert!(card_buttons(&r.shell).contains(&("Uninstall…".to_string(), true)));
    let fx = click_card_button(&mut r.shell, "Uninstall…", 800);
    let asked = fx.iter().any(|e| {
        matches!(
            e,
            ShellEffect::Action {
                action: bs_ui::action::Action::Handoff(bs_knowledge::Handoff::Uninstall),
                items,
                ..
            } if items == &[item.id]
        )
    });
    assert!(asked, "{fx:?}");
}

#[test]
fn a_part_of_a_program_says_that_uninstalling_removes_all_of_it() {
    let mut r = rig(plain_tree());
    r.owners.set("/data/folder2", program(false, true));
    let item = named(&r.shell, "folder2");
    show_card(&mut r.shell, &item, 100);
    let texts = card_texts(&r.shell).join("\n");
    assert!(
        texts.contains(
            "Part of the installed program Acme Editor by Acme Corp. Uninstalling removes the whole program, not just this folder."
        ),
        "{texts}"
    );
}

#[test]
fn a_program_without_an_uninstaller_gives_the_reason() {
    let mut r = rig(plain_tree());
    r.owners.set("/data/folder1", program(true, false));
    let item = named(&r.shell, "folder1");
    show_card(&mut r.shell, &item, 100);
    assert!(card_buttons(&r.shell).contains(&("Uninstall…".to_string(), false)));
    let texts = card_texts(&r.shell);
    assert!(
        texts.contains(&"Uninstall… unavailable: This program registered no uninstaller".into()),
        "{texts:?}"
    );
}

#[test]
fn the_menu_lists_uninstall_after_the_zoom_group_when_a_program_owns_the_folder() {
    let mut r = rig(plain_tree());
    r.owners.set("/data/folder1", program(true, true));
    let item = named(&r.shell, "folder1");
    let (x, y) = title_point(item.rect);
    r.shell.update(InputEvent::RightClick { x, y }, t(100));
    let rows = r.shell.menu_items();
    let at = |label: &str| rows.iter().position(|r| r.0 == label);
    let uninstall = at("Uninstall…").expect("an Uninstall… row");
    assert!(rows[uninstall].2, "enabled");
    assert!(at("Rescan this folder").unwrap() < uninstall);
    assert!(uninstall < at("Delete").unwrap());
}

#[test]
fn a_folder_nobody_owns_gets_no_hand_off_row() {
    let mut r = rig(plain_tree());
    r.owners.set("/data/folder1", Ownership::Nobody);
    let item = named(&r.shell, "folder1");
    let (x, y) = title_point(item.rect);
    r.shell.update(InputEvent::RightClick { x, y }, t(100));
    let labels: Vec<String> = r.shell.menu_items().into_iter().map(|r| r.0).collect();
    for hand_off in ["Uninstall…", "Open in Software", "Copy removal command"] {
        assert!(!labels.iter().any(|l| l == hand_off), "{labels:?}");
    }
}

// ---- Linux: the package --------------------------------------------------------

#[test]
fn a_packaged_folder_says_which_package_and_offers_software_and_a_command() {
    let mut r = rig(plain_tree());
    r.shell.set_software_center(true);
    r.owners.set(
        "/data/folder1",
        package(true, Some("sudo apt remove foo"), 2),
    );
    let item = named(&r.shell, "folder1");
    show_card(&mut r.shell, &item, 100);
    let texts = card_texts(&r.shell);
    assert!(
        texts.contains(&"Installed by package foo · 2 other packages also put files here".into()),
        "{texts:?}"
    );
    let buttons = card_buttons(&r.shell);
    assert!(buttons.contains(&("Open in Software".to_string(), true)));
    assert!(buttons.contains(&("Copy removal command".to_string(), true)));
    // The command is copied, never run.
    let fx = click_card_button(&mut r.shell, "Copy removal command", 800);
    let copied: Vec<&str> = fx
        .iter()
        .filter_map(|e| match e {
            ShellEffect::CopyText(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(copied, ["sudo apt remove foo"]);
    assert_eq!(r.shell.info_line().name, "Copied the removal command");
    assert!(
        !fx.iter().any(|e| matches!(e, ShellEffect::Action { .. })),
        "nothing is run for a command to copy"
    );
}

#[test]
fn open_in_software_goes_to_the_host_and_is_disabled_without_a_software_center() {
    let mut r = rig(plain_tree());
    r.owners.set(
        "/data/folder1",
        package(true, Some("sudo apt remove foo"), 0),
    );
    let item = named(&r.shell, "folder1");
    // No software center found yet.
    show_card(&mut r.shell, &item, 100);
    assert!(card_buttons(&r.shell).contains(&("Open in Software".to_string(), false)));
    assert!(
        card_texts(&r.shell)
            .contains(&"Open in Software unavailable: No software center found".into())
    );
    // The desktop turns out to have one.
    r.shell.set_software_center(true);
    r.shell.owners_changed();
    assert!(card_buttons(&r.shell).contains(&("Open in Software".to_string(), true)));
    let fx = click_card_button(&mut r.shell, "Open in Software", 900);
    assert!(fx.iter().any(|e| matches!(
        e,
        ShellEffect::Action {
            action: bs_ui::action::Action::OpenInSoftware,
            ..
        }
    )));
}

#[test]
fn a_package_without_an_entry_or_a_command_offers_only_what_it_has() {
    let mut r = rig(plain_tree());
    r.owners.set("/data/folder1", package(false, None, 1));
    let item = named(&r.shell, "folder1");
    show_card(&mut r.shell, &item, 100);
    let labels: Vec<String> = card_buttons(&r.shell).into_iter().map(|b| b.0).collect();
    assert!(
        !labels.contains(&"Open in Software".to_string()),
        "{labels:?}"
    );
    assert!(!labels.contains(&"Copy removal command".to_string()));
    assert!(
        card_texts(&r.shell)
            .contains(&"Installed by package foo · 1 other package also puts files here".into())
    );
}

#[test]
fn the_menu_offers_the_package_actions() {
    let mut r = rig(plain_tree());
    r.shell.set_software_center(true);
    r.owners.set(
        "/data/folder1",
        package(true, Some("sudo apt remove foo"), 0),
    );
    let item = named(&r.shell, "folder1");
    let (x, y) = title_point(item.rect);
    r.shell.update(InputEvent::RightClick { x, y }, t(100));
    let rows = r.shell.menu_items();
    for label in ["Open in Software", "Copy removal command"] {
        let row = rows.iter().find(|r| r.0 == label).expect(label);
        assert!(row.2, "{label}");
    }
}

#[test]
fn the_answers_that_arrive_later_change_the_card_that_is_up() {
    let mut r = rig(plain_tree());
    r.owners.set("/data/folder1", Ownership::Checking);
    let item = named(&r.shell, "folder1");
    show_card(&mut r.shell, &item, 100);
    assert!(
        card_texts(&r.shell).contains(&"Checking which program or package this belongs to…".into())
    );
    assert!(!card_buttons(&r.shell).iter().any(|b| b.0 == "Uninstall…"));
    r.owners.set("/data/folder1", program(true, true));
    r.shell.owners_changed();
    assert!(card_buttons(&r.shell).contains(&("Uninstall…".to_string(), true)));
    assert!(
        !card_texts(&r.shell)
            .iter()
            .any(|t| t.starts_with("Checking which"))
    );
}

#[test]
fn files_are_never_asked_about() {
    let mut r = rig(plain_tree());
    r.owners.set("/data/folder1/file0", program(true, true));
    let file = r
        .shell
        .app()
        .unwrap()
        .items()
        .iter()
        .find(|i| i.kind == ItemKind::FileTile)
        .cloned()
        .unwrap();
    show_card(&mut r.shell, &file, 100);
    assert!(!card_buttons(&r.shell).iter().any(|b| b.0 == "Uninstall…"));
}

// ---- which folders are worth asking about ------------------------------------------

#[test]
fn only_big_folders_near_the_top_are_handed_to_the_matcher() {
    let mut agg = Aggregator::new();
    let mut b = Batch::new();
    b.dir(DirKey(0), DirKey::NONE, b"/data", &meta(4096))
        .unwrap();
    b.dir(DirKey(1), DirKey(0), b"big", &meta(4096)).unwrap();
    b.dir(DirKey(2), DirKey(0), b"small", &meta(4096)).unwrap();
    b.dir(DirKey(3), DirKey(1), b"inner", &meta(4096)).unwrap();
    b.file(NodeKind::File, DirKey(3), b"blob.bin", &meta(150_000_000))
        .unwrap();
    b.file(NodeKind::File, DirKey(2), b"note.txt", &meta(1_000))
        .unwrap();
    for k in 0..4 {
        b.listing_done(DirKey(k));
    }
    agg.apply(&b);
    let snap = agg.publish();
    let found = bs_ui::owners::candidates(&snap);
    let paths: Vec<&str> = found.iter().map(|(p, _)| p.to_str().unwrap()).collect();
    assert_eq!(paths, ["/data", "/data/big", "/data/big/inner"]);
    assert!(found.iter().all(|(_, bytes)| *bytes >= 100_000_000));
}

#[test]
fn asking_in_the_background_returns_at_once_and_hands_over_later() {
    let mut agg = Aggregator::new();
    let mut b = Batch::new();
    b.dir(DirKey(0), DirKey::NONE, b"/data", &meta(4096))
        .unwrap();
    b.file(NodeKind::File, DirKey(0), b"blob.bin", &meta(150_000_000))
        .unwrap();
    b.listing_done(DirKey(0));
    agg.apply(&b);
    let owners = Arc::new(FakeOwners::default());
    let rx = bs_ui::owners::request_in_background(owners.clone(), agg.publish());
    rx.recv_timeout(Duration::from_secs(10))
        .expect("the folders were handed over");
    let asked = owners.requests.lock().unwrap().clone();
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0][0].0, PathBuf::from("/data"));
    // A scan with nothing big hands over nothing, and the receiver says so by
    // closing.
    let mut agg = Aggregator::new();
    let mut b = Batch::new();
    b.dir(DirKey(0), DirKey::NONE, b"/tiny", &meta(4096))
        .unwrap();
    b.listing_done(DirKey(0));
    agg.apply(&b);
    let owners = Arc::new(FakeOwners::default());
    let rx = bs_ui::owners::request_in_background(owners.clone(), agg.publish());
    assert!(rx.recv_timeout(Duration::from_secs(10)).is_err());
    assert!(owners.requests.lock().unwrap().is_empty());
}

// ---- the folder that is not a folder ------------------------------------------------

const NOT_REAL: &str = "Not a real folder (files no folder lists)";

#[test]
fn a_made_up_folder_disables_every_action_with_the_reason() {
    let mut r = rig(odd_tree());
    let lost = named(&r.shell, "lost");
    show_card(&mut r.shell, &lost, 100);
    let texts = card_texts(&r.shell);
    let all = texts.join("\n");
    assert!(all.contains(NOT_REAL), "{all}");
    assert!(
        all.contains("Not a real folder: it gathers files that no folder lists"),
        "{all}"
    );
    for (label, enabled) in card_buttons(&r.shell) {
        assert!(!enabled, "{label} must be disabled");
    }
    // The reason is stated once for the buttons that share it.
    assert!(
        texts
            .iter()
            .any(|t| t.ends_with(&format!("unavailable: {NOT_REAL}"))),
        "{texts:?}"
    );
}

#[test]
fn nothing_can_be_done_by_name_to_a_made_up_folder_from_the_keyboard_or_menu() {
    let mut r = rig(odd_tree());
    let lost = named(&r.shell, "lost");
    let (x, y) = title_point(lost.rect);
    r.shell.update(InputEvent::Click { x, y }, t(10));
    for key in [
        Key::Delete,
        Key::DeletePermanently,
        Key::CopyPath,
        Key::Rescan,
    ] {
        let fx = r.shell.update(InputEvent::Key(key), t(20));
        assert!(
            !fx.iter().any(|e| matches!(
                e,
                ShellEffect::Run(_) | ShellEffect::Action { .. } | ShellEffect::CopyText(_)
            )),
            "{key:?}: {fx:?}"
        );
        let info = r.shell.info_line().name;
        assert!(info.ends_with(NOT_REAL), "{key:?}: {info}");
    }
    // The menu opens, with every row that acts on disk disabled and saying why.
    r.shell.update(InputEvent::RightClick { x, y }, t(30));
    let rows = r.shell.menu_items();
    for label in [
        "Open",
        "Show in file manager",
        "Copy path",
        "Rescan this folder",
        "Delete",
        "Delete permanently",
    ] {
        let row = rows
            .iter()
            .find(|r| r.0 == label)
            .unwrap_or_else(|| panic!("{label}"));
        assert_eq!((row.1.as_str(), row.2), (NOT_REAL, false), "{label}");
    }
}

#[test]
fn what_is_below_a_made_up_folder_has_no_real_path_to_act_on() {
    let r = rig(odd_tree());
    let app = r.shell.app().unwrap();
    let view = app.current_view();
    let lost = named(&r.shell, "lost");
    assert!(app.source().target(view, lost.id).is_none());
    // Real neighbours still have one.
    let real = named(&r.shell, "real");
    assert!(app.source().target(view, real.id).is_some());
}

// ---- sharing ------------------------------------------------------------------------

#[test]
fn a_file_that_shares_data_says_how_little_deleting_it_frees() {
    let mut r = rig(odd_tree());
    let vm = named(&r.shell, "vm.img");
    show_card(&mut r.shell, &vm, 100);
    let texts = card_texts(&r.shell);
    assert!(
        texts.contains(
            &"Shares data with other files or snapshots · deleting frees about 1.0 MB".into()
        ),
        "{texts:?}"
    );
}

#[test]
fn a_folder_with_a_shared_file_inside_says_at_least_what_it_frees() {
    let mut r = rig(odd_tree());
    let snap = named(&r.shell, "snap");
    show_card(&mut r.shell, &snap, 100);
    let texts = card_texts(&r.shell).join("\n");
    assert!(
        texts.contains("Some files below share data with other files, names or snapshots · deleting frees at least 3.0 MB"),
        "{texts}"
    );
}

#[test]
fn a_folder_with_nothing_shared_says_nothing_about_sharing() {
    let mut r = rig(odd_tree());
    let real = named(&r.shell, "real");
    show_card(&mut r.shell, &real, 100);
    let texts = card_texts(&r.shell).join("\n");
    assert!(!texts.contains("share"), "{texts}");
}

#[test]
fn a_hard_linked_file_keeps_its_own_caveat_and_adds_no_second_one() {
    let mut r = rig(odd_tree());
    let one = named(&r.shell, "one");
    show_card(&mut r.shell, &one, 100);
    let texts = card_texts(&r.shell).join("\n");
    assert!(
        texts.contains("Another name of this file shares its data"),
        "{texts}"
    );
    assert!(!texts.contains("deleting frees"), "{texts}");
}

#[test]
fn sharing_is_looked_up_for_the_card_and_not_for_every_box() {
    // The layout and the info bar never ask: only a card does.
    struct Counting {
        inner: TreeSource,
        asked: Arc<Mutex<usize>>,
    }
    impl ViewSource for Counting {
        fn root(&self) -> u64 {
            self.inner.root()
        }
        fn layout(&mut self, view: u64, map: Rect) -> bs_ui::Layout {
            self.inner.layout(view, map)
        }
        fn parent(&self, view: u64) -> Option<u64> {
            self.inner.parent(view)
        }
        fn card(&self, view: u64, id: u32) -> Option<bs_ui::CardData> {
            self.inner.card(view, id)
        }
        fn sharing(&self, view: u64, id: u32) -> Option<bs_ui::card::Sharing> {
            *self.asked.lock().unwrap() += 1;
            self.inner.sharing(view, id)
        }
    }
    let asked = Arc::new(Mutex::new(0));
    let mut shell: Shell<Counting> = Shell::new(
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
    let source = TreeSource::new(odd_tree(), Metrics::from_logical(13.0, 1.0)).unwrap();
    shell.set_app(App::new(
        Counting {
            inner: source,
            asked: asked.clone(),
        },
        map,
    ));
    let boxes = shell.app().unwrap().items().len();
    assert!(boxes > 5);
    assert_eq!(*asked.lock().unwrap(), 0, "laying out asks nothing");
    let vm = shell
        .app()
        .unwrap()
        .items()
        .iter()
        .find(|i| i.label.as_deref().is_some_and(|l| &*l.name == "vm.img"))
        .cloned()
        .unwrap();
    let p = inside(vm.rect);
    shell.update(InputEvent::PointerMoved { x: p.0, y: p.1 }, t(100));
    assert_eq!(*asked.lock().unwrap(), 0, "hovering asks nothing");
    shell.tick(t(700));
    assert_eq!(*asked.lock().unwrap(), 1, "the card asks once");
    // Acting on the box, and the rows of its menu, check each action against
    // the card's facts, but never ask what a delete would free.
    shell.update(InputEvent::Click { x: p.0, y: p.1 }, t(800));
    shell.update(InputEvent::Key(Key::CopyPath), t(810));
    shell.update(InputEvent::Key(Key::ContextMenu), t(820));
    assert!(shell.modal_open());
    assert_eq!(*asked.lock().unwrap(), 1, "actions and menus ask nothing");
}

// ---- words -----------------------------------------------------------------------------

#[test]
fn the_new_words_avoid_the_banned_ones() {
    let mut r = rig(plain_tree());
    r.shell.set_software_center(true);
    r.owners.set(
        "/data/folder1",
        package(true, Some("sudo apt remove foo"), 1),
    );
    r.owners.set("/data/folder2", program(false, false));
    let mut all = String::new();
    for name in ["folder1", "folder2"] {
        let item = named(&r.shell, name);
        show_card(&mut r.shell, &item, 100);
        all.push_str(&card_texts(&r.shell).join("\n"));
        all.push('\n');
        r.shell.update(InputEvent::PointerLeft, t(2000));
    }
    all.push_str(
        "Not a real folder (files no folder lists)\nNo installed program lists this folder\n",
    );
    let lower = all.to_lowercase();
    for banned in ["safe", "junk", "reclaim", "clean up", "freed"] {
        assert!(!lower.contains(banned), "{banned} in:\n{all}");
    }
}
