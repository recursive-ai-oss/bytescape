//! Real scans in the benchmark and picture paths: the same renderer and input
//! core as the window, fed by a finished (or half-finished) scan.

use std::path::Path;
use std::time::Instant;

use std::time::Duration;

use bs_layout::Metrics;
use bs_render::{AnyRenderer, ItemKind, Rect, TextEngine};
use bs_ui::shell::{Look, Shell, ShellEffect};
use bs_ui::tree::{TreeSource, scan_status};
use bs_ui::{App, Direction, InputEvent, Key, ViewId, ViewSource};

use crate::bench::Script;
use crate::live::{scan_to_end, scan_until};
use crate::window::{ChromeSpec, apply, drive_breakdown, make_renderer, static_chrome};
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
) -> Result<(App<TreeSource>, AnyRenderer, Script), String> {
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

/// The navigation pictures: the hover card in the middle and at each edge, a
/// frame in the middle of a zoom, a keyboard selection and a deep breadcrumb.
pub fn is_nav_state(state: &str) -> bool {
    state.starts_with("card") || matches!(state, "midzoom" | "keysel" | "crumb")
}

fn apply_shell_effects(renderer: &mut AnyRenderer, fx: Vec<ShellEffect>, now: Duration) {
    for e in fx {
        match e {
            ShellEffect::Map(e) => {
                apply(renderer, vec![e], now);
            }
            ShellEffect::Dim(c) => renderer.set_dim(c),
            _ => {}
        }
    }
}

/// Where the pointer goes for a `card*` picture, as a fraction of the map.
fn card_point(state: &str) -> (f32, f32) {
    match state {
        "card-left" => (0.016, 0.5),
        "card-right" => (0.988, 0.55),
        "card-top" => (0.5, 0.045),
        "card-bottom" => (0.5, 0.985),
        "card-corner" => (0.985, 0.985),
        _ => (0.5, 0.5),
    }
}

/// Renders the navigation states of a real scan through the shell, the same
/// way the window drives it.
pub fn nav_screenshots(
    opts: &ShotOptions,
    states: &[String],
    text: &TextEngine,
) -> Result<(), String> {
    let Some(path) = opts.path.clone() else {
        return Err("the navigation states need a folder to scan".into());
    };
    let scale = opts.scale;
    let scale_name = if scale.fract() == 0.0 {
        format!("{}", scale as u32)
    } else {
        format!("{scale}")
    };
    let metrics = Metrics::from_logical(font_px(), scale);
    let (snapshot, _) = scan_to_end(path.clone())?;
    let abs = std::path::absolute(&path).unwrap_or_else(|_| path.clone());
    let ms = Duration::from_millis;
    for state in states {
        for dark in [false, true] {
            let mut shell: Shell<TreeSource> = Shell::new(
                opts.size,
                scale,
                font_px(),
                text.clone(),
                Look { dark, accent: None },
            );
            shell.set_crumb(abs.to_string_lossy().into_owned());
            shell.set_status(scan_status(&snapshot), false);
            if let Some(b) = drive_breakdown(&path, &snapshot) {
                shell.set_capacity(b, Duration::ZERO);
            }
            let map = shell.map_rect();
            let source =
                TreeSource::new(snapshot.clone(), metrics).ok_or("the scan found nothing")?;
            shell.set_app(App::new(source, map));
            let mut renderer = make_renderer(opts.size, scale, dark, text.clone())?;
            renderer.set_items(shell.app().map(App::items).unwrap_or_default());
            let mut clock = Duration::ZERO;
            let mut mid_zoom = false;
            match state.as_str() {
                "keysel" => {
                    for dir in [
                        Direction::Right,
                        Direction::Right,
                        Direction::Down,
                        Direction::Left,
                    ] {
                        clock += ms(50);
                        let fx = shell.update(InputEvent::Key(Key::Arrow(dir)), clock);
                        apply_shell_effects(&mut renderer, fx, clock);
                    }
                }
                "midzoom" | "crumb" => {
                    // Into the biggest nested folder; "crumb" goes two deep
                    // and rests on the first segment.
                    let depth = if state == "crumb" { 2 } else { 1 };
                    for _ in 0..depth {
                        let Some(app) = shell.app() else { break };
                        let Some(folder) = app
                            .items()
                            .iter()
                            .find(|i| i.depth == 1 && i.kind == ItemKind::FolderTitled)
                            .cloned()
                        else {
                            break;
                        };
                        clock += ms(400);
                        let (x, y) = (folder.rect.x + folder.rect.w / 2.0, folder.rect.y + 3.0);
                        shell.update(InputEvent::PointerMoved { x, y }, clock);
                        let fx = shell.update(InputEvent::DoubleClick { x, y }, clock);
                        apply_shell_effects(&mut renderer, fx, clock);
                        if state == "crumb" {
                            renderer.finish_animation();
                        } else {
                            mid_zoom = true;
                        }
                    }
                    if state == "crumb" {
                        let g = shell.geometry().clone();
                        let (x, y) = (g.crumb.x + 6.0 * scale, g.crumb.y + g.crumb.h / 2.0);
                        clock += ms(100);
                        let fx = shell.update(InputEvent::PointerMoved { x, y }, clock);
                        apply_shell_effects(&mut renderer, fx, clock);
                    }
                }
                s => {
                    let (fx, fy) = card_point(s);
                    let (x, y) = (map.x + map.w * fx, map.y + map.h * fy);
                    let fx = shell.update(InputEvent::PointerMoved { x, y }, ms(10));
                    apply_shell_effects(&mut renderer, fx, ms(10));
                    shell.tick(ms(600));
                    clock = ms(600);
                }
            }
            renderer.set_chrome(shell.chrome(clock));
            renderer.set_card(shell.card().cloned());
            let name = format!(
                "{state}-{}-{scale_name}x.png",
                if dark { "dark" } else { "light" }
            );
            let file = Path::new(&opts.dir).join(&name);
            if mid_zoom {
                // A frame in the middle of the animation, as the window would
                // present it.
                let (w, h) = renderer.size();
                let mut words = vec![0u32; (w * h) as usize];
                renderer.paint_words(&mut words, clock, true);
                renderer.paint_words(&mut words, clock + ms(55), true);
                bs_render::pixel::save_words_png(&words, w, h, &file)?;
            } else {
                renderer.save_png(&file)?;
            }
            println!("{state}: wrote {}", file.display());
        }
    }
    Ok(())
}
