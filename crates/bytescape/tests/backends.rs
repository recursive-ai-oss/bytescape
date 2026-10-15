//! The program draws the same pictures with either backend, and keeps going
//! when the GPU fails.
//!
//! * Pictures: the same views are saved with software and GPU drawing (on a
//!   software adapter, llvmpipe) and compared.
//! * Windows: with a start-up step forced to fail, the program still opens a
//!   window, draws the first frame and exits normally. This needs `xvfb-run`.
//!
//! A test that needs a GPU adapter or a virtual display prints `NOT RUN` when
//! there is none. `BYTESCAPE_REQUIRE_GPU=1` makes a missing adapter a failure
//! and `BYTESCAPE_REQUIRE_DISPLAY=1` does the same for `xvfb-run`.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tiny_skia::Pixmap;

const BIN: &str = env!("CARGO_BIN_EXE_bytescape");

fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch folder");
    dir
}

fn require(var: &str) -> bool {
    std::env::var_os(var).is_some()
}

fn picture(dir: &Path, backend: &str, states: &[&str]) -> Output {
    let mut cmd = Command::new(BIN);
    cmd.arg("--screenshot")
        .arg(dir)
        .args(["--size", "800x520", "--boxes", "2000", "--backend", backend])
        .env("BYTESCAPE_GPU_ADAPTER", "software")
        .env_remove("BYTESCAPE_GPU_FAIL_AT");
    for s in states {
        cmd.args(["--state", s]);
    }
    cmd.output().expect("the program starts")
}

fn rgba(path: &Path) -> Vec<u8> {
    Pixmap::load_png(path)
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
        .data()
        .to_vec()
}

fn over(a: &[u8], b: &[u8], tolerance: u8) -> usize {
    assert_eq!(a.len(), b.len());
    a.as_chunks::<4>()
        .0
        .iter()
        .zip(b.as_chunks::<4>().0)
        .filter(|(x, y)| (0..3).any(|i| x[i].abs_diff(y[i]) > tolerance))
        .count()
}

/// A channel may differ by this much at the anti-aliased edge of a hatch.
const EDGE_TOLERANCE: u8 = 16;
/// At most this fraction of a picture may differ by more than that. The
/// software hatch tile doubles a pixel where a one-pixel line wraps around
/// its seam; the GPU draws that line evenly.
const MAX_FRACTION: f64 = 0.02;

#[test]
fn the_same_views_look_alike_with_either_backend() {
    let states = [
        "map20k",
        "showcase",
        "swatch",
        "about",
        "dialog-hold",
        "deleted",
    ];
    let soft = scratch("pictures-software");
    let gpu = scratch("pictures-gpu");
    let a = picture(&soft, "software", &states);
    assert!(a.status.success(), "{}", String::from_utf8_lossy(&a.stderr));
    let b = picture(&gpu, "gpu", &states);
    assert!(b.status.success(), "{}", String::from_utf8_lossy(&b.stderr));
    let note = String::from_utf8_lossy(&b.stderr);
    if note.contains("GPU drawing is not available") {
        assert!(
            !require("BYTESCAPE_REQUIRE_GPU"),
            "a graphics adapter is required: {note}"
        );
        eprintln!("NOT RUN: no graphics adapter ({})", note.trim());
        return;
    }
    let mut compared = 0;
    for state in states {
        for scheme in ["light", "dark"] {
            let name = format!("{state}-{scheme}-1x.png");
            let (x, y) = (rgba(&soft.join(&name)), rgba(&gpu.join(&name)));
            let n = over(&x, &y, EDGE_TOLERANCE);
            let exact = over(&x, &y, 3);
            let pixels = x.len() / 4;
            eprintln!("{name}: {n} of {pixels} pixels over {EDGE_TOLERANCE}, {exact} over 3");
            assert!(
                n as f64 <= pixels as f64 * MAX_FRACTION,
                "{name}: {n} of {pixels} pixels differ by more than {EDGE_TOLERANCE}"
            );
            compared += 1;
        }
    }
    assert_eq!(compared, states.len() * 2);
}

fn has_xvfb() -> bool {
    Command::new("xvfb-run")
        .arg("--help")
        .output()
        .is_ok_and(|o| o.status.success() || !o.stdout.is_empty() || !o.stderr.is_empty())
}

fn first_frame_in_window(dir: &Path, env: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new("xvfb-run");
    cmd.args([
        "-a",
        "-s",
        "-screen 0 1280x800x24",
        BIN,
        "--bench-first-frame",
    ])
    .arg(dir)
    .env_remove("WAYLAND_DISPLAY")
    .env_remove("BYTESCAPE_GPU_FAIL_AT")
    .env_remove("BYTESCAPE_BACKEND");
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output().expect("xvfb-run starts")
}

#[test]
fn a_gpu_failure_at_any_stage_leaves_a_working_window() {
    if !has_xvfb() {
        assert!(
            !require("BYTESCAPE_REQUIRE_DISPLAY"),
            "xvfb-run is required for this test"
        );
        eprintln!("NOT RUN: xvfb-run is not installed");
        return;
    }
    let dir = scratch("window-folder");
    std::fs::write(dir.join("a.bin"), vec![0u8; 4096]).expect("a file");
    for stage in [
        "instance",
        "adapter",
        "device",
        "pipeline",
        "surface",
        "renderer",
        "first-frame",
    ] {
        // The GPU is asked for at the start, so every step runs before the
        // first frame.
        let out = first_frame_in_window(
            &dir,
            &[
                ("BYTESCAPE_BACKEND", "gpu"),
                ("BYTESCAPE_GPU_FAIL_AT", stage),
                ("BYTESCAPE_GPU_ADAPTER", "software"),
            ],
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{stage}: {} {stderr}", out.status);
        assert!(
            stdout.contains("presented"),
            "{stage}: no first frame: {stdout}"
        );
        assert!(
            stderr.contains("using software drawing"),
            "{stage}: no fall back message: {stderr}"
        );
    }
}

#[test]
fn the_automatic_choice_opens_a_window_even_when_the_gpu_cannot_start() {
    if !has_xvfb() {
        assert!(
            !require("BYTESCAPE_REQUIRE_DISPLAY"),
            "xvfb-run is required for this test"
        );
        eprintln!("NOT RUN: xvfb-run is not installed");
        return;
    }
    let dir = scratch("window-folder-auto");
    std::fs::write(dir.join("a.bin"), vec![0u8; 4096]).expect("a file");
    let out = first_frame_in_window(&dir, &[("BYTESCAPE_GPU_FAIL_AT", "device")]);
    assert!(out.status.success(), "{}", out.status);
    assert!(String::from_utf8_lossy(&out.stdout).contains("presented"));
}

#[test]
fn launches_that_leave_early_exit_cleanly() {
    // Leaving while the GPU is still starting must not abort the program.
    if !has_xvfb() {
        assert!(
            !require("BYTESCAPE_REQUIRE_DISPLAY"),
            "xvfb-run is required for this test"
        );
        eprintln!("NOT RUN: xvfb-run is not installed");
        return;
    }
    let dir = scratch("window-folder-early");
    std::fs::write(dir.join("a.bin"), vec![0u8; 4096]).expect("a file");
    for run in 0..8 {
        let out = first_frame_in_window(&dir, &[]);
        assert!(
            out.status.success(),
            "launch {run}: {} {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
    }
}
