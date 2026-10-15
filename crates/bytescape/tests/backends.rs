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

/// The program refuses to run with administrator rights, in every mode. When
/// this test run is itself elevated (a root container, a user namespace
/// mapped to root) there is nothing to draw: check that it said so, once, and
/// that is the whole test.
fn refused_as_elevated(said: &str) -> bool {
    match bs_platform::elevation::refusal() {
        Some(message) => {
            assert_eq!(said.matches(message).count(), 1, "{said}");
            true
        }
        None => false,
    }
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
    if refused_as_elevated(&String::from_utf8_lossy(&a.stderr)) {
        return;
    }
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

/// What one launch inside a virtual display left behind, everything a failed
/// run needs to be understood from a log alone: some versions of `xvfb-run`
/// send the program's error output to its standard output, so both are kept,
/// and the server's own messages go to a file that is read back.
struct Launch {
    out: Output,
    xvfb_messages: String,
}

impl Launch {
    fn stdout(&self) -> String {
        String::from_utf8_lossy(&self.out.stdout).into_owned()
    }

    fn stderr(&self) -> String {
        String::from_utf8_lossy(&self.out.stderr).into_owned()
    }

    fn success(&self) -> bool {
        self.out.status.success()
    }

    /// The whole story of the launch, for an assertion message.
    fn story(&self, what: &str) -> String {
        let env = |name: &str| std::env::var(name).unwrap_or_else(|_| "(not set)".into());
        format!(
            "{what}: {status}\n--- standard output ---\n{stdout}\n--- standard error ---\n{stderr}\n--- xvfb-run and server messages ---\n{xvfb}\n--- environment ---\nHOME={home}\nXDG_RUNTIME_DIR={runtime}\nDBUS_SESSION_BUS_ADDRESS={dbus}\nVK_ICD_FILENAMES={icd}\nRUST_BACKTRACE={bt}\n",
            status = self.out.status,
            stdout = self.stdout(),
            stderr = self.stderr(),
            xvfb = self.xvfb_messages,
            home = env("HOME"),
            runtime = env("XDG_RUNTIME_DIR"),
            dbus = env("DBUS_SESSION_BUS_ADDRESS"),
            icd = env("VK_ICD_FILENAMES"),
            bt = env("RUST_BACKTRACE"),
        )
    }
}

static LAUNCHES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn first_frame_in_window(dir: &Path, env: &[(&str, &str)]) -> Launch {
    let errors = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "xvfb-messages-{}-{}.txt",
        std::process::id(),
        LAUNCHES.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    ));
    let _ = std::fs::remove_file(&errors);
    let mut cmd = Command::new("xvfb-run");
    cmd.arg("-e")
        .arg(&errors)
        .args([
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
    let out = cmd.output().expect("xvfb-run starts");
    let xvfb_messages = std::fs::read_to_string(&errors).unwrap_or_else(|e| format!("(none: {e})"));
    let _ = std::fs::remove_file(&errors);
    Launch { out, xvfb_messages }
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
        if refused_as_elevated(&format!("{}{}", out.stdout(), out.stderr())) {
            return;
        }
        assert!(out.success(), "{}", out.story(&format!("stage {stage}")));
        assert!(
            out.stdout().contains("presented"),
            "{}",
            out.story(&format!("stage {stage}: no first frame"))
        );
        // The message may come on either stream (see `Launch`).
        let said = format!("{}{}", out.stdout(), out.stderr());
        assert!(
            said.contains("using software drawing"),
            "{}",
            out.story(&format!("stage {stage}: no fall back message"))
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
    if refused_as_elevated(&format!("{}{}", out.stdout(), out.stderr())) {
        return;
    }
    assert!(out.success(), "{}", out.story("automatic choice"));
    assert!(
        out.stdout().contains("presented"),
        "{}",
        out.story("automatic choice: no first frame")
    );
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
        if refused_as_elevated(&format!("{}{}", out.stdout(), out.stderr())) {
            return;
        }
        if refused_as_elevated(&format!("{}{}", out.stdout(), out.stderr())) {
            return;
        }
        assert!(out.success(), "{}", out.story(&format!("launch {run}")));
    }
}
