//! Pictures of the drive picker, a drive's map with its blocks, the progress
//! overlay and the stopped banner. They are made from fixture drives and a
//! fixture tree, through the same shell and renderer the window uses, so no
//! disk is scanned.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bs_layout::Metrics;
use bs_platform::drives::{Capacity, Drive, DriveId, DriveKind, ReservedKind};
use bs_render::TextEngine;
use bs_tree::{Aggregator, Batch, Category, DirKey, Meta, NodeKind, Snapshot};
use bs_ui::blocks::DriveBlocks;
use bs_ui::capacity::Breakdown;
use bs_ui::picker::{CardScan, Picker};
use bs_ui::shell::{Look, STOPPED_BANNER, ScanReadout, Shell};
use bs_ui::tree::TreeSource;
use bs_ui::{App, InputEvent};

use crate::window::{make_renderer, refresh_shell_chrome};
use crate::{ShotOptions, font_px};

/// The states this module draws.
pub fn is_picker_state(state: &str) -> bool {
    matches!(
        state,
        "picker" | "driveblocks" | "stopped" | "progress" | "picker-empty"
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

/// A drive's tree: a few big folders full of files. `finish` marks every
/// listing done.
fn fixture_tree(finish: bool, stopped: bool) -> Arc<Snapshot> {
    let folders: [(&str, u64, u8, u32); 7] = [
        ("Videos", 180 * GB, 1, 40),
        ("Games", 140 * GB, 6, 60),
        ("Pictures", 70 * GB, 2, 120),
        ("Projects", 52 * GB, 7, 200),
        ("Music", 31 * GB, 3, 90),
        ("Documents", 18 * GB, 4, 80),
        ("Backups", 66 * GB, 5, 12),
    ];
    let mut agg = Aggregator::new();
    let mut b = Batch::new();
    let _ = b.dir(DirKey(0), DirKey::NONE, b"/", &meta(4096, 0));
    for (i, (name, total, category, files)) in folders.iter().enumerate() {
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
    agg.publish()
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
    let mut shell = shell_for(opts, dark, text);
    let snapshot = fixture_tree(finish, stopped);
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
    shell
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
            let mut shell = match state.as_str() {
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
            refresh_shell_chrome(&mut renderer, &mut shell, Duration::ZERO);
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
