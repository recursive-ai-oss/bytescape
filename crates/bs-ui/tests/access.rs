//! The accessibility tree for a known layout, checked without a screen reader:
//! every update is applied to an independent consumer of the tree, which also
//! rejects updates that would break it (a node without a parent, a child
//! listed twice).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use accesskit::{Action, ActionRequest, NodeId, Role, TreeId};
use accesskit_consumer::{NodeRef, Tree, TreeChangeHandler};
use bs_layout::Metrics;
use bs_platform::drives::{Capacity, Drive, DriveId, DriveKind, ReservedKind};
use bs_render::{Rect, TextEngine};
use bs_tree::{Aggregator, Batch, Category, DirKey, Meta, NodeKind, Snapshot};
use bs_ui::access::{AccessTarget, AccessTree, MAP, ROOT, clean_name};
use bs_ui::capacity::Breakdown;
use bs_ui::chrome::TextMeasure;
use bs_ui::picker::Picker;
use bs_ui::shell::{Look, STOPPED_BANNER, ScanReadout, Shell};
use bs_ui::tree::TreeSource;
use bs_ui::{App, Direction, InputEvent, Key, ViewSource};

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

fn meta(size: u64) -> Meta {
    Meta {
        logical: size,
        allocated: size,
        category: Category(1),
        ..Meta::default()
    }
}

fn snap_with(finish: bool, names: &[&[u8]]) -> Arc<Snapshot> {
    let mut agg = Aggregator::new();
    let mut b = Batch::new();
    b.dir(DirKey(0), DirKey::NONE, b"/data", &meta(4096))
        .unwrap();
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
        if d == 1 {
            for (i, n) in names.iter().enumerate() {
                b.file(
                    NodeKind::File,
                    DirKey(d),
                    n,
                    &meta(2_000_000_000 + i as u64),
                )
                .unwrap();
            }
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

fn map_shell_with(finish: bool, names: &[&[u8]]) -> Shell<TreeSource> {
    let mut shell = new_shell();
    let map = shell.map_rect();
    let source =
        TreeSource::new(snap_with(finish, names), Metrics::from_logical(13.0, 1.0)).expect("root");
    shell.set_app(App::new(source, map));
    shell
}

fn map_shell(finish: bool) -> Shell<TreeSource> {
    map_shell_with(finish, &[])
}

/// Counts what changed between two updates, as a screen reader would hear it.
#[derive(Default)]
struct Changes {
    added: usize,
    updated: usize,
    removed: usize,
    focus: Option<u64>,
}

impl TreeChangeHandler for Changes {
    fn node_added(&mut self, _: &NodeRef) {
        self.added += 1;
    }
    fn node_updated(&mut self, _: &NodeRef, _: &NodeRef) {
        self.updated += 1;
    }
    fn focus_moved(&mut self, _: Option<&NodeRef>, new: Option<&NodeRef>) {
        self.focus = new.map(|n| local(n).0);
    }
    fn node_removed(&mut self, _: &NodeRef) {
        self.removed += 1;
    }
}

/// A shell, its tree state and an independent consumer kept in step.
struct Rig {
    shell: Shell<TreeSource>,
    tree: AccessTree,
    consumer: Tree,
    last: Changes,
    last_nodes: usize,
}

impl Rig {
    fn new(shell: Shell<TreeSource>) -> Rig {
        let mut shell = shell;
        let mut tree = AccessTree::new();
        let first = shell.access_update(&mut tree, t(0));
        assert!(first.tree.is_some(), "the first update carries the tree");
        let consumer = Tree::new(first, true);
        Rig {
            shell,
            tree,
            consumer,
            last: Changes::default(),
            last_nodes: 0,
        }
    }

    fn sync(&mut self, now: u64) {
        let update = self.shell.access_update(&mut self.tree, t(now));
        assert!(update.tree.is_none(), "only the first update carries info");
        self.last_nodes = update.nodes.len();
        let mut ch = Changes::default();
        self.consumer.update_and_process_changes(update, &mut ch);
        self.last = ch;
    }

    fn node(&self, id: NodeId) -> Option<NodeRef<'_>> {
        self.consumer
            .state()
            .node_by_tree_local_id(id, TreeId::ROOT)
    }

    /// Roles, names and states of the whole tree, depth first.
    fn dump(&self) -> Vec<String> {
        let mut out = Vec::new();
        fn walk(n: NodeRef<'_>, depth: usize, out: &mut Vec<String>) {
            out.push(format!(
                "{depth} {:?} label={:?} value={:?} desc={:?} sel={:?} busy={} dis={} state={:?}",
                n.role(),
                n.label(),
                n.value(),
                n.description(),
                n.is_selected(),
                n.is_busy(),
                n.is_disabled(),
                n.data().state_description(),
            ));
            for c in n.children() {
                walk(c, depth + 1, out);
            }
        }
        walk(self.consumer.state().root(), 0, &mut out);
        out
    }

    fn all(&self) -> Vec<NodeRef<'_>> {
        let mut out = Vec::new();
        fn walk<'a>(n: NodeRef<'a>, out: &mut Vec<NodeRef<'a>>) {
            out.push(n);
            for c in n.children() {
                walk(c, out);
            }
        }
        walk(self.consumer.state().root(), &mut out);
        out
    }

    fn by_label(&self, label: &str) -> Option<NodeRef<'_>> {
        self.all()
            .into_iter()
            .find(|n| n.label().as_deref() == Some(label))
    }

    fn by_value(&self, value: &str) -> Option<NodeRef<'_>> {
        self.all()
            .into_iter()
            .find(|n| n.value().as_deref() == Some(value))
    }

    /// A fresh tree of the same shell, for comparing with the incremental one.
    fn fresh_dump(&mut self, now: u64) -> Vec<String> {
        let mut tree = AccessTree::new();
        let update = self.shell.access_update(&mut tree, t(now));
        let consumer = Tree::new(update, true);
        let rig = Rig {
            shell: new_shell(),
            tree,
            consumer,
            last: Changes::default(),
            last_nodes: 0,
        };
        rig.dump()
    }
}

/// The id a node was given in the update.
fn local(n: &NodeRef) -> NodeId {
    NodeId((u128::from(n.id()) >> 64) as u64)
}

fn item_rect(shell: &Shell<TreeSource>, name: &str) -> Rect {
    let app = shell.app().expect("app");
    let view = app.current_view();
    app.items()
        .iter()
        .find(|i| {
            app.source()
                .describe(view, i.id)
                .is_some_and(|d| d.name == name)
        })
        .map(|i| i.rect)
        .expect("item")
}

#[test]
fn the_tree_covers_the_chrome_the_map_and_the_status_bar() {
    let mut shell = map_shell(true);
    shell.set_crumb("/data".into());
    shell.set_capacity(
        Breakdown {
            used: 400,
            recycle_bin: 10,
            unreadable: 0,
            reserved: 40,
            free: 550,
            total: 1000,
            quota: false,
        },
        t(0),
    );
    let rig = Rig::new(shell);
    let root = rig.consumer.state().root();
    assert_eq!(root.role(), Role::Window);
    assert_eq!(local(&root), ROOT);
    assert_eq!(root.label().as_deref(), Some("Bytescape"));

    // Navigation buttons: Back and Forward are disabled and say why.
    let back = rig.by_label("Back").expect("back");
    assert_eq!(back.role(), Role::Button);
    assert!(back.is_disabled());
    assert_eq!(back.description().as_deref(), Some("Nothing to go back to"));
    let fwd = rig.by_label("Forward").expect("forward");
    assert!(fwd.is_disabled());
    let up = rig.by_label("Up one level").expect("up");
    assert!(up.is_disabled());
    assert_eq!(up.description().as_deref(), Some("Already at the top"));
    let refresh = rig.by_label("Refresh").expect("refresh");
    assert!(!refresh.is_disabled());
    assert!(refresh.data().supports_action(Action::Click));
    assert_eq!(
        rig.by_label("Location").expect("crumbs").role(),
        Role::Navigation
    );
    assert!(rig.by_value("/data").is_some(), "{:#?}", rig.dump());

    // The capacity bar is a labelled value.
    let cap = rig.by_label("Drive capacity").expect("capacity");
    assert_eq!(cap.role(), Role::Meter);
    assert_eq!(cap.numeric_value(), Some(45.0));
    assert!(cap.value().expect("value").contains("Free 550 B"));

    // The map: a tree of boxes with their sizes, shares and file counts.
    let map = rig.node(MAP).expect("map");
    assert_eq!(map.role(), Role::Tree);
    assert!(map.is_multiselectable());
    assert_eq!(map.label().as_deref(), Some("Map of /data"));
    let folder = rig
        .all()
        .into_iter()
        .find(|n| n.label().is_some_and(|l| l.starts_with("folder1, ")))
        .expect("folder1");
    assert_eq!(folder.role(), Role::TreeItem);
    assert_eq!(folder.level(), Some(2));
    let desc = folder.description().expect("description");
    assert!(desc.starts_with("Folder · "), "{desc}");
    assert!(desc.contains("% of parent"), "{desc}");
    assert!(desc.contains("20 files"), "{desc}");
    let label = folder.label().unwrap();
    assert!(label.contains(" GB"), "{label}");
    assert!(
        !label.contains('≥'),
        "a finished folder has an exact size: {label}"
    );
    assert!(folder.raw_bounds().is_some());
    // Files sit inside their folder.
    let file = folder
        .children()
        .find(|n| n.label().is_some_and(|l| l.starts_with("file0, ")))
        .expect("file0 inside folder1");
    assert_eq!(file.level(), Some(3));
    assert!(
        file.description().unwrap().ends_with("Video"),
        "{:?}",
        file.description()
    );
    assert!(file.data().supports_action(Action::Click));

    // The status bar, with the link as a link.
    let status = rig.by_label("Status").expect("status");
    assert_eq!(status.role(), Role::Status);
    let link = rig
        .all()
        .into_iter()
        .find(|n| n.role() == Role::Link)
        .expect("link");
    assert!(link.label().unwrap().contains("RecursiveAI.org"));
    assert_eq!(link.url(), Some("https://recursiveai.org/"));
    assert!(rig.by_label("Categories").is_some());
    assert!(rig.by_value("Video").is_some());

    // With nothing selected, focus sits on the map.
    assert_eq!(local(&rig.consumer.state().focus_in_tree()), MAP);
}

#[test]
fn sizes_still_being_counted_carry_the_bound_and_a_busy_state() {
    let mut shell = map_shell(false);
    shell.set_status("Scanning…".into(), true);
    let rig = Rig::new(shell);
    let folder = rig
        .all()
        .into_iter()
        .find(|n| n.label().is_some_and(|l| l.starts_with("folder1, ")))
        .expect("folder1");
    let label = folder.label().unwrap();
    assert!(label.starts_with("folder1, ≥ "), "{label}");
    assert!(folder.is_busy());
    assert!(
        folder.description().unwrap().contains("≥ "),
        "{:?}",
        folder.description()
    );
    assert!(rig.node(MAP).unwrap().is_busy());
}

#[test]
fn keyboard_selection_moves_focus_and_updates_only_the_boxes_involved() {
    let mut rig = Rig::new(map_shell(true));
    rig.sync(1);
    assert_eq!(rig.last_nodes, 0, "nothing changed, nothing is sent");
    assert_eq!(rig.last.added + rig.last.updated + rig.last.removed, 0);

    // Select a box by clicking it, then move with the arrows.
    let r = item_rect(&rig.shell, "folder1");
    let (x, y) = (r.x + r.w / 2.0, r.y + 4.0);
    rig.shell.update(InputEvent::Click { x, y }, t(2));
    rig.sync(2);
    let selected = rig.shell.app().unwrap().selected().expect("selected");
    // The box, and the info bar that now describes it.
    assert_eq!(rig.last_nodes, 2);
    let id = AccessTree::box_node_id(selected);
    assert_eq!(rig.node(id).unwrap().is_selected(), Some(true));
    assert_eq!(rig.last.focus, Some(id.0));

    rig.shell
        .update(InputEvent::Key(Key::Arrow(Direction::Right)), t(3));
    rig.sync(3);
    let moved = rig.shell.app().unwrap().selected().expect("selected");
    assert_ne!(moved, selected, "the arrow moved the selection");
    assert_eq!(rig.last_nodes, 3, "the old box, the new box, the info bar");
    assert_eq!(rig.node(id).unwrap().is_selected(), Some(false));
    let new_id = AccessTree::box_node_id(moved);
    assert_eq!(rig.node(new_id).unwrap().is_selected(), Some(true));
    assert_eq!(rig.last.focus, Some(new_id.0));
}

#[test]
fn incremental_updates_always_equal_a_fresh_tree() {
    let mut rig = Rig::new(map_shell(true));
    let r = item_rect(&rig.shell, "folder2");
    let (x, y) = (r.x + r.w / 2.0, r.y + 4.0);
    let steps: Vec<InputEvent> = vec![
        InputEvent::Click { x, y },
        InputEvent::Key(Key::Enter),
        InputEvent::Key(Key::Arrow(Direction::Down)),
        InputEvent::Key(Key::AltUp),
        InputEvent::Key(Key::AltLeft),
        InputEvent::Key(Key::AltRight),
        InputEvent::Key(Key::Home),
    ];
    for (i, ev) in steps.into_iter().enumerate() {
        let now = 10 + i as u64 * 400;
        rig.shell.update(ev, t(now));
        // A navigation animates; let it finish.
        if let Some(app) = rig.shell.app_mut() {
            app.set_animating(false);
        }
        rig.sync(now);
        let fresh = rig.fresh_dump(now);
        assert_eq!(rig.dump(), fresh, "after step {i}: {ev:?}");
    }
}

#[test]
fn back_and_up_turn_on_after_going_into_a_folder() {
    let mut rig = Rig::new(map_shell(true));
    let r = item_rect(&rig.shell, "folder1");
    let (x, y) = (r.x + r.w / 2.0, r.y + 4.0);
    rig.shell.update(InputEvent::DoubleClick { x, y }, t(5));
    rig.sync(5);
    assert!(!rig.by_label("Back").unwrap().is_disabled());
    assert!(!rig.by_label("Up one level").unwrap().is_disabled());
    assert!(rig.by_label("Forward").unwrap().is_disabled());
    assert_eq!(
        rig.node(MAP)
            .unwrap()
            .label()
            .as_deref()
            .map(|s| s.starts_with("Map of ")),
        Some(true)
    );
}

#[test]
fn the_progress_readout_and_the_stopped_banner_are_announced() {
    let mut shell = map_shell(false);
    shell.set_status("Scanning…".into(), true);
    shell.set_readout(Some(ScanReadout {
        ratio: Some(0.37),
        files: 1_200_000,
        files_per_sec: 120_000.0,
        elapsed: Duration::from_secs(72),
    }));
    let mut rig = Rig::new(shell);
    let group = rig.by_label("Scan progress").expect("progress");
    assert!(group.is_busy());
    assert!(rig.by_value("37% · 120k files/s · 1:12").is_some());
    let cancel = rig.by_label("Cancel").expect("cancel");
    assert_eq!(cancel.role(), Role::Button);

    rig.shell.set_readout(None);
    rig.shell.set_status(STOPPED_BANNER.into(), false);
    rig.shell.set_stopped(true);
    rig.sync(1);
    assert!(rig.by_label("Scan progress").is_none());
    assert!(rig.by_label("Scan stopped").is_some());
    assert!(rig.by_value(STOPPED_BANNER).is_some());
    assert!(rig.by_label("Rescan").is_some());
    assert_eq!(rig.dump(), rig.fresh_dump(1));
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

#[test]
fn the_drive_picker_is_a_list_of_drives_and_a_folder_button() {
    let drives = vec![
        drive(
            "/",
            DriveKind::Fixed,
            1_000_000_000_000,
            400_000_000_000,
            true,
        ),
        drive(
            "/mnt/usb",
            DriveKind::Removable,
            16_000_000_000,
            4_000_000_000,
            false,
        ),
    ];
    let mut shell = new_shell();
    shell.set_picker(Picker::new(&drives));
    let mut rig = Rig::new(shell);
    let list = rig.node(MAP).expect("drives");
    assert_eq!(list.role(), Role::ListBox);
    assert_eq!(list.label().as_deref(), Some("Drives"));
    let options: Vec<_> = list.children().collect();
    assert_eq!(options.len(), 2);
    assert_eq!(options[0].role(), Role::ListBoxOption);
    assert_eq!(options[0].label().as_deref(), Some("/"));
    let d = options[0].description().unwrap();
    assert!(d.contains("400.0 GB of 1.0 TB used"), "{d}");
    assert!(d.contains("Scanning…"), "{d}");
    assert!(options[0].is_busy());
    assert!(
        options[1]
            .description()
            .unwrap()
            .contains("Not scanned until you pick it")
    );
    assert!(!options[1].is_busy());
    assert!(rig.by_label("Scan a folder…").is_some());

    // Keyboard selection moves the focus to the card.
    rig.shell
        .update(InputEvent::Key(Key::Arrow(Direction::Right)), t(1));
    rig.sync(1);
    let focused = rig.consumer.state().focus_in_tree();
    assert_eq!(focused.role(), Role::ListBoxOption);
    assert_eq!(focused.is_selected(), Some(true));
    assert_eq!(rig.dump(), rig.fresh_dump(1));
}

#[test]
fn a_click_asked_for_by_a_screen_reader_acts_like_a_pointer_click() {
    let mut rig = Rig::new(map_shell(true));
    // A box: selecting it.
    let r = item_rect(&rig.shell, "folder3");
    let _ = r;
    let target = rig
        .all()
        .into_iter()
        .find(|n| n.label().is_some_and(|l| l.starts_with("folder3, ")))
        .map(|n| local(&n))
        .expect("folder3");
    let req = ActionRequest {
        action: Action::Click,
        target_tree: TreeId::ROOT,
        target_node: target,
        data: None,
    };
    assert!(matches!(
        rig.tree.target(target),
        Some(AccessTarget::Box(_))
    ));
    let fx = rig.shell.access_action(&rig.tree, &req, t(1));
    drop(fx);
    let selected = rig.shell.app().unwrap().selected().expect("selected");
    assert_eq!(AccessTree::box_node_id(selected), target);

    // The link.
    let link = rig
        .all()
        .into_iter()
        .find(|n| n.role() == Role::Link)
        .map(|n| local(&n))
        .unwrap();
    let fx = rig.shell.access_action(
        &rig.tree,
        &ActionRequest {
            action: Action::Click,
            target_tree: TreeId::ROOT,
            target_node: link,
            data: None,
        },
        t(2),
    );
    assert!(
        fx.iter()
            .any(|e| matches!(e, bs_ui::shell::ShellEffect::OpenLink(_))),
        "{fx:?}"
    );

    // A disabled button does nothing.
    let back = local(&rig.by_label("Back").unwrap());
    assert_eq!(rig.tree.target(back), None);
    let fx = rig.shell.access_action(
        &rig.tree,
        &ActionRequest {
            action: Action::Click,
            target_tree: TreeId::ROOT,
            target_node: back,
            data: None,
        },
        t(3),
    );
    assert!(fx.is_empty());
}

#[test]
fn hostile_names_are_cleaned_and_shortened_by_characters() {
    let long = "é".repeat(4000);
    let weird = "a\u{202e}b\u{200b}c\u{0301}\nline\u{0}";
    let names: Vec<&[u8]> = vec![
        long.as_bytes(),
        weird.as_bytes(),
        b"bad\xff\xfeutf8",
        "שלום עולם.txt".as_bytes(),
    ];
    let rig = Rig::new(map_shell_with(true, &names));
    for n in rig.all() {
        for s in [n.label(), n.value(), n.description()]
            .into_iter()
            .flatten()
        {
            assert!(
                !s.chars().any(char::is_control),
                "control characters in {s:?}"
            );
            assert!(s.chars().count() < 400, "{} chars", s.chars().count());
        }
    }
    assert!(clean_name(&long).contains('…'));
    assert_eq!(clean_name("plain.txt"), "plain.txt");
    assert_eq!(clean_name("a\nb"), "a b");
    let cut = clean_name(&"日".repeat(1000));
    assert_eq!(cut.chars().count(), 160);
    assert!(
        rig.all()
            .iter()
            .any(|n| n.label().is_some_and(|l| l.contains("שלום")))
    );
}

#[test]
fn the_words_in_the_tree_follow_the_wording_rules() {
    let rig = Rig::new(map_shell(false));
    let banned = ["safe", "junk", "reclaim", "clean up", "freed"];
    for n in rig.all() {
        for s in [
            n.label(),
            n.value(),
            n.description(),
            n.data().state_description().map(String::from),
        ]
        .into_iter()
        .flatten()
        {
            let low = s.to_lowercase();
            for b in banned {
                assert!(!low.contains(b), "{b:?} in {s:?}");
            }
        }
    }
}

/// A single folder with `n` files, so a view holds thousands of boxes.
fn big_shell(n: u32) -> Shell<TreeSource> {
    let mut agg = Aggregator::new();
    let mut b = Batch::new();
    b.dir(DirKey(0), DirKey::NONE, b"/data", &meta(4096))
        .unwrap();
    for f in 0..n {
        b.file(
            NodeKind::File,
            DirKey(0),
            format!("file{f}").as_bytes(),
            &meta(1_000_000 + u64::from(f) * 977),
        )
        .unwrap();
    }
    b.listing_done(DirKey(0));
    agg.apply(&b);
    let mut shell = Shell::new(
        (3840, 2160),
        1.0,
        13.0,
        TextEngine::new(),
        Look {
            dark: false,
            accent: None,
        },
    )
    .with_measure(Box::new(Fixed(13.0 * 0.56)));
    let map = shell.map_rect();
    let source = TreeSource::new(agg.publish(), Metrics::from_logical(13.0, 1.0)).expect("root");
    shell.set_app(App::new(source, map));
    shell
}

#[test]
fn two_thousand_boxes_update_in_small_steps() {
    let shell = big_shell(2400);
    let boxes = shell.app().unwrap().items().len();
    assert!(boxes > 1500, "{boxes} boxes");
    let mut rig = Rig::new(shell);

    // Idle: nothing is rebuilt or sent.
    let start = Instant::now();
    for i in 0..200 {
        rig.sync(i);
        assert_eq!(rig.last_nodes, 0);
    }
    let idle = start.elapsed() / 200;

    // Selection changes touch the two boxes and the info bar.
    let start = Instant::now();
    let mut steps = 0;
    for i in 0..40u64 {
        rig.shell
            .update(InputEvent::Key(Key::Arrow(Direction::Right)), t(1000 + i));
        rig.sync(1000 + i);
        assert!(rig.last_nodes <= 3, "{} nodes", rig.last_nodes);
        steps += 1;
    }
    let select = start.elapsed() / steps;

    // A new layout of the same boxes is described again once.
    let mut tree = AccessTree::new();
    let start = Instant::now();
    let full = rig.shell.access_update(&mut tree, t(0));
    let build = start.elapsed();
    assert!(full.nodes.len() > boxes, "{} nodes", full.nodes.len());
    eprintln!(
        "access tree, {boxes} boxes: full build {build:?}, idle update {idle:?}, selection update {select:?}"
    );
    assert_eq!(tree.node_count(), full.nodes.len());
}
