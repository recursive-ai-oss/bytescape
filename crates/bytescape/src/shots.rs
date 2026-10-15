//! Pictures of the drive picker, a drive's map with its blocks, the progress
//! overlay, the stopped banner, and the delete flows (toast, dialogs, menus,
//! deleted tiles). They are made from fixture drives and a fixture tree,
//! through the same shell and renderer the window uses, so no disk is scanned
//! and nothing is deleted.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bs_knowledge::Level;
use bs_layout::Metrics;
use bs_platform::delete::{
    ByteBasis, Decision, EmptyOutcome, Item as DItem, Plan, PlanEntry, ProtectedConfirmation,
    Reason, RefuseReason, RestoreOutcome,
};
use bs_platform::drives::{Capacity, Drive, DriveId, DriveKind, ReservedKind};
use bs_render::{ItemKind, TextEngine};
use bs_tree::{Aggregator, Batch, Category, DeleteState, DirKey, Meta, NodeKind, Snapshot};
use bs_ui::blocks::DriveBlocks;
use bs_ui::capacity::Breakdown;
use bs_ui::notice::{AboutInfo, CrashNotice};
use bs_ui::owners::{Owners, Ownership, PackageInfo, ProgramInfo};
use bs_ui::picker::{CardScan, Picker};
use bs_ui::shell::{
    ActOutcome, Capabilities, Disposer, Look, STOPPED_BANNER, ScanReadout, Shell, ShellEffect,
    Token, TreeChange,
};
use bs_ui::tree::TreeSource;
use bs_ui::{App, InputEvent, Key, Modifiers, ViewSource};

use crate::window::{make_renderer, refresh_shell_chrome};
use crate::{ShotOptions, font_px};

/// The states this module draws.
pub fn is_picker_state(state: &str) -> bool {
    matches!(
        state,
        "picker"
            | "driveblocks"
            | "stopped"
            | "progress"
            | "picker-empty"
            | "toast"
            | "deleted"
            | "dialog-permanent"
            | "dialog-hold"
            | "menu"
            | "menu-blocked"
            | "menu-pending"
            | "dialog-notes"
            | "crash-banner"
            | "crash-report"
            | "crash-report-paths"
            | "crash-narrow"
            | "about"
            | "app-menu"
            | "card-program"
            | "card-package"
            | "card-package-nosoftware"
            | "card-sharing"
    )
}

const GB: u64 = 1_000_000_000;

fn drive(
    mount: &str,
    label: Option<&str>,
    kind: DriveKind,
    fs: &str,
    total: u64,
    used: u64,
    auto: bool,
) -> Drive {
    Drive {
        id: DriveId::new(mount),
        mount_point: PathBuf::from(mount),
        label: label.map(str::to_string),
        fs_type: fs.into(),
        kind,
        read_only: false,
        eligible_for_background_scan: auto,
        capacity: Some(Capacity::from_bytes(
            total,
            total - used,
            total - used - total / 20,
            ReservedKind::FilesystemReserve,
        )),
        capacity_problem: None,
        mounts: Vec::new(),
    }
}

/// Fixture drives: two internal ones being scanned and a memory stick.
pub fn fixture_drives() -> Vec<Drive> {
    let (a, b, c, fs) = if cfg!(windows) {
        ("C:\\", "D:\\", "E:\\", "NTFS")
    } else {
        ("/", "/data", "/media/usb", "ext4")
    };
    vec![
        drive(
            a,
            Some("System"),
            DriveKind::Fixed,
            fs,
            1_000 * GB,
            640 * GB,
            true,
        ),
        drive(
            b,
            Some("Projects"),
            DriveKind::Fixed,
            fs,
            500 * GB,
            310 * GB,
            true,
        ),
        drive(
            c,
            Some("SD CARD"),
            DriveKind::Removable,
            "exfat",
            64 * GB,
            21 * GB,
            false,
        ),
    ]
}

fn meta(size: u64, category: u8) -> Meta {
    Meta {
        logical: size,
        allocated: size,
        category: Category(category),
        mtime: 1_709_596_800,
        ..Meta::default()
    }
}

const FOLDERS: [(&str, u64, u8, u32); 7] = [
    ("Videos", 180 * GB, 1, 40),
    ("Games", 140 * GB, 6, 60),
    ("Pictures", 70 * GB, 2, 120),
    ("Projects", 52 * GB, 7, 200),
    ("Music", 31 * GB, 3, 90),
    ("Documents", 18 * GB, 4, 80),
    ("Backups", 66 * GB, 5, 12),
];

/// A drive's tree: a few big folders full of files. `finish` marks every
/// listing done.
fn fixture_agg(finish: bool, stopped: bool) -> (Aggregator, Arc<Snapshot>) {
    let mut agg = Aggregator::new();
    let mut b = Batch::new();
    let _ = b.dir(DirKey(0), DirKey::NONE, b"/", &meta(4096, 0));
    for (i, (name, total, category, files)) in FOLDERS.iter().enumerate() {
        let key = DirKey(i as u32 + 1);
        let _ = b.dir(key, DirKey(0), name.as_bytes(), &meta(4096, 0));
        for f in 0..*files {
            // Sizes fall off so the folder has a few big files and many small
            // ones.
            let weight = 1.0 / (f64::from(f) + 1.5);
            let size = ((*total as f64) * weight / 4.0) as u64 + 1;
            let _ = b.file(
                NodeKind::File,
                key,
                format!("{name}-{f:03}.dat").as_bytes(),
                &meta(size.min(*total / 3), *category),
            );
        }
        if finish {
            b.listing_done(key);
        }
    }
    if finish {
        b.listing_done(DirKey(0));
    }
    agg.apply(&b);
    if stopped {
        agg.mark_stopped();
    }
    let snapshot = agg.publish();
    (agg, snapshot)
}

fn shell_for(opts: &ShotOptions, dark: bool, text: &TextEngine) -> Shell<TreeSource> {
    let mut shell = Shell::new(
        opts.size,
        opts.scale,
        font_px(),
        text.clone(),
        Look { dark, accent: None },
    );
    shell.set_reduced_motion(true);
    shell
}

fn map_shell(
    opts: &ShotOptions,
    dark: bool,
    text: &TextEngine,
    finish: bool,
    stopped: bool,
) -> Shell<TreeSource> {
    map_shell_with(opts, dark, text, finish, stopped).0
}

fn map_shell_with(
    opts: &ShotOptions,
    dark: bool,
    text: &TextEngine,
    finish: bool,
    stopped: bool,
) -> (Shell<TreeSource>, Aggregator) {
    let mut shell = shell_for(opts, dark, text);
    let (agg, snapshot) = fixture_agg(finish, stopped);
    let drive = &fixture_drives()[0];
    let cap = drive.capacity.unwrap_or_else(|| {
        Capacity::from_bytes(GB, GB / 2, GB / 2, ReservedKind::FilesystemReserve)
    });
    let counted = snapshot.root().map_or(0, |r| r.allocated());
    let breakdown = Breakdown::new(&cap, counted, finish, 14_100_000_000);
    let mut source = TreeSource::new(snapshot, Metrics::from_logical(font_px(), opts.scale))
        .expect("fixture tree has a root");
    source.set_drive_blocks(Some(DriveBlocks::from(&breakdown)));
    let map = shell.map_rect();
    shell.set_app(App::new(source, map));
    shell.set_capacity(breakdown, Duration::ZERO);
    shell.set_crumb(drive.mount_point.to_string_lossy().into_owned());
    (shell, agg)
}

// ---- the delete flows --------------------------------------------------------

/// A stand-in for the platform that deletes nothing: it judges folders by
/// name (Backups stands for part of the system, Documents is protected) and
/// answers as a bin would.
struct ShotDisposer {
    bytes: HashMap<String, u64>,
    /// More folders are protected, each with a note of its own.
    many_notes: bool,
}

/// Notes for the folders that are protected in the many-notes picture.
const OTHER_NOTES: [(&str, &str); 3] = [
    (
        "Music",
        "Your music library. Media players keep their database beside it.",
    ),
    (
        "Pictures",
        "Your photos. The photo apps of the system index them here.",
    ),
    (
        "Projects",
        "Your work folders. Sync tools and editors keep state in them.",
    ),
];

/// What a protected personal folder says, in the words of the system the
/// picture is made on.
const DOCUMENTS_NOTE: &str = if cfg!(windows) {
    "Your personal files. Windows and many programs keep things here."
} else {
    "Your personal files. The desktop and many programs keep things here."
};

impl ShotDisposer {
    fn name(item: &DItem) -> String {
        item.path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    }
}

impl Disposer for ShotDisposer {
    fn precheck(
        &self,
        items: &[DItem],
        _permanent: bool,
        confirmed: Option<&ProtectedConfirmation>,
    ) -> Plan {
        let entries: Vec<PlanEntry> = items
            .iter()
            .map(|i| match Self::name(i).as_str() {
                name if self.many_notes && OTHER_NOTES.iter().any(|(n, _)| *n == name) => {
                    let note = OTHER_NOTES
                        .iter()
                        .find(|(n, _)| *n == name)
                        .map_or("", |(_, note)| *note);
                    PlanEntry {
                        level: Level::Protected,
                        reason: Reason::Table,
                        note: Some(note),
                        decision: if confirmed.is_some() {
                            Decision::Proceed
                        } else {
                            Decision::Refused(RefuseReason::ProtectedNotConfirmed {
                                note: Some(note),
                            })
                        },
                    }
                }
                "Backups" => PlanEntry {
                    level: Level::Blocked,
                    reason: Reason::Table,
                    note: None,
                    decision: Decision::Refused(RefuseReason::Blocked {
                        reason: Reason::Table,
                        note: None,
                    }),
                },
                "Documents" => PlanEntry {
                    level: Level::Protected,
                    reason: Reason::Table,
                    note: Some(DOCUMENTS_NOTE),
                    decision: if confirmed.is_some() {
                        Decision::Proceed
                    } else {
                        Decision::Refused(RefuseReason::ProtectedNotConfirmed {
                            note: Some(DOCUMENTS_NOTE),
                        })
                    },
                },
                _ => PlanEntry {
                    level: Level::Normal,
                    reason: Reason::None,
                    note: None,
                    decision: Decision::Proceed,
                },
            })
            .collect();
        let strictest = entries
            .iter()
            .map(|e| e.level)
            .max()
            .unwrap_or(Level::Normal);
        Plan {
            selection_refused: entries
                .iter()
                .any(|e| matches!(e.decision, Decision::Refused(_))),
            drivers: (0..entries.len())
                .filter(|&i| entries[i].level == strictest)
                .collect(),
            entries,
            strictest,
        }
    }

    fn trash(
        &self,
        items: &[DItem],
        _confirmed: Option<&ProtectedConfirmation>,
    ) -> Vec<ActOutcome> {
        items
            .iter()
            .map(|i| ActOutcome::MovedToBin {
                token: Token::new(i.path.clone()),
                bytes: self.bytes.get(&Self::name(i)).copied().unwrap_or(0),
                basis: ByteBasis::Allocated,
                shared_files: 0,
            })
            .collect()
    }

    fn delete_permanently(
        &self,
        items: &[DItem],
        _confirmed: Option<&ProtectedConfirmation>,
    ) -> Vec<ActOutcome> {
        items
            .iter()
            .map(|i| ActOutcome::Deleted {
                bytes: self.bytes.get(&Self::name(i)).copied().unwrap_or(0),
                shared_files: 0,
            })
            .collect()
    }

    fn restore(&self, token: &Token) -> RestoreOutcome {
        RestoreOutcome::Restored {
            path: token.get::<PathBuf>().cloned().unwrap_or_default(),
        }
    }

    fn empty_bin(&self, _mount: &Path) -> EmptyOutcome {
        EmptyOutcome::Emptied {
            bytes: 0,
            entries: 0,
        }
    }
}

/// The point on the title bar of the folder called `name`.
fn folder_point(shell: &Shell<TreeSource>, name: &str) -> Option<(f32, f32)> {
    shell
        .app()?
        .items()
        .iter()
        .find(|i| {
            i.depth == 1
                && i.kind == ItemKind::FolderTitled
                && i.label.as_ref().is_some_and(|l| &*l.name == name)
        })
        .map(|i| (i.rect.x + i.rect.w / 2.0, i.rect.y + 3.0))
}

fn at(ms: u64) -> Duration {
    Duration::from_millis(ms)
}

/// Does the disk work and the tree changes the shell asked for, as the
/// window would.
fn settle(
    shell: &mut Shell<TreeSource>,
    agg: &mut Aggregator,
    fx: Vec<ShellEffect>,
    now: Duration,
) {
    let mut queue = fx;
    while let Some(effect) = queue.pop() {
        match effect {
            ShellEffect::Run(job) => queue.extend(shell.job_done(job.run(), now)),
            ShellEffect::Tree(changes) => {
                apply_changes(shell, agg, &changes, now);
            }
            _ => {}
        }
    }
}

fn apply_changes(
    shell: &mut Shell<TreeSource>,
    agg: &mut Aggregator,
    changes: &[TreeChange],
    now: Duration,
) {
    for c in changes {
        let _ = agg.unmark_deleted(c.node);
        if c.to != DeleteState::None {
            let _ = agg.mark_deleted(c.node, c.to);
        }
    }
    let snapshot = agg.publish();
    if let Some(app) = shell.app_mut() {
        app.source_mut().set_snapshot(snapshot);
    }
    shell.update(InputEvent::DataChanged, now);
}

/// Builds the scene for a delete-flow picture. Returns the shell, the time to
/// draw at, and the map's items.
fn actions_scene(
    opts: &ShotOptions,
    dark: bool,
    text: &TextEngine,
    state: &str,
) -> Result<(Shell<TreeSource>, Duration), String> {
    let (mut shell, mut agg) = map_shell_with(opts, dark, text, true, false);
    shell.set_status("640 GB in 1.2 million files".into(), false);
    shell.set_capabilities(Capabilities { properties: true });
    let bytes: HashMap<String, u64> = shell
        .app()
        .map(|a| {
            let view = a.current_view();
            a.items()
                .iter()
                .filter(|i| i.depth == 1 && i.kind == ItemKind::FolderTitled)
                .filter_map(|i| {
                    let t = a.source().target(view, i.id)?;
                    Some((t.name, t.bytes))
                })
                .collect()
        })
        .ok_or("the fixture has no folders")?;
    shell.set_disposer(Arc::new(ShotDisposer {
        bytes,
        many_notes: state == "dialog-notes",
    }));
    let point = |shell: &Shell<TreeSource>, name: &str| {
        folder_point(shell, name).ok_or_else(|| format!("no folder called {name}"))
    };
    let ctrl = Modifiers {
        ctrl: true,
        shift: false,
    };
    let mut now = at(100);
    match state {
        "toast" | "deleted" => {
            let (x, y) = point(&shell, "Videos")?;
            shell.update(InputEvent::Click { x, y }, now);
            now = at(200);
            let fx = shell.update(InputEvent::Key(Key::Delete), now);
            settle(&mut shell, &mut agg, fx, now);
            // A second item, deleted for good, so both looks show.
            if state == "deleted" {
                let (x, y) = point(&shell, "Pictures")?;
                now = at(300);
                shell.update(InputEvent::Click { x, y }, now);
                let fx = shell.update(InputEvent::Key(Key::DeletePermanently), now);
                settle(&mut shell, &mut agg, fx, now);
                if let Some(p) = shell.popup() {
                    let button = p
                        .card
                        .parts
                        .iter()
                        .find_map(|part| match part {
                            bs_render::CardPart::Button { rect, label, .. }
                                if &**label == "Delete permanently" =>
                            {
                                Some((rect.x + rect.w / 2.0, rect.y + rect.h / 2.0))
                            }
                            _ => None,
                        })
                        .ok_or("no confirm button")?;
                    let fx = shell.update(
                        InputEvent::Click {
                            x: button.0,
                            y: button.1,
                        },
                        now,
                    );
                    settle(&mut shell, &mut agg, fx, now);
                }
                // The toast has gone by the time the picture is taken.
                now = at(20_000);
                shell.tick(now);
                shell.tick(at(20_300));
                now = at(20_400);
            } else {
                now = at(900);
                shell.tick(now);
            }
        }
        "dialog-permanent" => {
            let (x, y) = point(&shell, "Games")?;
            shell.update(InputEvent::Click { x, y }, now);
            let (x, y) = point(&shell, "Music")?;
            shell.update(InputEvent::ClickWith { x, y, mods: ctrl }, at(150));
            let (x, y) = point(&shell, "Projects")?;
            shell.update(InputEvent::ClickWith { x, y, mods: ctrl }, at(180));
            let fx = shell.update(InputEvent::Key(Key::DeletePermanently), at(200));
            settle(&mut shell, &mut agg, fx, at(200));
            now = at(300);
        }
        "dialog-notes" => {
            let (x, y) = point(&shell, "Documents")?;
            shell.update(InputEvent::Click { x, y }, now);
            for (n, name) in ["Music", "Pictures", "Projects"].into_iter().enumerate() {
                let (x, y) = point(&shell, name)?;
                shell.update(
                    InputEvent::ClickWith { x, y, mods: ctrl },
                    at(150 + 10 * n as u64),
                );
            }
            let fx = shell.update(InputEvent::Key(Key::DeletePermanently), at(200));
            settle(&mut shell, &mut agg, fx, at(200));
            now = at(300);
        }
        "dialog-hold" => {
            let (x, y) = point(&shell, "Documents")?;
            shell.update(InputEvent::Click { x, y }, now);
            let fx = shell.update(InputEvent::Key(Key::Delete), at(200));
            settle(&mut shell, &mut agg, fx, at(200));
            let button = shell
                .popup()
                .and_then(|p| {
                    p.card.parts.iter().find_map(|part| match part {
                        bs_render::CardPart::Hold { rect, .. } => {
                            Some((rect.x + rect.w / 2.0, rect.y + rect.h / 2.0))
                        }
                        _ => None,
                    })
                })
                .ok_or("no hold button")?;
            shell.update(
                InputEvent::PointerDown {
                    x: button.0,
                    y: button.1,
                },
                at(1_000),
            );
            // Partway through the hold.
            now = at(1_900);
            shell.tick(now);
        }
        "menu" | "menu-blocked" | "menu-pending" => {
            let name = if state == "menu-blocked" {
                "Backups"
            } else {
                "Projects"
            };
            let (x, y) = point(&shell, name)?;
            let fx = shell.update(InputEvent::RightClick { x, y: y + 20.0 }, now);
            // The pending picture is taken before the platform has answered.
            if state != "menu-pending" {
                settle(&mut shell, &mut agg, fx, now);
            }
            now = at(200);
        }
        other => return Err(format!("no delete-flow picture called {other}")),
    }
    Ok((shell, now))
}

// ---- hand-offs, the crash note, About -----------------------------------------

/// A stand-in for the platform's lookup of which program or package owns a
/// folder, by the folder's name.
struct ShotOwners(HashMap<String, Ownership>);

impl Owners for ShotOwners {
    fn request(&self, _: &[(PathBuf, u64)]) {}

    fn lookup(&self, path: &Path) -> Ownership {
        path.file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| self.0.get(n))
            .cloned()
            .unwrap_or(Ownership::Unknown)
    }

    fn is_idle(&self) -> bool {
        true
    }
}

fn shot_owners(state: &str) -> Arc<ShotOwners> {
    let mut answers = HashMap::new();
    match state {
        "card-program" => {
            answers.insert(
                "Games".to_string(),
                Ownership::Program(ProgramInfo {
                    name: "Pixel Quest".into(),
                    publisher: Some("Lantern Studio".into()),
                    whole_folder: true,
                    can_uninstall: true,
                }),
            );
        }
        _ => {
            answers.insert(
                "Games".to_string(),
                Ownership::Package(PackageInfo {
                    name: "pixel-quest".into(),
                    manager: "dpkg",
                    other_packages: 2,
                    removal_command: Some("sudo apt remove pixel-quest".into()),
                    software_entry: true,
                }),
            );
        }
    }
    Arc::new(ShotOwners(answers))
}

/// A report as the platform writes one, for the crash pictures.
fn shot_notice() -> CrashNotice {
    let body = |home: &str| {
        format!(
            "Bytescape crash report\nFormat: 1\nProgram: bytescape 0.1.0\nBuilt: 2025-08-20\n\
             Target: x86_64-unknown-linux-gnu\nOperating system: Linux\nThread: main\nPanic:\n    \
             could not read {home}/Documents/taxes 2025.pdf: gone\nLocation: {home}/src/scan.rs:112:9\n\
             Recent actions:\n  scan finished\n  zoomed in\nBacktrace:\n   0: bytescape::scan::read\n             \
             at {home}/src/scan.rs:112:9\n   1: bytescape::window::pump\n             at {home}/src/window.rs:340:5\n   \
             2: std::rt::lang_start\n"
        )
    };
    let hidden = body("<path-1>")
        .replace("<path-1>/src/", "<source>/")
        .replace("<path-1>/Documents/taxes 2025.pdf", "<path-1>");
    CrashNotice::new(
        hidden,
        body("/home/alex"),
        bs_platform::crash::ISSUE_URL.into(),
    )
}

/// A tree with files that share data with a snapshot, for the sharing picture.
fn sharing_snapshot() -> Arc<Snapshot> {
    let mut agg = Aggregator::new();
    let mut b = Batch::new();
    let _ = b.dir(DirKey(0), DirKey::NONE, b"/", &meta(4096, 0));
    let _ = b.dir(DirKey(1), DirKey(0), b"Machines", &meta(4096, 0));
    let _ = b.dir(DirKey(2), DirKey(0), b"Archive", &meta(4096, 0));
    for (n, size) in [("builder.img", 64 * GB), ("test-bed.img", 40 * GB)] {
        let shared = Meta {
            flags: bs_tree::NodeFlags::SHARED,
            exclusive: size / 8,
            ..meta(size, 6)
        };
        let _ = b.file(NodeKind::File, DirKey(1), n.as_bytes(), &shared);
    }
    for f in 0..30u64 {
        let _ = b.file(
            NodeKind::File,
            DirKey(2),
            format!("old-{f:02}.tar").as_bytes(),
            &meta(GB + f * GB / 7, 5),
        );
    }
    b.listing_done(DirKey(1));
    b.listing_done(DirKey(2));
    b.listing_done(DirKey(0));
    agg.apply(&b);
    agg.publish()
}

/// Where the pointer goes to show the card of the box called `name`.
fn rest_on(shell: &mut Shell<TreeSource>, name: &str, now: Duration) -> Result<(), String> {
    let (x, y) = shell
        .app()
        .and_then(|a| {
            a.items()
                .iter()
                .find(|i| i.label.as_ref().is_some_and(|l| &*l.name == name))
                .map(|i| match i.kind {
                    ItemKind::FolderTitled => (i.rect.x + i.rect.w / 2.0, i.rect.y + 3.0),
                    _ => (i.rect.x + i.rect.w / 2.0, i.rect.y + i.rect.h / 2.0),
                })
        })
        .ok_or_else(|| format!("no box called {name}"))?;
    shell.update(InputEvent::PointerMoved { x, y }, now);
    shell.tick(now + Duration::from_secs(1));
    Ok(())
}

fn pill_point(shell: &mut Shell<TreeSource>, label: &str) -> Result<(f32, f32), String> {
    let c = shell.chrome(Duration::ZERO);
    c.widgets
        .iter()
        .find_map(|w| match w {
            bs_render::Widget::Pill { rect, text, .. } if &**text == label => {
                Some((rect.x + rect.w / 2.0, rect.y + rect.h / 2.0))
            }
            _ => None,
        })
        .ok_or_else(|| format!("no button {label}"))
}

/// Builds the scene for a hand-off, crash-note or About picture.
fn notes_scene(
    opts: &ShotOptions,
    dark: bool,
    text: &TextEngine,
    state: &str,
) -> Result<(Shell<TreeSource>, Duration), String> {
    let mut now = at(100);
    let mut shell = if state == "card-sharing" {
        let mut shell = shell_for(opts, dark, text);
        let source = TreeSource::new(
            sharing_snapshot(),
            Metrics::from_logical(font_px(), opts.scale),
        )
        .ok_or("the fixture tree has no root")?;
        let map = shell.map_rect();
        shell.set_app(App::new(source, map));
        shell
    } else {
        let (mut shell, _) = map_shell_with(opts, dark, text, true, false);
        if state.starts_with("card-") {
            // The source was made without owners; rebuild it with them.
            let (_, snapshot) = fixture_agg(true, false);
            let mut source =
                TreeSource::new(snapshot, Metrics::from_logical(font_px(), opts.scale))
                    .ok_or("the fixture tree has no root")?;
            source.set_owners(Some(shot_owners(state)));
            let map = shell.map_rect();
            shell.set_app(App::new(source, map));
        }
        shell
    };
    shell.set_status("640 GB in 1.2 million files".into(), false);
    shell.set_capabilities(Capabilities { properties: true });
    shell.set_software_center(state != "card-package-nosoftware");
    shell.set_about(AboutInfo::new(
        "0.1.0",
        "2025-08-20",
        "x86_64-unknown-linux-gnu",
    ));
    shell.set_unix_clock(Box::new(|| 1_792_454_400));
    match state {
        "crash-banner" | "crash-narrow" => {
            shell.set_crash_notice(Some(shot_notice()), now);
        }
        "crash-report" | "crash-report-paths" => {
            shell.set_crash_notice(Some(shot_notice()), now);
            let (x, y) = pill_point(&mut shell, "View report")?;
            shell.update(InputEvent::Click { x, y }, at(200));
            if state == "crash-report-paths" {
                let (x, y) = shell
                    .popup()
                    .and_then(|p| {
                        p.card.parts.iter().find_map(|part| match part {
                            bs_render::CardPart::Button { rect, label, .. }
                                if &**label == "Show paths" =>
                            {
                                Some((rect.x + rect.w / 2.0, rect.y + rect.h / 2.0))
                            }
                            _ => None,
                        })
                    })
                    .ok_or("no Show paths button")?;
                shell.update(InputEvent::Click { x, y }, at(300));
            }
            now = at(400);
        }
        "about" => {
            shell.update(InputEvent::Key(Key::About), at(200));
            now = at(300);
        }
        "app-menu" => {
            let status = shell.geometry().status;
            shell.update(
                InputEvent::RightClick {
                    x: status.x + status.w * 0.4,
                    y: status.y + status.h / 2.0,
                },
                at(200),
            );
            now = at(300);
        }
        "card-program" | "card-package" | "card-package-nosoftware" => {
            rest_on(&mut shell, "Games", at(200))?;
            now = at(1_300);
        }
        "card-sharing" => {
            rest_on(&mut shell, "builder.img", at(200))?;
            now = at(1_300);
        }
        other => return Err(format!("no picture called {other}")),
    }
    Ok((shell, now))
}

/// Pictures for the picker states.
pub fn screenshots(opts: &ShotOptions, states: &[String], text: &TextEngine) -> Result<(), String> {
    let scale = opts.scale;
    let scale_name = if scale.fract() == 0.0 {
        format!("{}", scale as u32)
    } else {
        format!("{scale}")
    };
    for state in states {
        for dark in [false, true] {
            let mut now = Duration::ZERO;
            let mut shell = match state.as_str() {
                "toast" | "deleted" | "dialog-permanent" | "dialog-hold" | "dialog-notes"
                | "menu" | "menu-blocked" | "menu-pending" => {
                    let (shell, at) = actions_scene(opts, dark, text, state)?;
                    now = at;
                    shell
                }
                "crash-banner"
                | "crash-report"
                | "crash-report-paths"
                | "crash-narrow"
                | "about"
                | "app-menu"
                | "card-program"
                | "card-package"
                | "card-package-nosoftware"
                | "card-sharing" => {
                    let (shell, at) = notes_scene(opts, dark, text, state)?;
                    now = at;
                    shell
                }
                "picker" | "picker-empty" => {
                    let mut shell = shell_for(opts, dark, text);
                    let drives = if state == "picker" {
                        fixture_drives()
                    } else {
                        Vec::new()
                    };
                    let mut picker = Picker::new(&drives);
                    // Mid-scan: one drive well along, one just started.
                    if let Some(c) = picker.card_mut(0) {
                        c.scan = CardScan::Running {
                            ratio: Some(0.62),
                            files: 1_204_511,
                            bytes: 397 * GB,
                        };
                        c.counted = 397 * GB;
                        c.bin = 14_100_000_000;
                    }
                    if let Some(c) = picker.card_mut(1) {
                        c.scan = CardScan::Running {
                            ratio: Some(0.18),
                            files: 212_840,
                            bytes: 56 * GB,
                        };
                        c.counted = 56 * GB;
                    }
                    shell.set_crumb("Drives".into());
                    shell.set_status(
                        "Pick a drive. Your internal drives are already being scanned.".into(),
                        false,
                    );
                    shell.set_picker(picker);
                    shell
                }
                "driveblocks" => {
                    let mut shell = map_shell(opts, dark, text, true, false);
                    shell.set_status("640 GB in 1.2 million files".into(), false);
                    shell
                }
                "stopped" => {
                    let mut shell = map_shell(opts, dark, text, false, true);
                    shell.set_status(STOPPED_BANNER.into(), false);
                    if shell.set_stopped(true) {
                        let map = shell.map_rect();
                        if let Some(app) = shell.app_mut() {
                            app.update(InputEvent::Resized { map }, Duration::ZERO);
                        }
                    }
                    shell
                }
                _ => {
                    let mut shell = map_shell(opts, dark, text, false, false);
                    shell.set_status("Scanning… 62% · ≥ 397 GB in 1.2 million files".into(), true);
                    shell.set_readout(Some(ScanReadout {
                        ratio: Some(0.62),
                        files: 1_204_511,
                        files_per_sec: 148_000.0,
                        elapsed: Duration::from_secs(8),
                    }));
                    shell
                }
            };
            if state == "picker"
                && let Some(items) = shell.picker_items()
                && let Some(it) = items.iter().find(|i| i.id == 2)
            {
                // The pointer rests on the second card.
                let (x, y) = (it.rect.x + it.rect.w / 2.0, it.rect.y + it.rect.h / 2.0);
                shell.update(InputEvent::PointerMoved { x, y }, Duration::ZERO);
            }
            let mut renderer = make_renderer(opts.size, scale, dark, text.clone())?;
            let items = match shell.picker_items() {
                Some(items) => items,
                None => shell.app().map(App::items).ok_or("nothing to draw")?,
            };
            renderer.set_items(items);
            refresh_shell_chrome(&mut renderer, &mut shell, now);
            let name = format!(
                "{state}-{}-{scale_name}x.png",
                if dark { "dark" } else { "light" }
            );
            let file = Path::new(&opts.dir).join(&name);
            renderer.save_png(&file)?;
            println!("wrote {}", file.display());
        }
    }
    Ok(())
}
