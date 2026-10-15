//! Delete flows, Undo, the bin, the context menu, dialogs and the toast, driven
//! through the shell with real layouts and a scripted stand-in for the
//! platform. Every test asserts the words the user would read. Time is part of
//! the events, so nothing here sleeps.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bs_knowledge::{Level, Reason};
use bs_layout::Metrics;
use bs_platform::delete::{
    ByteBasis, Decision, EmptyOutcome, Item as DItem, OpError, PermanentReason, Plan, PlanEntry,
    ProtectedConfirmation, RefuseReason, RestoreOutcome,
};
use bs_render::{Item, ItemKind, Rect, TextEngine};
use bs_tree::{
    Aggregator, Batch, Category, DeleteState, DirKey, Meta, NodeFlags, NodeKind, ReadErrorKind,
    ReadFailure, Snapshot,
};
use bs_ui::capacity::Breakdown;
use bs_ui::chrome::TextMeasure;
use bs_ui::shell::{
    ActOutcome, BinDrive, BinStyle, Capabilities, Disposer, Look, Shell, ShellEffect, Token,
    TreeChange,
};
use bs_ui::tree::TreeSource;
use bs_ui::{App, InputEvent, Key, Modifiers, ViewSource};

// ---- fixture -----------------------------------------------------------------

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

fn names_for(n: usize) -> Vec<Vec<u8>> {
    (1..=n).map(|i| format!("folder{i}").into_bytes()).collect()
}

fn build(names: &[Vec<u8>], finish: bool) -> (Aggregator, Arc<Snapshot>) {
    let mut agg = Aggregator::new();
    let mut b = Batch::new();
    b.dir(DirKey(0), DirKey::NONE, b"/data", &meta(4096))
        .unwrap();
    for (i, name) in names.iter().enumerate() {
        let key = DirKey(i as u32 + 1);
        b.dir(key, DirKey(0), name, &meta(4096)).unwrap();
        for f in 0..20u32 {
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
    let snap = agg.publish();
    (agg, snap)
}

const SIZE: (u32, u32) = (1200, 800);

fn t(ms: u64) -> Duration {
    Duration::from_millis(ms)
}

fn center(r: Rect) -> (f32, f32) {
    (r.x + r.w / 2.0, r.y + r.h / 2.0)
}

fn title_point(r: Rect) -> (f32, f32) {
    (r.x + r.w / 2.0, r.y + 3.0)
}

// ---- the scripted platform ----------------------------------------------------

#[derive(Clone)]
enum Rule {
    Protected(&'static str),
    Blocked,
    Overflow(PermanentReason),
    Changed,
}

#[derive(Clone, Copy, PartialEq)]
enum TrashMode {
    Moves,
    SystemDeletes,
    FailsDenied,
    LogicalBytes,
}

#[derive(Clone, Copy, PartialEq)]
enum RestoreMode {
    Restores,
    NotInBin,
}

struct Fake {
    rules: Mutex<HashMap<String, Rule>>,
    calls: Mutex<Vec<String>>,
    confirmed: Mutex<Vec<Vec<PathBuf>>>,
    per_item: Mutex<u64>,
    /// How long a precheck takes, like a folder walk on a slow disk.
    precheck_delay: Mutex<Duration>,
    precheck_calls: Mutex<usize>,
    /// The identity each precheck was given for each item.
    seen_log: Mutex<Vec<Vec<Option<bs_platform::delete::Identity>>>>,
    trash_mode: Mutex<TrashMode>,
    restore_mode: Mutex<RestoreMode>,
    empty: Mutex<EmptyOutcome>,
    shared: Mutex<u64>,
}

impl Fake {
    fn new() -> Arc<Fake> {
        Arc::new(Fake {
            rules: Mutex::new(HashMap::new()),
            calls: Mutex::new(Vec::new()),
            confirmed: Mutex::new(Vec::new()),
            per_item: Mutex::new(12_300_000_000),
            precheck_delay: Mutex::new(Duration::ZERO),
            precheck_calls: Mutex::new(0),
            seen_log: Mutex::new(Vec::new()),
            trash_mode: Mutex::new(TrashMode::Moves),
            restore_mode: Mutex::new(RestoreMode::Restores),
            empty: Mutex::new(EmptyOutcome::Emptied {
                bytes: 14_100_000_000,
                entries: 3,
            }),
            shared: Mutex::new(0),
        })
    }

    fn rule(&self, name: &str, rule: Rule) {
        self.rules.lock().unwrap().insert(name.to_string(), rule);
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    fn name(path: &Path) -> String {
        path.file_name()
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
        let delay = *self.precheck_delay.lock().unwrap();
        if !delay.is_zero() {
            std::thread::sleep(delay);
        }
        *self.precheck_calls.lock().unwrap() += 1;
        self.seen_log
            .lock()
            .unwrap()
            .push(items.iter().map(|i| i.seen).collect());
        let rules = self.rules.lock().unwrap();
        let mut entries = Vec::new();
        for item in items {
            let rule = rules.get(&Self::name(&item.path)).cloned();
            let entry = match rule {
                None => PlanEntry {
                    level: Level::Normal,
                    reason: Reason::None,
                    note: None,
                    decision: Decision::Proceed,
                },
                Some(Rule::Blocked) => PlanEntry {
                    level: Level::Blocked,
                    reason: Reason::Table,
                    note: Some("Part of Windows."),
                    decision: Decision::Refused(RefuseReason::Blocked {
                        reason: Reason::Table,
                        note: Some("Part of Windows."),
                    }),
                },
                Some(Rule::Protected(note)) => PlanEntry {
                    level: Level::Protected,
                    reason: Reason::Table,
                    note: Some(note),
                    decision: if confirmed.is_some() {
                        Decision::Proceed
                    } else {
                        Decision::Refused(RefuseReason::ProtectedNotConfirmed { note: Some(note) })
                    },
                },
                Some(Rule::Overflow(why)) => PlanEntry {
                    level: Level::Normal,
                    reason: Reason::None,
                    note: None,
                    decision: if permanent {
                        Decision::Proceed
                    } else {
                        Decision::WouldBePermanent(why)
                    },
                },
                Some(Rule::Changed) => PlanEntry {
                    level: Level::Normal,
                    reason: Reason::None,
                    note: None,
                    decision: Decision::Refused(RefuseReason::ChangedSinceScan),
                },
            };
            entries.push(entry);
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
            selection_refused: entries
                .iter()
                .any(|e| matches!(e.decision, Decision::Refused(_))),
            entries,
            strictest,
            drivers,
        }
    }

    fn trash(&self, items: &[DItem], confirmed: Option<&ProtectedConfirmation>) -> Vec<ActOutcome> {
        self.calls.lock().unwrap().push(format!(
            "trash:{}{}",
            items
                .iter()
                .map(|i| Self::name(&i.path))
                .collect::<Vec<_>>()
                .join(","),
            if confirmed.is_some() {
                ":confirmed"
            } else {
                ""
            }
        ));
        self.confirmed
            .lock()
            .unwrap()
            .push(items.iter().map(|i| i.path.clone()).collect());
        let mode = *self.trash_mode.lock().unwrap();
        items
            .iter()
            .map(|i| match mode {
                TrashMode::Moves | TrashMode::LogicalBytes => ActOutcome::MovedToBin {
                    token: Token::new(i.path.clone()),
                    bytes: *self.per_item.lock().unwrap(),
                    basis: if mode == TrashMode::LogicalBytes {
                        ByteBasis::Logical
                    } else {
                        ByteBasis::Allocated
                    },
                    shared_files: *self.shared.lock().unwrap(),
                },
                TrashMode::SystemDeletes => ActOutcome::Deleted {
                    bytes: *self.per_item.lock().unwrap(),
                    shared_files: 0,
                },
                TrashMode::FailsDenied => ActOutcome::Failed(OpError {
                    step: "move",
                    kind: std::io::ErrorKind::PermissionDenied,
                    code: Some(13),
                    removed_bytes: 0,
                    removed_entries: 0,
                }),
            })
            .collect()
    }

    fn delete_permanently(
        &self,
        items: &[DItem],
        confirmed: Option<&ProtectedConfirmation>,
    ) -> Vec<ActOutcome> {
        self.calls.lock().unwrap().push(format!(
            "perm:{}{}",
            items
                .iter()
                .map(|i| Self::name(&i.path))
                .collect::<Vec<_>>()
                .join(","),
            if confirmed.is_some() {
                ":confirmed"
            } else {
                ""
            }
        ));
        items
            .iter()
            .map(|_| ActOutcome::Deleted {
                bytes: *self.per_item.lock().unwrap(),
                shared_files: *self.shared.lock().unwrap(),
            })
            .collect()
    }

    fn restore(&self, token: &Token) -> RestoreOutcome {
        let path = token.get::<PathBuf>().cloned().unwrap_or_default();
        self.calls
            .lock()
            .unwrap()
            .push(format!("restore:{}", Self::name(&path)));
        match *self.restore_mode.lock().unwrap() {
            RestoreMode::Restores => RestoreOutcome::Restored { path },
            RestoreMode::NotInBin => RestoreOutcome::Refused {
                reason: RefuseReason::NotInBin,
            },
        }
    }

    fn empty_bin(&self, mount: &Path) -> EmptyOutcome {
        self.calls
            .lock()
            .unwrap()
            .push(format!("empty:{}", mount.display()));
        self.empty.lock().unwrap().clone()
    }
}

struct Rig {
    shell: Shell<TreeSource>,
    fake: Arc<Fake>,
    agg: Aggregator,
}

fn rig_with(names: Vec<Vec<u8>>, finish: bool, style: BinStyle, scale: f32) -> Rig {
    let (agg, snapshot) = build(&names, finish);
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
    let fake = Fake::new();
    shell.set_disposer(fake.clone());
    shell.set_bin_style(style);
    shell.set_capabilities(Capabilities { properties: true });
    Rig { shell, fake, agg }
}

fn rig(style: BinStyle) -> Rig {
    rig_with(names_for(3), true, style, 1.0)
}

fn folders(shell: &Shell<TreeSource>) -> Vec<Item> {
    let mut v: Vec<Item> = shell
        .app()
        .expect("app")
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

/// Answers the platform's questions (the checks a delete starts) at once, as
/// a host with an idle worker thread would, and returns what is left.
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

fn click(shell: &mut Shell<TreeSource>, p: (f32, f32), at: u64) -> Vec<ShellEffect> {
    let fx = shell.update(InputEvent::Click { x: p.0, y: p.1 }, t(at));
    settle(shell, fx, at)
}

fn ctrl_click(shell: &mut Shell<TreeSource>, p: (f32, f32), at: u64) -> Vec<ShellEffect> {
    shell.update(
        InputEvent::ClickWith {
            x: p.0,
            y: p.1,
            mods: Modifiers {
                ctrl: true,
                shift: false,
            },
        },
        t(at),
    )
}

/// Selects the folders at `indexes` (left to right in the layout).
fn select(shell: &mut Shell<TreeSource>, indexes: &[usize]) -> Vec<Item> {
    let all = folders(shell);
    for (n, &i) in indexes.iter().enumerate() {
        let p = title_point(all[i].rect);
        if n == 0 {
            click(shell, p, 10);
        } else {
            ctrl_click(shell, p, 10 + n as u64);
        }
    }
    all
}

fn key(shell: &mut Shell<TreeSource>, k: Key, at: u64) -> Vec<ShellEffect> {
    let fx = shell.update(InputEvent::Key(k), t(at));
    settle(shell, fx, at)
}

fn info(shell: &Shell<TreeSource>) -> String {
    let l = shell.info_line();
    format!("{}{}", l.name, l.details)
}

/// Runs the first disk job among `fx` and gives its result back to the shell.
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

fn bytes_of(shell: &Shell<TreeSource>, indexes: &[usize]) -> u64 {
    let app = shell.app().unwrap();
    let all = folders(shell);
    indexes
        .iter()
        .map(|&i| {
            app.source()
                .target(app.current_view(), all[i].id)
                .unwrap()
                .bytes
        })
        .sum()
}

fn button_center(shell: &Shell<TreeSource>, label: &str) -> (f32, f32) {
    let popup = shell.popup().expect("a popup");
    for part in &popup.card.parts {
        match part {
            bs_render::CardPart::Button { rect, label: l, .. }
            | bs_render::CardPart::Hold { rect, label: l, .. }
            | bs_render::CardPart::Row { rect, label: l, .. }
                if &**l == label =>
            {
                return center(*rect);
            }
            _ => {}
        }
    }
    panic!("no control called {label}");
}

fn toast_button_center(shell: &Shell<TreeSource>, at: u64) -> (f32, f32) {
    let card = shell.toast_card(t(at)).expect("a toast");
    for part in &card.parts {
        if let bs_render::CardPart::Button { rect, .. } = part {
            return center(*rect);
        }
    }
    panic!("the toast has no button");
}

const BIN: &str = "Recycle Bin";

// ---- 1. recycling -------------------------------------------------------------

#[test]
fn delete_moves_to_the_bin_without_a_dialog_and_the_toast_offers_undo() {
    for (style, name) in [
        (BinStyle::Recycle, "Recycle Bin"),
        (BinStyle::Trash, "Trash"),
    ] {
        let mut r = rig(style);
        select(&mut r.shell, &[0]);
        let fx = key(&mut r.shell, Key::Delete, 20);
        assert!(!r.shell.modal_open(), "no dialog for a plain delete");
        assert!(has_job(&fx), "{fx:?}");
        assert!(
            r.shell.toast_text().is_none(),
            "nothing is claimed before it happened"
        );
        let fx = run_job(&mut r.shell, fx, 30);
        assert_eq!(
            r.shell.toast_text().as_deref(),
            Some(format!("Moved to {name} · 12.3 GB · Undo").as_str())
        );
        let changes = tree_changes(&fx);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].to, DeleteState::InBin);
        assert!(fx.iter().any(|e| matches!(e, ShellEffect::BinChanged)));
        assert_eq!(r.shell.undo_depth(), 1);
        assert_eq!(r.fake.calls(), vec!["trash:folder1".to_string()]);
    }
}

#[test]
fn nothing_says_freed_and_a_system_delete_is_never_called_a_move() {
    let mut r = rig(BinStyle::Recycle);
    *r.fake.trash_mode.lock().unwrap() = TrashMode::SystemDeletes;
    select(&mut r.shell, &[0]);
    let fx = key(&mut r.shell, Key::Delete, 20);
    let fx = run_job(&mut r.shell, fx, 30);
    let text = r.shell.toast_text().unwrap();
    assert!(!text.contains("Moved"), "{text}");
    assert!(text.starts_with("Deleted · 12.3 GB"), "{text}");
    assert!(
        text.contains("couldn't be moved to the Recycle Bin"),
        "{text}"
    );
    assert!(!text.to_lowercase().contains("freed"));
    assert_eq!(tree_changes(&fx)[0].to, DeleteState::Deleted);
    assert_eq!(r.shell.undo_depth(), 0, "nothing in the bin to put back");
    assert!(!text.contains("Undo"));
}

#[test]
fn bytes_in_a_windows_bin_are_labelled_as_file_sizes() {
    let mut r = rig(BinStyle::Recycle);
    *r.fake.trash_mode.lock().unwrap() = TrashMode::LogicalBytes;
    select(&mut r.shell, &[0]);
    let fx = key(&mut r.shell, Key::Delete, 20);
    run_job(&mut r.shell, fx, 30);
    assert_eq!(
        r.shell.toast_text().as_deref(),
        Some("Moved to Recycle Bin · 12.3 GB of files · Undo")
    );
}

#[test]
fn failures_are_reported_plainly_and_nothing_is_marked() {
    let mut r = rig(BinStyle::Trash);
    *r.fake.trash_mode.lock().unwrap() = TrashMode::FailsDenied;
    select(&mut r.shell, &[0]);
    let fx = key(&mut r.shell, Key::Delete, 20);
    let fx = run_job(&mut r.shell, fx, 30);
    let text = r.shell.toast_text().unwrap();
    assert!(
        text.contains("Couldn't move to the Trash “folder1”"),
        "{text}"
    );
    assert!(text.contains("permission denied"), "{text}");
    assert!(!text.contains("Moved to"));
    assert!(tree_changes(&fx).is_empty());
    assert_eq!(r.shell.undo_depth(), 0);
}

#[test]
fn a_hard_linked_file_is_said_to_keep_its_data() {
    let mut r = rig(BinStyle::Trash);
    *r.fake.shared.lock().unwrap() = 2;
    select(&mut r.shell, &[0]);
    let fx = key(&mut r.shell, Key::Delete, 20);
    run_job(&mut r.shell, fx, 30);
    let text = r.shell.toast_text().unwrap();
    assert!(
        text.contains("2 files also have another name, so their data stays on the drive."),
        "{text}"
    );
}

// ---- 2. the toast ---------------------------------------------------------------

#[test]
fn the_toast_stays_about_eight_seconds_and_goes_with_a_short_slide() {
    let mut r = rig(BinStyle::Trash);
    select(&mut r.shell, &[0]);
    let fx = key(&mut r.shell, Key::Delete, 20);
    run_job(&mut r.shell, fx, 100);
    r.shell.tick(t(4_000));
    assert!(r.shell.toast_text().is_some());
    r.shell.tick(t(7_900));
    assert!(r.shell.toast_text().is_some());
    r.shell.tick(t(8_200));
    // It is sliding out.
    assert!(r.shell.toast_text().is_some());
    assert!(r.shell.toast_card(t(8_200)).is_some());
    r.shell.tick(t(8_400));
    assert!(r.shell.toast_text().is_none());
}

#[test]
fn the_toast_stays_while_the_pointer_is_on_it() {
    let mut r = rig(BinStyle::Trash);
    select(&mut r.shell, &[0]);
    let fx = key(&mut r.shell, Key::Delete, 20);
    run_job(&mut r.shell, fx, 100);
    r.shell.tick(t(1_000));
    let card = r.shell.toast_card(t(1_000)).unwrap();
    let on = (card.rect.x + 3.0, card.rect.y + 3.0);
    r.shell
        .update(InputEvent::PointerMoved { x: on.0, y: on.1 }, t(1_000));
    r.shell.tick(t(20_000));
    r.shell.tick(t(30_000));
    assert!(r.shell.toast_text().is_some(), "it persists while hovered");
    // Leaving starts the full time again.
    r.shell
        .update(InputEvent::PointerMoved { x: 5.0, y: 5.0 }, t(30_000));
    r.shell.tick(t(35_000));
    assert!(r.shell.toast_text().is_some());
    r.shell.tick(t(38_200));
    r.shell.tick(t(38_500));
    assert!(r.shell.toast_text().is_none());
}

#[test]
fn only_one_toast_shows_at_a_time() {
    let mut r = rig(BinStyle::Trash);
    select(&mut r.shell, &[0]);
    let fx = key(&mut r.shell, Key::Delete, 20);
    run_job(&mut r.shell, fx, 30);
    let first = r.shell.toast_text().unwrap();
    select(&mut r.shell, &[1]);
    let fx = key(&mut r.shell, Key::Delete, 40);
    run_job(&mut r.shell, fx, 50);
    let second = r.shell.toast_text().unwrap();
    assert_eq!(first, second, "the same words, one toast");
    assert_eq!(r.shell.undo_depth(), 2, "both deletes can still be undone");
}

#[test]
fn reduced_motion_shows_and_removes_the_toast_at_once() {
    let mut r = rig(BinStyle::Trash);
    r.shell.set_reduced_motion(true);
    select(&mut r.shell, &[0]);
    let fx = key(&mut r.shell, Key::Delete, 20);
    run_job(&mut r.shell, fx, 100);
    let in_place = r.shell.toast_card(t(100)).unwrap().rect;
    let later = r.shell.toast_card(t(101)).unwrap().rect;
    assert_eq!(in_place, later, "no slide");
    r.shell.tick(t(8_200));
    assert!(r.shell.toast_text().is_none());
}

// ---- 3. undo --------------------------------------------------------------------

#[test]
fn ctrl_z_puts_the_last_delete_back_and_takes_the_mark_off() {
    let mut r = rig(BinStyle::Trash);
    select(&mut r.shell, &[0]);
    let fx = key(&mut r.shell, Key::Delete, 20);
    let marked = tree_changes(&run_job(&mut r.shell, fx, 30));
    let fx = key(&mut r.shell, Key::Undo, 40);
    assert!(has_job(&fx));
    let fx = run_job(&mut r.shell, fx, 50);
    let changes = tree_changes(&fx);
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].node, marked[0].node);
    assert_eq!(changes[0].to, DeleteState::None);
    assert_eq!(r.shell.toast_text().as_deref(), Some("Put “folder1” back"));
    assert_eq!(r.shell.undo_depth(), 0);
    assert_eq!(
        r.fake.calls(),
        vec!["trash:folder1".to_string(), "restore:folder1".to_string()]
    );
}

#[test]
fn the_undo_button_on_the_toast_does_the_same() {
    let mut r = rig(BinStyle::Trash);
    select(&mut r.shell, &[0]);
    let fx = key(&mut r.shell, Key::Delete, 20);
    run_job(&mut r.shell, fx, 100);
    r.shell.tick(t(300));
    let p = toast_button_center(&r.shell, 300);
    let fx = click(&mut r.shell, p, 300);
    assert!(has_job(&fx), "{fx:?}");
    run_job(&mut r.shell, fx, 310);
    assert!(r.fake.calls().contains(&"restore:folder1".to_string()));
}

#[test]
fn undo_goes_back_one_delete_at_a_time() {
    let mut r = rig(BinStyle::Trash);
    for (i, at) in [(0usize, 20u64), (1, 60)] {
        select(&mut r.shell, &[i]);
        let fx = key(&mut r.shell, Key::Delete, at);
        run_job(&mut r.shell, fx, at + 5);
    }
    let fx = key(&mut r.shell, Key::Undo, 100);
    run_job(&mut r.shell, fx, 110);
    assert_eq!(r.shell.undo_depth(), 1);
    assert_eq!(r.fake.calls().last().unwrap(), "restore:folder2");
}

#[test]
fn undo_says_so_when_the_item_has_left_the_bin_or_nothing_was_deleted() {
    let mut r = rig(BinStyle::Recycle);
    key(&mut r.shell, Key::Undo, 5);
    assert_eq!(info(&r.shell), "Nothing to undo");
    select(&mut r.shell, &[0]);
    let fx = key(&mut r.shell, Key::Delete, 20);
    run_job(&mut r.shell, fx, 30);
    *r.fake.restore_mode.lock().unwrap() = RestoreMode::NotInBin;
    let fx = key(&mut r.shell, Key::Undo, 40);
    let fx = run_job(&mut r.shell, fx, 50);
    assert_eq!(
        r.shell.toast_text().as_deref(),
        Some("Couldn't put “folder1” back: it is no longer in the Recycle Bin.")
    );
    assert!(tree_changes(&fx).is_empty(), "the tile stays marked");
}

// ---- 4. permanent delete and the overflow -------------------------------------------

#[test]
fn shift_delete_asks_once_with_the_count_and_the_size() {
    let mut r = rig(BinStyle::Recycle);
    select(&mut r.shell, &[0, 1, 2]);
    let total = bytes_of(&r.shell, &[0, 1, 2]);
    let fx = key(&mut r.shell, Key::DeletePermanently, 20);
    assert!(!has_job(&fx), "nothing happens before the answer");
    let text = r.shell.dialog_text().expect("a dialog");
    assert_eq!(
        text.split(" • ").next().unwrap(),
        format!(
            "Permanently delete 3 items ({})? This can't be undone.",
            bs_layout::format_size(total)
        )
    );
    assert_eq!(r.shell.dialog_buttons(), ["Delete permanently", "Cancel"]);
    assert!(r.fake.calls().is_empty());
    let p = button_center(&r.shell, "Delete permanently");
    let fx = click(&mut r.shell, p, 30);
    assert!(!r.shell.modal_open());
    let fx = run_job(&mut r.shell, fx, 40);
    assert_eq!(
        r.fake.calls(),
        vec!["perm:folder1,folder2,folder3".to_string()]
    );
    assert_eq!(tree_changes(&fx).len(), 3);
    assert!(
        tree_changes(&fx)
            .iter()
            .all(|c| c.to == DeleteState::Deleted)
    );
    assert!(r.shell.toast_text().unwrap().starts_with("Deleted · "));
    assert_eq!(
        r.shell.undo_depth(),
        0,
        "a permanent delete can't be undone"
    );
}

#[test]
fn a_single_item_reads_in_the_singular() {
    let mut r = rig(BinStyle::Trash);
    select(&mut r.shell, &[0]);
    key(&mut r.shell, Key::DeletePermanently, 20);
    let text = r.shell.dialog_text().unwrap();
    assert!(text.starts_with("Permanently delete 1 item ("), "{text}");
}

#[test]
fn cancelling_a_permanent_delete_does_nothing() {
    for how in 0..3 {
        let mut r = rig(BinStyle::Trash);
        select(&mut r.shell, &[0]);
        key(&mut r.shell, Key::DeletePermanently, 20);
        let fx = match how {
            0 => key(&mut r.shell, Key::Escape, 30),
            1 => {
                let p = button_center(&r.shell, "Cancel");
                click(&mut r.shell, p, 30)
            }
            // Enter on the default button, which is Cancel.
            _ => key(&mut r.shell, Key::Enter, 30),
        };
        assert!(!r.shell.modal_open(), "way {how}");
        assert!(!has_job(&fx));
        assert!(r.fake.calls().is_empty());
    }
}

#[test]
fn a_dialog_takes_every_event_so_the_map_below_does_not_change() {
    let mut r = rig(BinStyle::Trash);
    let all = select(&mut r.shell, &[0]);
    key(&mut r.shell, Key::DeletePermanently, 20);
    let other = center(all[2].rect);
    click(&mut r.shell, other, 30);
    key(&mut r.shell, Key::Delete, 31);
    assert!(r.shell.modal_open());
    assert_eq!(r.shell.app().unwrap().selection().len(), 1);
    assert!(r.fake.calls().is_empty());
}

#[test]
fn a_thing_too_big_for_the_bin_goes_through_the_permanent_dialog_with_a_first_line() {
    let cases = [
        (
            BinStyle::Recycle,
            PermanentReason::TooLargeForBin {
                item_bytes: 12_300_000_000,
                limit_bytes: 4_000_000_000,
            },
            "This is 12.3 GB and the Recycle Bin holds at most 4.0 GB, so it can't be moved there.",
        ),
        (
            BinStyle::Recycle,
            PermanentReason::BinDisabled,
            "This drive is set to delete without using the Recycle Bin.",
        ),
        (
            BinStyle::Trash,
            PermanentReason::NoTrashOnVolume,
            "This drive has no Trash that can be used, so it can't be moved there.",
        ),
        (
            BinStyle::Trash,
            PermanentReason::CrossesFilesystem,
            "Moving this to the Trash would mean copying it to another drive, so it can't be moved there.",
        ),
    ];
    for (style, why, first) in cases {
        let mut r = rig(style);
        r.fake.rule("folder1", Rule::Overflow(why));
        select(&mut r.shell, &[0]);
        let fx = key(&mut r.shell, Key::Delete, 20);
        assert!(!has_job(&fx), "the bin is not used behind the user's back");
        let text = r.shell.dialog_text().expect("the permanent dialog");
        assert!(text.starts_with(first), "{text}");
        assert!(text.contains("Permanently delete 1 item ("), "{text}");
        assert!(text.contains("This can't be undone."), "{text}");
        let p = button_center(&r.shell, "Delete permanently");
        let fx = click(&mut r.shell, p, 30);
        run_job(&mut r.shell, fx, 40);
        assert_eq!(r.fake.calls(), vec!["perm:folder1".to_string()]);
    }
}

// ---- 5. protected items -----------------------------------------------------------

const NOTE: &str = "Your documents folder. Windows and your apps keep files here.";

fn protected_rig() -> Rig {
    let r = rig(BinStyle::Recycle);
    r.fake.rule("folder1", Rule::Protected(NOTE));
    r
}

#[test]
fn a_protected_item_repeats_the_note_and_needs_a_hold() {
    let mut r = protected_rig();
    select(&mut r.shell, &[0]);
    let fx = key(&mut r.shell, Key::Delete, 20);
    assert!(!has_job(&fx));
    let text = r.shell.dialog_text().expect("the stronger dialog");
    assert!(text.contains(NOTE), "{text}");
    assert!(
        text.contains("Hold the button for 1.5 seconds to confirm."),
        "{text}"
    );
    assert_eq!(
        r.shell.dialog_buttons(),
        [format!("Hold to move to the {BIN}"), "Cancel".to_string()]
    );
}

#[test]
fn releasing_early_cancels_the_hold_and_a_full_hold_confirms() {
    let mut r = protected_rig();
    select(&mut r.shell, &[0]);
    key(&mut r.shell, Key::Delete, 20);
    let label = format!("Hold to move to the {BIN}");
    let p = button_center(&r.shell, &label);

    // Press, hold for a second, let go: nothing happens.
    r.shell
        .update(InputEvent::PointerDown { x: p.0, y: p.1 }, t(1_000));
    r.shell.tick(t(1_700));
    r.shell.tick(t(2_000));
    assert!(r.shell.modal_open());
    r.shell
        .update(InputEvent::PointerUp { x: p.0, y: p.1 }, t(2_000));
    click(&mut r.shell, p, 2_001);
    r.shell.tick(t(4_000));
    assert!(r.shell.modal_open(), "released early: still asking");
    assert!(r.fake.calls().is_empty());

    // Press and hold the whole time.
    r.shell
        .update(InputEvent::PointerDown { x: p.0, y: p.1 }, t(5_000));
    r.shell.tick(t(6_400));
    assert!(r.shell.modal_open(), "1.4 s is not enough");
    r.shell.tick(t(6_500));
    assert!(!r.shell.modal_open(), "1.5 s confirms");
    let fx = r.shell.take_effects();
    assert!(has_job(&fx), "{fx:?}");
    run_job(&mut r.shell, fx, 6_600);
    assert_eq!(r.fake.calls(), vec!["trash:folder1:confirmed".to_string()]);
}

#[test]
fn the_hold_fills_the_button_as_time_passes() {
    let mut r = protected_rig();
    select(&mut r.shell, &[0]);
    key(&mut r.shell, Key::Delete, 20);
    let label = format!("Hold to move to the {BIN}");
    let p = button_center(&r.shell, &label);
    let progress = |shell: &Shell<TreeSource>| {
        shell
            .popup()
            .unwrap()
            .card
            .parts
            .iter()
            .find_map(|part| match part {
                bs_render::CardPart::Hold { progress, .. } => Some(*progress),
                _ => None,
            })
            .unwrap()
    };
    assert_eq!(progress(&r.shell), 0.0);
    r.shell
        .update(InputEvent::PointerDown { x: p.0, y: p.1 }, t(1_000));
    r.shell.tick(t(1_750));
    assert!((progress(&r.shell) - 0.5).abs() < 0.02);
    assert!(
        r.shell.next_deadline(t(1_750)).is_some(),
        "frames keep coming"
    );
}

#[test]
fn a_click_alone_never_confirms_a_protected_delete() {
    let mut r = protected_rig();
    select(&mut r.shell, &[0]);
    key(&mut r.shell, Key::Delete, 20);
    let p = button_center(&r.shell, &format!("Hold to move to the {BIN}"));
    let fx = click(&mut r.shell, p, 30);
    assert!(!has_job(&fx));
    assert!(r.shell.modal_open());
}

#[test]
fn holding_enter_confirms_after_the_time_and_letting_go_cancels() {
    let mut r = protected_rig();
    select(&mut r.shell, &[0]);
    key(&mut r.shell, Key::Delete, 20);
    // Focus starts on Cancel; Tab goes to the hold button.
    key(&mut r.shell, Key::Tab, 30);
    key(&mut r.shell, Key::Enter, 1_000);
    key(&mut r.shell, Key::Enter, 1_500); // key repeat must not restart it
    r.shell.tick(t(2_000));
    r.shell
        .update(InputEvent::KeyReleased(Key::Enter), t(2_100));
    r.shell.tick(t(5_000));
    assert!(r.shell.modal_open());
    assert!(r.fake.calls().is_empty());
    key(&mut r.shell, Key::Enter, 6_000);
    r.shell.tick(t(7_500));
    assert!(!r.shell.modal_open());
    assert!(has_job(&r.shell.take_effects()));
}

#[test]
fn letting_go_after_a_completed_hold_does_not_reach_the_map() {
    let mut r = protected_rig();
    select(&mut r.shell, &[0]);
    key(&mut r.shell, Key::Delete, 20);
    let p = button_center(&r.shell, &format!("Hold to move to the {BIN}"));
    r.shell
        .update(InputEvent::PointerDown { x: p.0, y: p.1 }, t(1_000));
    r.shell.tick(t(2_500));
    assert!(!r.shell.modal_open());
    let before = r.shell.app().unwrap().selection().to_vec();
    let fx = r
        .shell
        .update(InputEvent::PointerUp { x: p.0, y: p.1 }, t(2_600));
    assert!(fx.is_empty());
    assert_eq!(r.shell.app().unwrap().selection(), &before[..]);
}

#[test]
fn a_protected_permanent_delete_needs_the_hold_too() {
    let mut r = protected_rig();
    select(&mut r.shell, &[0]);
    key(&mut r.shell, Key::DeletePermanently, 20);
    let text = r.shell.dialog_text().unwrap();
    assert!(
        text.contains(NOTE) && text.contains("This can't be undone."),
        "{text}"
    );
    assert_eq!(
        r.shell.dialog_buttons(),
        [
            "Hold to delete permanently".to_string(),
            "Cancel".to_string()
        ]
    );
}

// ---- 6. blocked ---------------------------------------------------------------------

#[test]
fn an_operating_system_item_is_never_offered_for_deletion() {
    let mut r = rig(BinStyle::Recycle);
    r.fake.rule("folder1", Rule::Blocked);
    select(&mut r.shell, &[0]);
    let fx = key(&mut r.shell, Key::Delete, 20);
    assert!(!has_job(&fx) && !r.shell.modal_open());
    assert_eq!(info(&r.shell), "Delete: Part of the operating system");
    key(&mut r.shell, Key::DeletePermanently, 30);
    assert!(!r.shell.modal_open());
    assert_eq!(
        info(&r.shell),
        "Delete permanently: Part of the operating system"
    );
    assert!(r.fake.calls().is_empty());
}

#[test]
fn the_blocked_menu_rows_are_disabled_with_the_reason() {
    let mut r = rig(BinStyle::Recycle);
    r.fake.rule("folder1", Rule::Blocked);
    select(&mut r.shell, &[0]);
    key(&mut r.shell, Key::ContextMenu, 20);
    let rows = r.shell.menu_items();
    for label in ["Delete", "Delete permanently"] {
        let row = rows.iter().find(|r| r.0 == label).expect(label);
        assert!(!row.2, "{label} is disabled");
        assert_eq!(row.1, "Part of the operating system");
    }
    // Clicking a disabled row does nothing and keeps the menu open.
    let p = button_center(&r.shell, "Delete");
    let fx = click(&mut r.shell, p, 30);
    assert!(!has_job(&fx));
    assert!(r.shell.modal_open());
}

// ---- 7. several items at once -----------------------------------------------------

#[test]
fn the_strictest_item_of_a_selection_decides() {
    // One protected item among plain ones: the hold applies to the whole.
    let mut r = protected_rig();
    select(&mut r.shell, &[0, 1]);
    let fx = key(&mut r.shell, Key::Delete, 20);
    assert!(!has_job(&fx));
    assert!(r.shell.dialog_text().unwrap().contains(NOTE));
    assert_eq!(
        r.shell.dialog_buttons()[0],
        format!("Hold to move to the {BIN}")
    );

    // One blocked item: the whole selection is refused.
    let mut r = rig(BinStyle::Recycle);
    r.fake.rule("folder2", Rule::Blocked);
    select(&mut r.shell, &[0, 1]);
    let fx = key(&mut r.shell, Key::Delete, 20);
    assert!(!has_job(&fx) && !r.shell.modal_open());
    assert_eq!(info(&r.shell), "Delete: Part of the operating system");

    // One item that can't use the bin: the whole selection is deleted for
    // good, after the question.
    let mut r = rig(BinStyle::Recycle);
    r.fake
        .rule("folder2", Rule::Overflow(PermanentReason::BinDisabled));
    select(&mut r.shell, &[0, 1]);
    let fx = key(&mut r.shell, Key::Delete, 20);
    assert!(!has_job(&fx));
    let text = r.shell.dialog_text().unwrap();
    assert!(
        text.starts_with("This drive is set to delete without"),
        "{text}"
    );
    assert!(text.contains("Permanently delete 2 items ("), "{text}");
}

#[test]
fn an_item_that_changed_since_the_scan_is_refused_with_a_reason() {
    let mut r = rig(BinStyle::Trash);
    r.fake.rule("folder1", Rule::Changed);
    select(&mut r.shell, &[0]);
    let fx = key(&mut r.shell, Key::Delete, 20);
    assert!(!has_job(&fx));
    assert_eq!(
        info(&r.shell),
        "Delete: it changed since the scan; rescan the folder first"
    );
}

#[test]
fn several_items_in_one_move_make_one_toast_and_one_undo_step() {
    let mut r = rig(BinStyle::Trash);
    select(&mut r.shell, &[0, 1]);
    let fx = key(&mut r.shell, Key::Delete, 20);
    let fx = run_job(&mut r.shell, fx, 30);
    assert_eq!(
        r.shell.toast_text().as_deref(),
        Some("Moved to Trash · 24.6 GB · Undo")
    );
    assert_eq!(tree_changes(&fx).len(), 2);
    assert_eq!(r.shell.undo_depth(), 1);
    let fx = key(&mut r.shell, Key::Undo, 40);
    let fx = run_job(&mut r.shell, fx, 50);
    assert_eq!(tree_changes(&fx).len(), 2);
    assert_eq!(r.shell.toast_text().as_deref(), Some("Put 2 items back"));
}

// ---- 8. scanning ------------------------------------------------------------------

#[test]
fn nothing_is_deleted_from_a_folder_that_is_still_being_counted() {
    let mut r = rig_with(names_for(3), false, BinStyle::Trash, 1.0);
    select(&mut r.shell, &[0]);
    for k in [Key::Delete, Key::DeletePermanently] {
        let fx = key(&mut r.shell, k, 20);
        assert!(!has_job(&fx) && !r.shell.modal_open());
    }
    assert_eq!(info(&r.shell), "Delete permanently: Still scanning");
    key(&mut r.shell, Key::ContextMenu, 30);
    let rows = r.shell.menu_items();
    let delete = rows.iter().find(|r| r.0 == "Delete").unwrap();
    assert!(!delete.2);
    assert_eq!(delete.1, "Still scanning");
    assert!(r.fake.calls().is_empty());
}

// ---- 9. emptying the bin ------------------------------------------------------------

fn with_bin(r: &mut Rig, label: &str) {
    r.shell.set_bin_drive(Some(BinDrive {
        mount: PathBuf::from(if label.is_empty() { "/" } else { "C:\\" }),
        label: label.to_string(),
    }));
    r.shell.set_capacity(
        Breakdown {
            used: 40_000_000_000,
            recycle_bin: 14_100_000_000,
            unreadable: 0,
            reserved: 0,
            free: 46_000_000_000,
            total: 100_000_000_000,
            quota: false,
        },
        t(0),
    );
}

#[test]
fn emptying_the_bin_names_the_drive_and_says_undo_goes() {
    for (style, label, title) in [
        (
            BinStyle::Recycle,
            "C:",
            "Empty Recycle Bin on C: (14.1 GB)?",
        ),
        (BinStyle::Trash, "", "Empty Trash on this drive (14.1 GB)?"),
    ] {
        let mut r = rig(style);
        with_bin(&mut r, label);
        // The bar's bin segment: just right of the used part.
        let g = r.shell.geometry().capacity;
        let x = g.x + g.w * 0.45;
        let y = g.y + g.h / 2.0;
        let fx = r.shell.update(InputEvent::Click { x, y }, t(5_000));
        assert!(!has_job(&fx));
        assert_eq!(
            r.shell.dialog_text().as_deref(),
            Some(format!("{title} Undo won't be available for these items.").as_str())
        );
        let action = title
            .trim_end_matches("?")
            .split(" (")
            .next()
            .unwrap()
            .to_string();
        assert_eq!(
            r.shell.dialog_buttons(),
            [action.clone(), "Cancel".to_string()]
        );
        let p = button_center(&r.shell, &action);
        let fx = click(&mut r.shell, p, 5_010);
        let fx = run_job(&mut r.shell, fx, 5_020);
        assert_eq!(
            r.fake.calls(),
            vec![format!(
                "empty:{}",
                if label.is_empty() { "/" } else { "C:\\" }
            )]
        );
        assert!(fx.iter().any(|e| matches!(e, ShellEffect::BinChanged)));
        let toast = r.shell.toast_text().unwrap();
        assert!(toast.starts_with("Emptied "), "{toast}");
        assert!(toast.contains("14.1 GB given back to the drive"), "{toast}");
        assert!(!toast.to_lowercase().contains("freed"));
    }
}

#[test]
fn emptying_clears_undo_and_turns_the_tiles_into_deleted_ones() {
    let mut r = rig(BinStyle::Recycle);
    with_bin(&mut r, "C:");
    select(&mut r.shell, &[0]);
    let fx = key(&mut r.shell, Key::Delete, 20);
    run_job(&mut r.shell, fx, 30);
    assert_eq!(r.shell.undo_depth(), 1);
    let g = r.shell.geometry().capacity;
    r.shell.update(
        InputEvent::Click {
            x: g.x + g.w * 0.45,
            y: g.y + g.h / 2.0,
        },
        t(40),
    );
    let p = button_center(&r.shell, "Empty Recycle Bin on C:");
    let fx = click(&mut r.shell, p, 50);
    let fx = run_job(&mut r.shell, fx, 60);
    assert_eq!(r.shell.undo_depth(), 0);
    let changes = tree_changes(&fx);
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].to, DeleteState::Deleted);
    // Ctrl+Z now has nothing to put back.
    key(&mut r.shell, Key::Undo, 70);
    assert_eq!(info(&r.shell), "Nothing to undo");
}

#[test]
fn an_empty_bin_says_so_instead_of_asking() {
    let mut r = rig(BinStyle::Recycle);
    r.shell.set_bin_drive(Some(BinDrive {
        mount: PathBuf::from("C:\\"),
        label: "C:".into(),
    }));
    let b = Breakdown {
        total: 100,
        free: 100,
        ..Breakdown::default()
    };
    r.shell.set_capacity(b, t(0));
    r.shell.request_empty(t(10));
    assert!(!r.shell.modal_open());
    assert_eq!(info(&r.shell), "The Recycle Bin on this drive is empty");
}

#[test]
fn a_partly_emptied_bin_says_what_stopped() {
    let mut r = rig(BinStyle::Trash);
    with_bin(&mut r, "");
    *r.fake.empty.lock().unwrap() = EmptyOutcome::Partial {
        bytes: 5_000_000_000,
        entries: 1,
        error: OpError {
            step: "remove",
            kind: std::io::ErrorKind::PermissionDenied,
            code: Some(13),
            removed_bytes: 0,
            removed_entries: 0,
        },
    };
    r.shell.request_empty(t(10));
    let p = button_center(&r.shell, "Empty Trash on this drive");
    let fx = click(&mut r.shell, p, 20);
    run_job(&mut r.shell, fx, 30);
    let text = r.shell.toast_text().unwrap();
    assert!(
        text.starts_with("Emptied part of the Trash · 5.0 GB given back to the drive"),
        "{text}"
    );
    assert!(
        text.contains("Couldn't empty the rest: permission denied"),
        "{text}"
    );
}

// ---- 10. the context menu ------------------------------------------------------------

#[test]
fn a_right_click_opens_the_menu_in_the_documented_order() {
    let mut r = rig(BinStyle::Recycle);
    let all = folders(&r.shell);
    let p = title_point(all[0].rect);
    r.shell
        .update(InputEvent::RightClick { x: p.0, y: p.1 }, t(10));
    assert!(r.shell.modal_open());
    let labels: Vec<String> = r.shell.menu_items().into_iter().map(|r| r.0).collect();
    assert_eq!(
        labels,
        [
            "Open",
            "Show in Explorer",
            "Open terminal here",
            "Copy path",
            "Properties",
            "Zoom into",
            "Rescan this folder",
            "Delete",
            "Delete permanently",
        ]
        .map(|s| if cfg!(windows) {
            s.to_string()
        } else {
            s.replace("Show in Explorer", "Show in file manager")
        })
    );
    // The box under the pointer is selected.
    assert_eq!(r.shell.app().unwrap().selection(), &[all[0].id]);
}

#[test]
fn properties_is_left_out_where_the_file_manager_has_none() {
    let mut r = rig(BinStyle::Trash);
    r.shell.set_capabilities(Capabilities { properties: false });
    select(&mut r.shell, &[0]);
    key(&mut r.shell, Key::ContextMenu, 20);
    assert!(r.shell.menu_items().iter().all(|r| r.0 != "Properties"));
}

#[test]
fn menu_rows_work_with_pointer_and_keyboard_and_escape_closes() {
    let mut r = rig(BinStyle::Trash);
    select(&mut r.shell, &[0]);
    key(&mut r.shell, Key::ContextMenu, 20);
    // Click Copy path.
    let p = button_center(&r.shell, "Copy path");
    let fx = click(&mut r.shell, p, 30);
    assert!(!r.shell.modal_open());
    assert!(
        fx.iter()
            .any(|e| matches!(e, ShellEffect::CopyText(t) if t.ends_with("folder1"))),
        "{fx:?}"
    );
    // The keyboard: the first working row is hot; Down moves on; Enter acts.
    key(&mut r.shell, Key::ContextMenu, 40);
    key(&mut r.shell, Key::Arrow(bs_ui::Direction::Down), 41);
    let fx = key(&mut r.shell, Key::Enter, 42);
    assert!(!r.shell.modal_open());
    assert!(!fx.is_empty());
    // Escape closes without doing anything.
    key(&mut r.shell, Key::ContextMenu, 50);
    let fx = key(&mut r.shell, Key::Escape, 51);
    assert!(!r.shell.modal_open() && fx.is_empty());
    // A click away closes it and does not select what is under it.
    let all = folders(&r.shell);
    key(&mut r.shell, Key::ContextMenu, 60);
    let before = r.shell.app().unwrap().selection().to_vec();
    click(&mut r.shell, center(all[2].rect), 61);
    assert!(!r.shell.modal_open());
    assert_eq!(r.shell.app().unwrap().selection(), &before[..]);
}

#[test]
fn the_menu_delete_row_runs_the_delete_flow() {
    let mut r = rig(BinStyle::Trash);
    select(&mut r.shell, &[0]);
    key(&mut r.shell, Key::ContextMenu, 20);
    let p = button_center(&r.shell, "Delete");
    let fx = click(&mut r.shell, p, 30);
    assert!(has_job(&fx));
}

#[test]
fn rows_that_need_one_item_say_so_for_several() {
    let mut r = rig(BinStyle::Trash);
    select(&mut r.shell, &[0, 1]);
    key(&mut r.shell, Key::ContextMenu, 20);
    for (label, hint, enabled) in r.shell.menu_items() {
        match label.as_str() {
            "Open" | "Zoom into" | "Rescan this folder" | "Open terminal here" => {
                assert!(!enabled, "{label}");
                assert_eq!(hint, "Select one item");
            }
            "Delete" | "Copy path" => assert!(enabled, "{label}"),
            _ => {}
        }
    }
}

#[test]
fn menu_hostile_names_do_not_break_anything() {
    let names = vec![
        "x".repeat(4000).into_bytes(),
        "ab\u{202e}cd\u{200b}\u{0301}\n\tz".as_bytes().to_vec(),
        vec![b'f', 0xff, 0xfe, b'g'],
    ];
    let mut r = rig_with(names, true, BinStyle::Recycle, 1.0);
    r.fake.rule(&"x".repeat(4000), Rule::Protected(NOTE));
    for i in 0..3 {
        select(&mut r.shell, &[i]);
        key(&mut r.shell, Key::ContextMenu, 20);
        assert!(r.shell.modal_open());
        key(&mut r.shell, Key::Escape, 21);
        key(&mut r.shell, Key::DeletePermanently, 22);
        let text = r.shell.dialog_text().expect("dialog");
        assert!(!text.contains('\u{0}'));
        let popup = r.shell.popup().unwrap();
        assert!(
            popup.card.rect.right() <= SIZE.0 as f32 && popup.card.rect.bottom() <= SIZE.1 as f32
        );
        key(&mut r.shell, Key::Escape, 23);
    }
}

// ---- 11. the map ----------------------------------------------------------------------

#[test]
fn a_deleted_tile_stays_where_it_is_and_the_parent_title_shows_the_total() {
    let mut r = rig(BinStyle::Trash);
    let before = folders(&r.shell);
    // What the host does with a Tree effect.
    let app = r.shell.app().unwrap();
    let view = app.current_view();
    let node = app
        .source()
        .target(view, before[0].id)
        .unwrap()
        .node
        .unwrap();
    r.agg.mark_deleted(node, DeleteState::InBin).unwrap();
    let snap = r.agg.publish();
    r.shell.app_mut().unwrap().source_mut().set_snapshot(snap);
    r.shell.update(InputEvent::DataChanged, t(100));
    let after = folders(&r.shell);
    assert_eq!(before.len(), after.len());
    for (a, b) in before.iter().zip(&after) {
        assert_eq!(a.id, b.id);
        assert_eq!(a.rect, b.rect, "nothing moves");
    }
    let hit = after.iter().find(|i| i.id == before[0].id).unwrap();
    assert!(hit.flags.contains(bs_render::Flags::DELETED));
    let label = hit.label.as_ref().expect("a label");
    let words = format!("{} {}", label.name, label.size);
    assert!(words.contains("Moved to"), "{words}");
    // The root title shows what moved.
    let root = r
        .shell
        .app()
        .unwrap()
        .items()
        .iter()
        .find(|i| i.depth == 0)
        .and_then(|i| i.label.as_ref().map(|l| format!("{} {}", l.name, l.size)));
    if let Some(root) = root {
        assert!(root.contains("moved to"), "{root}");
    }
}

#[test]
fn deleted_boxes_cannot_be_acted_on_again() {
    let mut r = rig(BinStyle::Trash);
    let before = folders(&r.shell);
    let app = r.shell.app().unwrap();
    let view = app.current_view();
    let node = app
        .source()
        .target(view, before[0].id)
        .unwrap()
        .node
        .unwrap();
    r.agg.mark_deleted(node, DeleteState::InBin).unwrap();
    let snap = r.agg.publish();
    r.shell.app_mut().unwrap().source_mut().set_snapshot(snap);
    r.shell.update(InputEvent::DataChanged, t(100));
    select(&mut r.shell, &[0]);
    let fx = key(&mut r.shell, Key::Delete, 120);
    assert!(!has_job(&fx));
    assert_eq!(info(&r.shell), "Delete: Already moved");
}

// ---- 12. a stand-in with no platform ----------------------------------------------------

#[test]
fn without_a_platform_service_nothing_is_deleted() {
    let (_, snapshot) = build(&names_for(3), true);
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
    let map = shell.map_rect();
    let source = TreeSource::new(snapshot, Metrics::from_logical(13.0, 1.0)).unwrap();
    shell.set_app(App::new(source, map));
    select(&mut shell, &[0]);
    let fx = key(&mut shell, Key::Delete, 20);
    assert!(!has_job(&fx));
    assert_eq!(info(&shell), "Delete: not available here");
}

#[test]
fn resizing_keeps_a_dialog_on_screen_and_closes_a_menu() {
    let mut r = rig(BinStyle::Trash);
    select(&mut r.shell, &[0]);
    key(&mut r.shell, Key::DeletePermanently, 20);
    r.shell.resize((640, 400), 1.0, t(30));
    let rect = r.shell.popup().unwrap().card.rect;
    assert!(rect.right() <= 640.0 && rect.bottom() <= 400.0, "{rect:?}");
    key(&mut r.shell, Key::Escape, 40);
    select(&mut r.shell, &[0]);
    key(&mut r.shell, Key::ContextMenu, 50);
    assert!(r.shell.modal_open());
    r.shell.resize((900, 600), 1.0, t(60));
    assert!(!r.shell.modal_open());
}

#[test]
fn the_dialog_and_menu_scale_with_the_display() {
    for scale in [1.0f32, 2.0] {
        let mut r = rig_with(names_for(3), true, BinStyle::Trash, scale);
        select(&mut r.shell, &[0]);
        key(&mut r.shell, Key::DeletePermanently, 20);
        let rect = r.shell.popup().unwrap().card.rect;
        assert!(rect.w <= SIZE.0 as f32 * scale && rect.h <= SIZE.1 as f32 * scale);
        assert!(rect.w >= 13.0 * scale * 12.0);
    }
}

#[test]
fn every_word_the_flows_produce_avoids_the_banned_ones() {
    // The wording lint covers the source; this checks the assembled texts.
    let banned = ["safe", "junk", "reclaim", "clean up", "freed"];
    let mut r = protected_rig();
    let mut seen: Vec<String> = Vec::new();
    select(&mut r.shell, &[0]);
    key(&mut r.shell, Key::Delete, 20);
    seen.extend(r.shell.dialog_text());
    key(&mut r.shell, Key::Escape, 21);
    select(&mut r.shell, &[1]);
    let fx = key(&mut r.shell, Key::Delete, 30);
    run_job(&mut r.shell, fx, 40);
    seen.extend(r.shell.toast_text());
    key(&mut r.shell, Key::ContextMenu, 50);
    seen.extend(r.shell.menu_items().into_iter().flat_map(|r| [r.0, r.1]));
    for text in seen {
        let lower = text.to_lowercase();
        for word in banned {
            assert!(!lower.contains(word), "{text:?} contains {word}");
        }
    }
}

#[test]
fn estimated_and_partial_sizes_carry_their_marks() {
    // A folder still being counted is refused, so only the dialog of a
    // complete folder with an unreadable part shows the lower bound; here the
    // marks themselves are checked on the words.
    let mut r = rig(BinStyle::Trash);
    select(&mut r.shell, &[0]);
    key(&mut r.shell, Key::DeletePermanently, 20);
    let text = r.shell.dialog_text().unwrap();
    assert!(!text.contains('≥') && !text.contains('≈'), "{text}");
}

// ---- 13. the accessibility tree ---------------------------------------------------

fn tree_of(shell: &mut Shell<TreeSource>, at: u64) -> bs_ui::shell::access::AccessTree {
    let mut tree = bs_ui::shell::access::AccessTree::new();
    shell.access_update(&mut tree, t(at));
    tree
}

#[test]
fn the_toast_is_a_polite_status_with_a_working_undo_button() {
    use accesskit::{Live, Role};
    use bs_ui::shell::access::AccessTarget;
    let mut r = rig(BinStyle::Trash);
    select(&mut r.shell, &[0]);
    let fx = key(&mut r.shell, Key::Delete, 20);
    run_job(&mut r.shell, fx, 100);
    r.shell.tick(t(400));
    let tree = tree_of(&mut r.shell, 400);
    let undo = tree
        .node_for(AccessTarget::ToastUndo)
        .expect("an Undo node");
    assert_eq!(tree.get(undo).unwrap().role(), Role::Button);
    let status = tree.get(accesskit::NodeId(21)).expect("the toast node");
    assert_eq!(status.role(), Role::Status);
    assert_eq!(status.live(), Some(Live::Polite));
    assert_eq!(status.value(), Some("Moved to Trash · 12.3 GB · Undo"));
    // A screen reader clicking Undo acts like the pointer.
    let request = accesskit::ActionRequest {
        action: accesskit::Action::Click,
        target_tree: accesskit::TreeId::ROOT,
        target_node: undo,
        data: None,
    };
    let fx = r.shell.access_action(&tree, &request, t(410));
    assert!(has_job(&fx), "{fx:?}");
}

#[test]
fn a_dialog_is_modal_with_its_text_and_buttons_and_a_hold_says_how() {
    use accesskit::Role;
    use bs_ui::shell::access::AccessTarget;
    let mut r = protected_rig();
    select(&mut r.shell, &[0]);
    key(&mut r.shell, Key::Delete, 20);
    let tree = tree_of(&mut r.shell, 30);
    let dialog = tree.get(accesskit::NodeId(20)).expect("the dialog node");
    assert_eq!(dialog.role(), Role::Dialog);
    assert!(dialog.is_modal());
    assert!(dialog.description().unwrap().contains(NOTE));
    let hold = tree.node_for(AccessTarget::Popup(0)).unwrap();
    let hold = tree.get(hold).unwrap();
    assert!(hold.label().unwrap().starts_with("Hold to move"));
    assert!(hold.description().unwrap().contains("hold Enter or Space"));
    // A click can't stand in for the hold.
    assert!(!hold.supports_action(accesskit::Action::Click));
    let cancel = tree.node_for(AccessTarget::Popup(1)).unwrap();
    assert!(
        tree.get(cancel)
            .unwrap()
            .supports_action(accesskit::Action::Click)
    );
}

#[test]
fn a_menu_lists_its_rows_with_the_reason_for_each_disabled_one() {
    use accesskit::Role;
    use bs_ui::shell::access::AccessTarget;
    let mut r = rig(BinStyle::Trash);
    r.fake.rule("folder1", Rule::Blocked);
    select(&mut r.shell, &[0]);
    key(&mut r.shell, Key::ContextMenu, 20);
    let tree = tree_of(&mut r.shell, 30);
    assert_eq!(tree.get(accesskit::NodeId(20)).unwrap().role(), Role::Menu);
    let mut found = false;
    for i in 0..12 {
        let Some(id) = tree.node_for(AccessTarget::Popup(i)) else {
            break;
        };
        let n = tree.get(id).unwrap();
        assert_eq!(n.role(), Role::MenuItem);
        if n.label() == Some("Delete") {
            assert!(n.is_disabled());
            assert_eq!(n.description(), Some("Part of the operating system"));
            found = true;
        }
    }
    assert!(found);
    // Closing it takes the nodes away.
    key(&mut r.shell, Key::Escape, 40);
    let tree = tree_of(&mut r.shell, 50);
    assert!(tree.get(accesskit::NodeId(20)).is_none());
}

// ---- 14. marks and undo never cross from one map to another ---------------------

/// What the host does when the user has the window show another map: the old
/// one goes, a new one with other boxes takes its place.
fn open_other_map(r: &mut Rig, prefix: &str) -> (Aggregator, Arc<Snapshot>) {
    let names: Vec<Vec<u8>> = (1..=4)
        .map(|i| format!("{prefix}{i}").into_bytes())
        .collect();
    let (agg, snap) = build(&names, true);
    let map = r.shell.map_rect();
    let source = TreeSource::new(snap.clone(), Metrics::from_logical(13.0, 1.0)).expect("root");
    r.shell.clear_app();
    r.shell.set_app(App::new(source, map));
    (agg, snap)
}

fn host_applies(r: &mut Rig, fx: &[ShellEffect], at: u64) {
    for c in tree_changes(fx) {
        let _ = r.agg.unmark_deleted(c.node);
        if c.to != DeleteState::None {
            r.agg.mark_deleted(c.node, c.to).unwrap();
        }
    }
    let snap = r.agg.publish();
    r.shell.app_mut().unwrap().source_mut().set_snapshot(snap);
    r.shell.update(InputEvent::DataChanged, t(at));
}

fn delete_first_folder(r: &mut Rig, at: u64) -> Vec<ShellEffect> {
    select(&mut r.shell, &[0]);
    let fx = key(&mut r.shell, Key::Delete, at);
    run_job(&mut r.shell, fx, at + 10)
}

#[test]
fn emptying_the_bin_on_another_map_marks_none_of_its_boxes() {
    let mut r = rig(BinStyle::Recycle);
    with_bin(&mut r, "C:");
    let old = tree_changes(&delete_first_folder(&mut r, 20));
    assert_eq!(old.len(), 1, "the delete itself was marked on its own map");
    let (mut agg2, snap2) = open_other_map(&mut r, "other");
    r.shell.request_empty(t(40));
    let p = button_center(&r.shell, "Empty Recycle Bin on C:");
    let fx = click(&mut r.shell, p, 50);
    let fx = run_job(&mut r.shell, fx, 60);
    let changes = tree_changes(&fx);
    assert!(
        changes.is_empty(),
        "marks of the old map reach the new one: {changes:?}"
    );
    let after = agg2.publish();
    assert_eq!(after.root().unwrap().delete_totals().deleted.allocated, 0);
    drop((agg2, snap2));
    assert_eq!(r.shell.undo_depth(), 0, "what the emptied bin held is gone");
}

#[test]
fn undo_on_another_map_puts_the_file_back_without_touching_its_boxes() {
    let mut r = rig(BinStyle::Trash);
    delete_first_folder(&mut r, 20);
    open_other_map(&mut r, "other");
    let fx = key(&mut r.shell, Key::Undo, 100);
    assert!(has_job(&fx), "the file can still come back from the bin");
    let fx = run_job(&mut r.shell, fx, 110);
    assert_eq!(r.fake.calls().last().unwrap(), "restore:folder1");
    assert!(tree_changes(&fx).is_empty(), "{fx:?}");
    assert_eq!(r.shell.toast_text().as_deref(), Some("Put “folder1” back"));
}

#[test]
fn rescanning_a_folder_does_not_carry_the_marks_to_the_new_map() {
    let mut r = rig(BinStyle::Trash);
    delete_first_folder(&mut r, 20);
    // Shift+F5 on a folder asks the host to read it again.
    select(&mut r.shell, &[1]);
    let fx = key(&mut r.shell, Key::Rescan, 40);
    assert!(
        fx.iter().any(|e| matches!(
            e,
            ShellEffect::Action {
                action: bs_ui::action::Action::Rescan,
                ..
            }
        )),
        "{fx:?}"
    );
    // The host scans it into a new tree: new boxes, the same names.
    let names = names_for(3);
    let (mut agg2, snap2) = build(&names, true);
    let map = r.shell.map_rect();
    let source = TreeSource::new(snap2, Metrics::from_logical(13.0, 1.0)).expect("root");
    r.shell.clear_app();
    r.shell.set_app(App::new(source, map));
    let fx = key(&mut r.shell, Key::Undo, 100);
    let fx = run_job(&mut r.shell, fx, 110);
    for c in tree_changes(&fx) {
        agg2.unmark_deleted(c.node).ok();
    }
    assert!(tree_changes(&fx).is_empty(), "{fx:?}");
    assert_eq!(
        agg2.publish()
            .root()
            .unwrap()
            .delete_totals()
            .in_bin
            .allocated,
        0
    );
}

#[test]
fn the_rescan_banner_does_not_carry_the_marks_to_the_new_map() {
    let mut r = rig(BinStyle::Trash);
    delete_first_folder(&mut r, 20);
    // The scan was stopped: a banner offers Rescan.
    r.shell.set_stopped(true);
    let chrome = r.shell.chrome(t(30));
    let rescan = chrome
        .widgets
        .iter()
        .find_map(|w| match w {
            bs_render::Widget::Pill { rect, text, .. } if &**text == "Rescan" => Some(*rect),
            _ => None,
        })
        .expect("the banner's Rescan button");
    let fx = click(&mut r.shell, center(rescan), 40);
    assert!(
        fx.iter().any(|e| matches!(e, ShellEffect::Rescan)),
        "{fx:?}"
    );
    r.shell.set_stopped(false);
    open_other_map(&mut r, "again");
    let fx = key(&mut r.shell, Key::Undo, 100);
    let fx = run_job(&mut r.shell, fx, 110);
    assert!(tree_changes(&fx).is_empty(), "{fx:?}");
}

#[test]
fn a_delete_that_finishes_after_the_map_changed_marks_nothing() {
    let mut r = rig(BinStyle::Trash);
    select(&mut r.shell, &[0]);
    let fx = key(&mut r.shell, Key::Delete, 20);
    assert!(has_job(&fx));
    // The user opens another drive before the disk has finished.
    open_other_map(&mut r, "other");
    let fx = run_job(&mut r.shell, fx, 100);
    assert!(tree_changes(&fx).is_empty(), "{fx:?}");
    assert!(fx.iter().any(|e| matches!(e, ShellEffect::BinChanged)));
    assert_eq!(
        r.shell.toast_text().as_deref(),
        Some("Moved to Trash · 12.3 GB · Undo"),
        "the file did move, and the toast says so"
    );
    // Undoing it afterwards still marks nothing on the map now showing.
    let fx = key(&mut r.shell, Key::Undo, 200);
    let fx = run_job(&mut r.shell, fx, 210);
    assert!(tree_changes(&fx).is_empty(), "{fx:?}");
}

#[test]
fn emptying_one_drive_leaves_the_undo_of_another_drives_bin() {
    let mut r = rig(BinStyle::Recycle);
    r.shell.set_bin_drive(Some(BinDrive {
        mount: PathBuf::from("C:\\"),
        label: "C:".into(),
    }));
    delete_first_folder(&mut r, 20);
    // Another drive is opened and something is deleted there.
    open_other_map(&mut r, "other");
    r.shell.set_bin_drive(Some(BinDrive {
        mount: PathBuf::from("D:\\"),
        label: "D:".into(),
    }));
    r.shell.set_capacity(
        Breakdown {
            used: 40_000_000_000,
            recycle_bin: 14_100_000_000,
            unreadable: 0,
            reserved: 0,
            free: 46_000_000_000,
            total: 100_000_000_000,
            quota: false,
        },
        t(0),
    );
    delete_first_folder(&mut r, 100);
    assert_eq!(r.shell.undo_depth(), 2);
    r.shell.request_empty(t(200));
    let p = button_center(&r.shell, "Empty Recycle Bin on D:");
    let fx = click(&mut r.shell, p, 210);
    let fx = run_job(&mut r.shell, fx, 220);
    assert_eq!(
        tree_changes(&fx).len(),
        1,
        "only the box deleted on the emptied drive turns into a deleted one"
    );
    assert_eq!(r.shell.undo_depth(), 1, "C:'s bin was not touched");
    let fx = key(&mut r.shell, Key::Undo, 300);
    run_job(&mut r.shell, fx, 310);
    assert_eq!(r.fake.calls().last().unwrap(), "restore:folder1");
}

/// A fresh directory with three folders in it, for tests that need the real
/// disk.
fn real_dirs(tag: &str) -> PathBuf {
    let base = std::env::var_os("TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let dir = base.join(format!("bs-ui-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    for i in 1..=3 {
        std::fs::create_dir_all(dir.join(format!("folder{i}"))).unwrap();
    }
    dir
}

#[test]
fn what_was_seen_before_a_rescan_does_not_make_the_new_map_refuse_the_folder() {
    let dir = real_dirs("seen");
    let root = dir.to_string_lossy().into_owned().into_bytes();
    let make = |root: &[u8]| {
        let mut agg = Aggregator::new();
        let mut b = Batch::new();
        b.dir(DirKey(0), DirKey::NONE, root, &meta(4096)).unwrap();
        for i in 1..=3u32 {
            let key = DirKey(i);
            b.dir(key, DirKey(0), format!("folder{i}").as_bytes(), &meta(4096))
                .unwrap();
            b.file(
                NodeKind::File,
                key,
                b"f",
                &meta(1_000_000 * u64::from(4 - i)),
            )
            .unwrap();
            b.listing_done(key);
        }
        b.listing_done(DirKey(0));
        agg.apply(&b);
        agg.publish()
    };
    let mut r = rig(BinStyle::Trash);
    let map = r.shell.map_rect();
    let source = TreeSource::new(make(&root), Metrics::from_logical(13.0, 1.0)).unwrap();
    r.shell.clear_app();
    r.shell.set_app(App::new(source, map));
    select(&mut r.shell, &[0]);
    // The folder is replaced on disk (a new one under the old name) and the
    // user scans again.
    std::fs::rename(dir.join("folder1"), dir.join("folder1.old")).unwrap();
    std::fs::create_dir_all(dir.join("folder1")).unwrap();
    let source = TreeSource::new(make(&root), Metrics::from_logical(13.0, 1.0)).unwrap();
    r.shell.clear_app();
    r.shell.set_app(App::new(source, map));
    select(&mut r.shell, &[0]);
    key(&mut r.shell, Key::Delete, 50);
    let now_on_disk = bs_platform::delete::identity_of(&dir.join("folder1")).unwrap();
    let given = r.fake.seen_log.lock().unwrap().last().cloned().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(
        given,
        vec![Some(now_on_disk)],
        "the identity noted before the rescan was kept"
    );
}

// ---- 15. keyboard: Ctrl+Z, the toast button, losing the window ----------------------

#[test]
fn ctrl_z_works_while_the_deleted_box_is_still_selected() {
    let mut r = rig(BinStyle::Trash);
    let fx = delete_first_folder(&mut r, 20);
    // The host marks the tile; the box stays selected and shows as moved.
    host_applies(&mut r, &fx, 35);
    assert_eq!(r.shell.app().unwrap().selection().len(), 1);
    let fx = key(&mut r.shell, Key::Undo, 40);
    assert!(has_job(&fx), "Ctrl+Z refused: {}", info(&r.shell));
    let fx = run_job(&mut r.shell, fx, 50);
    let changes = tree_changes(&fx);
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].to, DeleteState::None);
}

#[test]
fn ctrl_z_does_not_need_anything_selected() {
    let mut r = rig(BinStyle::Trash);
    delete_first_folder(&mut r, 20);
    key(&mut r.shell, Key::Escape, 25); // closes the toast's card, then the selection
    key(&mut r.shell, Key::Escape, 26);
    assert!(r.shell.app().unwrap().selection().is_empty());
    let fx = key(&mut r.shell, Key::Undo, 40);
    assert!(has_job(&fx), "{}", info(&r.shell));
}

fn toast_button_is_hot(shell: &Shell<TreeSource>, at: u64) -> bool {
    let card = shell.toast_card(t(at)).expect("a toast");
    card.parts
        .iter()
        .any(|p| matches!(p, bs_render::CardPart::Button { hot: true, .. }))
}

#[test]
fn the_toast_button_is_reached_with_tab_and_pressed_with_enter() {
    let mut r = rig(BinStyle::Trash);
    delete_first_folder(&mut r, 20);
    r.shell.tick(t(500));
    assert!(!toast_button_is_hot(&r.shell, 500));
    let fx = key(&mut r.shell, Key::Tab, 510);
    assert!(fx.is_empty());
    assert!(toast_button_is_hot(&r.shell, 510), "Tab reaches Undo");
    // Tab again lets go of it.
    key(&mut r.shell, Key::Tab, 520);
    assert!(!toast_button_is_hot(&r.shell, 520));
    // Tab, then Enter puts the file back.
    key(&mut r.shell, Key::Tab, 530);
    let fx = key(&mut r.shell, Key::Enter, 540);
    assert!(has_job(&fx), "Enter on the focused Undo");
    let fx = run_job(&mut r.shell, fx, 550);
    assert_eq!(tree_changes(&fx)[0].to, DeleteState::None);
}

#[test]
fn another_key_lets_go_of_the_toast_button() {
    let mut r = rig(BinStyle::Trash);
    delete_first_folder(&mut r, 20);
    key(&mut r.shell, Key::Tab, 510);
    key(&mut r.shell, Key::Escape, 520);
    assert!(!toast_button_is_hot(&r.shell, 520));
    let fx = key(&mut r.shell, Key::Enter, 530);
    assert!(!has_job(&fx), "Enter no longer presses Undo");
}

#[test]
fn losing_the_window_cancels_a_hold_that_was_running() {
    let mut r = protected_rig();
    select(&mut r.shell, &[0]);
    key(&mut r.shell, Key::Delete, 20);
    key(&mut r.shell, Key::Tab, 30);
    key(&mut r.shell, Key::Enter, 1_000);
    // The user switches to another window; the key comes up over there.
    r.shell.update(InputEvent::FocusLost, t(1_200));
    r.shell.tick(t(5_000));
    assert!(r.shell.modal_open(), "the dialog is still waiting");
    assert!(!has_job(&r.shell.take_effects()), "nothing was confirmed");
    assert!(r.fake.calls().is_empty());
    // Back in the window, holding starts from nothing again.
    key(&mut r.shell, Key::Enter, 6_000);
    r.shell.tick(t(6_600));
    assert!(r.shell.modal_open());
    r.shell.tick(t(7_600));
    assert!(!r.shell.modal_open());
    assert!(has_job(&r.shell.take_effects()));
}

#[test]
fn losing_the_window_cancels_a_pointer_hold_too() {
    let mut r = protected_rig();
    select(&mut r.shell, &[0]);
    key(&mut r.shell, Key::Delete, 20);
    let p = button_center(&r.shell, &format!("Hold to move to the {BIN}"));
    r.shell
        .update(InputEvent::PointerDown { x: p.0, y: p.1 }, t(1_000));
    r.shell.update(InputEvent::FocusLost, t(1_100));
    r.shell.tick(t(4_000));
    assert!(r.shell.modal_open());
    assert!(!has_job(&r.shell.take_effects()));
}

// ---- 16. the platform's question never blocks the window ----------------------------

#[test]
fn the_menu_opens_at_once_and_the_delete_rows_wait_for_the_platform() {
    let mut r = rig(BinStyle::Trash);
    *r.fake.precheck_delay.lock().unwrap() = Duration::from_millis(200);
    select(&mut r.shell, &[0]);
    let began = std::time::Instant::now();
    let fx = r.shell.update(InputEvent::Key(Key::ContextMenu), t(20));
    assert!(
        began.elapsed() < Duration::from_millis(100),
        "opening the menu waited for the disk: {:?}",
        began.elapsed()
    );
    assert!(r.shell.modal_open());
    assert_eq!(*r.fake.precheck_calls.lock().unwrap(), 0);
    let rows = r.shell.menu_items();
    for label in ["Delete", "Delete permanently"] {
        let row = rows.iter().find(|r| r.0 == label).expect(label);
        assert_eq!((row.1.as_str(), row.2), ("Checking…", false), "{label}");
    }
    let open = rows.iter().find(|r| r.0 == "Open").unwrap();
    assert!(open.2, "the rows that need no answer work already");
    // The answer arrives from a worker.
    let job = fx
        .into_iter()
        .find_map(|e| match e {
            ShellEffect::Run(j) => Some(j),
            _ => None,
        })
        .expect("a question for the platform");
    assert!(job.is_check());
    let done = job.run();
    r.shell.job_done(done, t(300));
    let rows = r.shell.menu_items();
    let del = rows.iter().find(|r| r.0 == "Delete").unwrap();
    assert_eq!((del.1.as_str(), del.2), ("Del", true));
    let perm = rows.iter().find(|r| r.0 == "Delete permanently").unwrap();
    assert_eq!((perm.1.as_str(), perm.2), ("Shift+Del", true));
}

#[test]
fn the_platforms_refusal_reaches_the_menu_row_when_it_answers() {
    let mut r = rig(BinStyle::Trash);
    r.fake.rule("folder1", Rule::Blocked);
    select(&mut r.shell, &[0]);
    let fx = r.shell.update(InputEvent::Key(Key::ContextMenu), t(20));
    let del = |s: &Shell<TreeSource>| {
        s.menu_items()
            .into_iter()
            .find(|r| r.0 == "Delete")
            .unwrap()
    };
    assert_eq!(del(&r.shell).1, "Checking…");
    settle(&mut r.shell, fx, 30);
    assert_eq!(del(&r.shell).1, "Part of the operating system");
    assert!(!del(&r.shell).2);
}

#[test]
fn an_answer_for_a_menu_that_was_closed_is_ignored() {
    let mut r = rig(BinStyle::Trash);
    select(&mut r.shell, &[0]);
    let fx = r.shell.update(InputEvent::Key(Key::ContextMenu), t(20));
    r.shell.update(InputEvent::Key(Key::Escape), t(25));
    assert!(!r.shell.modal_open());
    settle(&mut r.shell, fx, 30);
    assert!(!r.shell.modal_open(), "a late answer opens nothing");
    // And a Delete is not held up by the question that was left open.
    let fx = key(&mut r.shell, Key::Delete, 40);
    assert!(has_job(&fx));
}

#[test]
fn delete_asks_the_platform_on_a_worker_and_changes_nothing_until_it_answers() {
    let mut r = rig(BinStyle::Trash);
    *r.fake.precheck_delay.lock().unwrap() = Duration::from_millis(200);
    select(&mut r.shell, &[0]);
    let began = std::time::Instant::now();
    let fx = r.shell.update(InputEvent::Key(Key::Delete), t(20));
    assert!(began.elapsed() < Duration::from_millis(100));
    assert_eq!(*r.fake.precheck_calls.lock().unwrap(), 0);
    assert!(r.fake.calls().is_empty());
    assert!(!r.shell.modal_open() && r.shell.toast_text().is_none());
    // Pressing it again while the question is open waits for the answer.
    let again = r.shell.update(InputEvent::Key(Key::Delete), t(30));
    assert!(!has_job(&again));
    assert!(info(&r.shell).contains("wait for the last action"));
    let fx = settle(&mut r.shell, fx, 400);
    assert!(has_job(&fx), "the answer lets the move go ahead");
    assert_eq!(*r.fake.precheck_calls.lock().unwrap(), 1);
}

#[test]
fn an_answer_that_comes_after_the_map_changed_deletes_nothing() {
    let mut r = rig(BinStyle::Trash);
    select(&mut r.shell, &[0]);
    let fx = r.shell.update(InputEvent::Key(Key::Delete), t(20));
    open_other_map(&mut r, "other");
    let fx = settle(&mut r.shell, fx, 100);
    assert!(!has_job(&fx), "{fx:?}");
    assert!(r.fake.calls().is_empty());
    assert!(!r.shell.modal_open());
}

// ---- 17. "≈" and "≥" on the sizes of a delete ----------------------------------------

/// Three folders: the second holds a file whose size is an estimate, the
/// third could not be read completely.
fn marked_rig(style: BinStyle) -> Rig {
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
            m.flags = NodeFlags::ESTIMATED_SIZE;
        }
        b.file(NodeKind::File, key, b"file", &m).unwrap();
        if i == 3 {
            b.unreadable(
                key,
                ReadFailure {
                    kind: ReadErrorKind::PermissionDenied,
                    code: 13,
                },
            );
        } else {
            b.listing_done(key);
        }
    }
    b.listing_done(DirKey(0));
    agg.apply(&b);
    let snapshot = agg.publish();
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
    .with_measure(Box::new(Fixed(13.0 * 0.56)));
    let map = shell.map_rect();
    let source = TreeSource::new(snapshot, Metrics::from_logical(13.0, 1.0)).expect("root");
    shell.set_app(App::new(source, map));
    let fake = Fake::new();
    shell.set_disposer(fake.clone());
    shell.set_bin_style(style);
    Rig { shell, fake, agg }
}

#[test]
fn an_estimated_size_carries_its_mark_in_the_dialog_and_the_toast() {
    let mut r = marked_rig(BinStyle::Trash);
    select(&mut r.shell, &[1]);
    key(&mut r.shell, Key::DeletePermanently, 20);
    let text = r.shell.dialog_text().expect("a dialog");
    assert!(text.contains("(≈ "), "{text}");
    assert!(!text.contains('≥'), "{text}");
    let p = button_center(&r.shell, "Delete permanently");
    let fx = click(&mut r.shell, p, 30);
    run_job(&mut r.shell, fx, 40);
    let toast = r.shell.toast_text().unwrap();
    assert!(toast.starts_with("Deleted · ≈ 12.3 GB"), "{toast}");

    let mut r = marked_rig(BinStyle::Trash);
    select(&mut r.shell, &[1]);
    let fx = key(&mut r.shell, Key::Delete, 20);
    run_job(&mut r.shell, fx, 30);
    assert_eq!(
        r.shell.toast_text().as_deref(),
        Some("Moved to Trash · ≈ 12.3 GB · Undo")
    );
}

#[test]
fn a_lower_bound_size_carries_its_mark_in_the_dialog_and_the_toast() {
    let mut r = marked_rig(BinStyle::Trash);
    select(&mut r.shell, &[2]);
    key(&mut r.shell, Key::DeletePermanently, 20);
    let text = r.shell.dialog_text().expect("a dialog");
    assert!(text.contains("(≥ "), "{text}");
    let p = button_center(&r.shell, "Delete permanently");
    let fx = click(&mut r.shell, p, 30);
    run_job(&mut r.shell, fx, 40);
    let toast = r.shell.toast_text().unwrap();
    assert!(toast.starts_with("Deleted · ≥ 12.3 GB"), "{toast}");

    let mut r = marked_rig(BinStyle::Trash);
    select(&mut r.shell, &[2]);
    let fx = key(&mut r.shell, Key::Delete, 20);
    run_job(&mut r.shell, fx, 30);
    assert_eq!(
        r.shell.toast_text().as_deref(),
        Some("Moved to Trash · ≥ 12.3 GB · Undo")
    );
}

#[test]
fn a_selection_with_both_says_that_the_total_is_at_least_that_much() {
    let mut r = marked_rig(BinStyle::Trash);
    select(&mut r.shell, &[1, 2]);
    key(&mut r.shell, Key::DeletePermanently, 20);
    let text = r.shell.dialog_text().expect("a dialog");
    assert!(text.contains("(≥ "), "{text}");
    // A plain folder carries no mark.
    let mut r = marked_rig(BinStyle::Trash);
    select(&mut r.shell, &[0]);
    key(&mut r.shell, Key::DeletePermanently, 20);
    let text = r.shell.dialog_text().expect("a dialog");
    assert!(!text.contains('≥') && !text.contains('≈'), "{text}");
}

#[test]
fn byte_totals_cannot_overflow() {
    let mut r = rig(BinStyle::Trash);
    *r.fake.per_item.lock().unwrap() = u64::MAX;
    select(&mut r.shell, &[0, 1, 2]);
    let fx = key(&mut r.shell, Key::Delete, 20);
    run_job(&mut r.shell, fx, 30);
    let toast = r.shell.toast_text().expect("a toast");
    assert!(toast.starts_with("Moved to Trash · "), "{toast}");
}

// ---- 18. the accessibility focus follows the dialog, the menu and the toast -----------

fn focus_after(
    shell: &mut Shell<TreeSource>,
    tree: &mut bs_ui::shell::access::AccessTree,
    at: u64,
) -> accesskit::NodeId {
    shell.access_update(tree, t(at)).focus
}

#[test]
fn the_screen_reader_focus_is_on_the_dialog_button_the_keyboard_is_on() {
    use bs_ui::shell::access::{AccessTarget, AccessTree};
    let mut r = protected_rig();
    select(&mut r.shell, &[0]);
    let mut tree = AccessTree::new();
    let selected = r.shell.app().unwrap().selected().unwrap();
    assert_eq!(
        focus_after(&mut r.shell, &mut tree, 10),
        AccessTree::box_node_id(selected)
    );
    key(&mut r.shell, Key::Delete, 20);
    // The dialog opens with Cancel.
    let f = focus_after(&mut r.shell, &mut tree, 30);
    assert_eq!(Some(f), tree.node_for(AccessTarget::Popup(1)));
    assert_eq!(tree.get(f).unwrap().label(), Some("Cancel"));
    // Tab moves to the hold button, and the focus goes with it.
    key(&mut r.shell, Key::Tab, 40);
    let f = focus_after(&mut r.shell, &mut tree, 50);
    assert_eq!(Some(f), tree.node_for(AccessTarget::Popup(0)));
    assert!(
        tree.get(f)
            .unwrap()
            .label()
            .unwrap()
            .starts_with("Hold to move")
    );
    // Closing the dialog hands it back to the selected box.
    key(&mut r.shell, Key::Escape, 60);
    assert_eq!(
        focus_after(&mut r.shell, &mut tree, 70),
        AccessTree::box_node_id(selected)
    );
}

#[test]
fn the_screen_reader_focus_is_on_the_menu_row_the_keyboard_is_on() {
    use bs_ui::shell::access::AccessTree;
    let mut r = rig(BinStyle::Trash);
    select(&mut r.shell, &[0]);
    let mut tree = AccessTree::new();
    let selected = r.shell.app().unwrap().selected().unwrap();
    key(&mut r.shell, Key::ContextMenu, 20);
    let enabled: Vec<String> = r
        .shell
        .menu_items()
        .into_iter()
        .filter(|r| r.2)
        .map(|r| r.0)
        .collect();
    let f = focus_after(&mut r.shell, &mut tree, 30);
    assert_eq!(tree.get(f).unwrap().label(), Some(enabled[0].as_str()));
    key(&mut r.shell, Key::Arrow(bs_ui::Direction::Down), 40);
    let f = focus_after(&mut r.shell, &mut tree, 50);
    assert_eq!(tree.get(f).unwrap().label(), Some(enabled[1].as_str()));
    key(&mut r.shell, Key::Escape, 60);
    assert_eq!(
        focus_after(&mut r.shell, &mut tree, 70),
        AccessTree::box_node_id(selected)
    );
}

#[test]
fn the_screen_reader_focus_is_on_the_toast_button_when_tab_reached_it() {
    use bs_ui::shell::access::{AccessTarget, AccessTree};
    let mut r = rig(BinStyle::Trash);
    delete_first_folder(&mut r, 20);
    let mut tree = AccessTree::new();
    key(&mut r.shell, Key::Tab, 510);
    let f = focus_after(&mut r.shell, &mut tree, 520);
    assert_eq!(Some(f), tree.node_for(AccessTarget::ToastUndo));
    key(&mut r.shell, Key::Tab, 530);
    let f = focus_after(&mut r.shell, &mut tree, 540);
    assert_ne!(Some(f), tree.node_for(AccessTarget::ToastUndo));
}

// ---- 19. many protected notes ---------------------------------------------------------

fn drawn_lines(shell: &Shell<TreeSource>) -> Vec<String> {
    shell
        .popup()
        .expect("a dialog")
        .card
        .parts
        .iter()
        .filter_map(|p| match p {
            bs_render::CardPart::Line { text, .. } => Some(text.to_string()),
            _ => None,
        })
        .collect()
}

#[test]
fn the_same_note_on_many_items_is_said_once() {
    const SAME: &str = "Kept by the system for its own use.";
    let mut r = rig_with(names_for(8), true, BinStyle::Trash, 1.0);
    let n = folders(&r.shell).len().min(8);
    assert!(n >= 3, "{n} folders are laid out");
    for i in 1..=8 {
        r.fake.rule(&format!("folder{i}"), Rule::Protected(SAME));
    }
    let idx: Vec<usize> = (0..n).collect();
    select(&mut r.shell, &idx);
    key(&mut r.shell, Key::DeletePermanently, 20);
    let lines = drawn_lines(&r.shell);
    assert_eq!(lines.iter().filter(|l| *l == SAME).count(), 1, "{lines:?}");
    assert!(
        lines.iter().any(|l| l == "This can't be undone."),
        "{lines:?}"
    );
    assert!(
        lines.iter().any(|l| l.starts_with("Hold the button")),
        "{lines:?}"
    );
}

#[test]
fn a_long_list_of_notes_is_capped_and_the_warning_and_hold_stay() {
    const NOTES: [&str; 8] = [
        "Note one: the system keeps this.",
        "Note two: an app keeps this.",
        "Note three: another app keeps it.",
        "Note four: the system again.",
        "Note five: a service keeps it.",
        "Note six: kept for the user.",
        "Note seven: kept for updates.",
        "Note eight: kept for drivers.",
    ];
    let mut r = rig_with(names_for(8), true, BinStyle::Trash, 1.0);
    let n = folders(&r.shell).len().min(8);
    assert!(n >= 4, "{n} folders are laid out");
    for (i, note) in NOTES.iter().enumerate() {
        r.fake
            .rule(&format!("folder{}", i + 1), Rule::Protected(note));
    }
    let idx: Vec<usize> = (0..n).collect();
    select(&mut r.shell, &idx);
    key(&mut r.shell, Key::DeletePermanently, 20);
    let lines = drawn_lines(&r.shell);
    let shown = lines.iter().filter(|l| l.starts_with("Note ")).count();
    assert_eq!(shown, 2, "{lines:?}");
    assert!(
        lines.iter().any(|l| *l == format!("and {} more", n - 2)),
        "{lines:?}"
    );
    assert!(
        lines.iter().any(|l| l == "This can't be undone."),
        "{lines:?}"
    );
    assert!(
        lines.iter().any(|l| l.starts_with("Hold the button")),
        "{lines:?}"
    );
}
