//! Real scans in the benchmark and picture paths: the same renderer and input
//! core as the window, fed by a finished (or half-finished) scan.

use std::path::Path;
use std::time::Instant;

use bs_layout::Metrics;
use bs_render::{ItemKind, Rect, Renderer, TextEngine};
use bs_ui::tree::{TreeSource, scan_status};
use bs_ui::{App, ViewId, ViewSource};

use crate::bench::Script;
use crate::live::{scan_to_end, scan_until};
use crate::window::{ChromeSpec, drive_breakdown, make_renderer, static_chrome};
use crate::{ShotOptions, ZoomOptions, font_px};

/// How many of the biggest folders are laid out to find the busiest view.
const CANDIDATES: usize = 16;

fn center(r: Rect) -> (f32, f32) {
    (r.x + r.w / 2.0, r.y + r.h / 2.0)
}

/// Scans the folder, lays out its biggest views and prepares the zoom script:
/// the busiest child view is the warm target, the other nested folders are the
/// cold targets. Prints what the scene is, with the box counts of the views.
pub fn prepare_zoom(
    opts: &ZoomOptions,
    path: &Path,
    size: (u32, u32),
    text: &TextEngine,
) -> Result<(App<TreeSource>, Renderer, Script), String> {
    let tag = &opts.tag;
    let started = Instant::now();
    let (snapshot, stats) = scan_to_end(path.to_path_buf())?;
    let scan_ms = started.elapsed().as_secs_f64() * 1000.0;
    let (chrome, map) = static_chrome(
        text,
        ChromeSpec {
            size,
            scale: opts.scale,
            dark: opts.dark,
            crumb: &path.to_string_lossy(),
            status: &scan_status(&snapshot),
            hot: None,
            capacity: drive_breakdown(path, &snapshot),
        },
    );
    let metrics = Metrics::from_logical(font_px(), opts.scale);
    let mut source = TreeSource::new(snapshot.clone(), metrics).ok_or("the scan found nothing")?;

    let root_view = source.root();
    let t = Instant::now();
    let root_layout = source.layout(root_view, map);
    let root_layout_ms = t.elapsed().as_secs_f64() * 1000.0;
    let mut folders: Vec<(u32, ViewId, Rect)> = root_layout
        .folders
        .iter()
        .filter_map(|&(id, view)| {
            let it = root_layout.items.iter().find(|i| i.id == id)?;
            (it.depth == 1 && it.kind == ItemKind::FolderTitled).then_some((id, view, it.rect))
        })
        .collect();
    folders.sort_by(|a, b| (b.2.w * b.2.h).total_cmp(&(a.2.w * a.2.h)));
    if folders.is_empty() {
        return Err("the scan has no nested folder big enough to zoom into".into());
    }
    let mut best: Option<(usize, usize, f64)> = None;
    for (i, (_, view, _)) in folders.iter().take(CANDIDATES).enumerate() {
        let t = Instant::now();
        let n = source.layout(*view, map).items.len();
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        if best.is_none_or(|(_, bn, _)| n > bn) {
            best = Some((i, n, ms));
        }
    }
    let (best_i, best_boxes, best_ms) = best.ok_or("no candidate views")?;
    let aim = center(folders[best_i].2);
    let cold_aims: Vec<(f32, f32)> = folders
        .iter()
        .enumerate()
        .filter(|&(i, _)| i != best_i)
        .map(|(_, f)| center(f.2))
        .collect();

    println!("{tag}.scene.kind=real scan of {}", path.display());
    println!("{tag}.scene.entries={}", snapshot.len());
    println!(
        "{tag}.scene.files={}",
        snapshot.root().map_or(0, |r| r.file_count())
    );
    println!("{tag}.scene.scan_ms={scan_ms:.0}");
    println!("{tag}.scene.scan_threads={}", stats.threads);
    println!("{tag}.scene.font_px={}", font_px());
    println!("{tag}.scene.boxes_root_view={}", root_layout.items.len());
    println!("{tag}.scene.boxes_child_view={best_boxes}");
    println!(
        "{tag}.scene.max_view_boxes={}",
        root_layout.items.len().max(best_boxes)
    );
    println!("{tag}.scene.root_layout_ms={root_layout_ms:.1}");
    println!("{tag}.scene.child_layout_ms={best_ms:.1}");
    println!("{tag}.scene.cold_folders_available={}", cold_aims.len());

    let mut renderer = make_renderer(size, opts.scale, opts.dark, text.clone())?;
    renderer.set_chrome(chrome);
    let app = App::new(source, map);
    Ok((
        app,
        renderer,
        Script {
            cycles: opts.cycles,
            cold_cycles: opts.cold_cycles,
            hover_moves: opts.hover_moves,
            aim,
            cold_aims,
            paced: opts.paced,
        },
    ))
}

/// Pictures of a real scan: `midscan` (taken while the scan is still running,
/// so folders still say "Scanning…") and `complete`.
pub fn screenshots(opts: &ShotOptions, states: &[String], text: &TextEngine) -> Result<(), String> {
    let Some(path) = opts.path.clone() else {
        return Err("the midscan and complete states need a folder to scan".into());
    };
    let scale = opts.scale;
    let scale_name = if scale.fract() == 0.0 {
        format!("{}", scale as u32)
    } else {
        format!("{scale}")
    };
    let metrics = Metrics::from_logical(font_px(), scale);
    for state in states {
        let (snapshot, driver) = match state.as_str() {
            "midscan" => {
                let (s, d) = scan_until(path.clone(), opts.midscan_files)?;
                (s, Some(d))
            }
            _ => (scan_to_end(path.clone())?.0, None),
        };
        let status = scan_status(&snapshot);
        for dark in [false, true] {
            let abs = std::path::absolute(&path).unwrap_or_else(|_| path.clone());
            let (chrome, map) = static_chrome(
                text,
                ChromeSpec {
                    size: opts.size,
                    scale,
                    dark,
                    crumb: &abs.to_string_lossy(),
                    status: &status,
                    hot: None,
                    capacity: drive_breakdown(&path, &snapshot),
                },
            );
            let mut source =
                TreeSource::new(snapshot.clone(), metrics).ok_or("the scan found nothing")?;
            let root = source.root();
            let layout = source.layout(root, map);
            println!(
                "{state}: {} boxes, {} entries counted, {status}",
                layout.items.len(),
                snapshot.len()
            );
            let mut renderer = make_renderer(opts.size, scale, dark, text.clone())?;
            renderer.set_chrome(chrome);
            renderer.set_items(layout.items);
            let name = format!(
                "{state}-{}-{scale_name}x.png",
                if dark { "dark" } else { "light" }
            );
            let file = Path::new(&opts.dir).join(&name);
            renderer.save_png(&file)?;
            println!("wrote {}", file.display());
        }
        if let Some(d) = driver {
            d.cancel();
        }
    }
    Ok(())
}
