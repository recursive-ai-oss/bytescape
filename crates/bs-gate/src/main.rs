//! Measures the first-frame time and the zoom frame times of the Bytescape
//! binary and writes machine-readable results plus a short summary.
//!
//! ```text
//! bs-gate run   --label NAME --bytescape PATH --out DIR [--repo DIR]
//!               [--warmups N] [--first-frame-runs N] [--cold-runs N]
//!               [--cycles N] [--cold-cycles N] [--hover-moves N]
//! bs-gate merge --out DIR
//! ```
//!
//! `run` launches the release binary for the first-frame numbers, then runs
//! the offscreen and windowed zoom benchmarks, and stores `DIR/<label>.kv`.
//! Core pinning is done by whoever starts `run` (`taskset`); `run` records the
//! cores it is allowed to use. `merge` turns every `.kv` file into
//! `results.json` and `summary.md`. Use `run.sh` next to this file for the
//! whole procedure.
//!
//! Each gate is reported as a warm series and a cold series, with two
//! verdicts per series: the pass rule, and a strict rule that allows no slow
//! frame at all.
//!
//! * First frame, pass: p95 under 100 ms. Strict: no launch at or over 100 ms.
//!   The warm series is launches after untimed warm-ups. The cold series
//!   evicts the binary's and its libraries' pages from the page cache before
//!   each launch (best effort: no root, so pages another process maps stay).
//! * Zoom is measured paced (each frame waits for its 1/60 s slot, as a
//!   display would) in a window and offscreen, and offscreen unpaced.
//! * Zoom, pass: p99 within 16.7 ms, at most 1% of frames over 16.7 ms and
//!   no frame over 33.3 ms. Strict: no frame over 16.7 ms. The warm series
//!   repeats one pair of views; the cold series zooms into a folder not seen
//!   before each time.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

type Kv = BTreeMap<String, String>;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("run") => run(&args[1..]),
        Some("merge") => merge(&args[1..]),
        _ => Err("usage: bs-gate run|merge ...".to_string()),
    };
    if let Err(e) = result {
        eprintln!("bs-gate: {e}");
        std::process::exit(1);
    }
}

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).cloned())
}

fn flag_num(args: &[String], name: &str, default: usize) -> usize {
    flag(args, name)
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn cmd_out(program: &str, args: &[&str]) -> String {
    Command::new(program)
        .args(args)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

fn read(path: &str) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

fn parse_kv(text: &str, kv: &mut Kv) {
    for line in text.lines() {
        if let Some((k, v)) = line.split_once('=') {
            kv.insert(k.trim().to_string(), v.trim().to_string());
        }
    }
}

fn percentile(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    sorted[(((sorted.len() - 1) as f64) * q).round() as usize]
}

fn put_dist(kv: &mut Kv, prefix: &str, values: &[f64]) {
    let mut s = values.to_vec();
    s.sort_by(|a, b| a.total_cmp(b));
    kv.insert(format!("{prefix}.n"), s.len().to_string());
    kv.insert(
        format!("{prefix}.min_ms"),
        format!("{:.3}", percentile(&s, 0.0)),
    );
    kv.insert(
        format!("{prefix}.median_ms"),
        format!("{:.3}", percentile(&s, 0.5)),
    );
    kv.insert(
        format!("{prefix}.p95_ms"),
        format!("{:.3}", percentile(&s, 0.95)),
    );
    kv.insert(
        format!("{prefix}.max_ms"),
        format!("{:.3}", percentile(&s, 1.0)),
    );
}

/// Starts the binary, stamps the clock before the spawn and again when the
/// child reports that its first frame has been presented.
fn first_frame_once(bin: &Path) -> Result<(f64, Kv), String> {
    let start = Instant::now();
    let mut child = Command::new(bin)
        .arg("--bench-first-frame")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("cannot start {}: {e}", bin.display()))?;
    let stdout = child.stdout.take().ok_or("no stdout")?;
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    let mut presented = None;
    loop {
        line.clear();
        let n = reader.read_line(&mut line).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        if line.trim() == "presented" {
            presented = Some(start.elapsed());
            break;
        }
    }
    let mut rest = String::new();
    for l in reader.lines().map_while(Result::ok) {
        rest.push_str(&l);
        rest.push('\n');
    }
    let _ = child.wait();
    let t = presented.ok_or("the binary exited without presenting a frame")?;
    let mut kv = Kv::new();
    parse_kv(&rest, &mut kv);
    Ok((t.as_secs_f64() * 1000.0, kv))
}

fn capture(bin: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new(bin)
        .args(args)
        .stderr(Stdio::inherit())
        .output()
        .map_err(|e| format!("cannot start {}: {e}", bin.display()))?;
    if !out.status.success() {
        return Err(format!(
            "{} {:?} failed: {}",
            bin.display(),
            args,
            out.status
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// The short commit of the checkout at `repo`, and whether it has uncommitted
/// changes. Git searches parent directories, so a directory that is not itself
/// a checkout could report some enclosing repository's commit; the answer is
/// only used when the checkout's top level is exactly `repo`.
fn repo_commit(repo: &str) -> (String, bool) {
    let top = cmd_out("git", &["-C", repo, "rev-parse", "--show-toplevel"]);
    let same = match (std::fs::canonicalize(&top), std::fs::canonicalize(repo)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    };
    if !same {
        return ("unknown".into(), false);
    }
    let commit = cmd_out("git", &["-C", repo, "rev-parse", "--short=12", "HEAD"]);
    if commit.is_empty() {
        return ("unknown".into(), false);
    }
    let dirty = !cmd_out("git", &["-C", repo, "status", "--porcelain"]).is_empty();
    (commit, dirty)
}

fn environment(args: &[String], label: &str, kv: &mut Kv) {
    let cpuinfo = read("/proc/cpuinfo");
    let model = cpuinfo
        .lines()
        .find(|l| l.starts_with("model name"))
        .and_then(|l| l.split_once(':'))
        .map(|(_, v)| v.trim().to_string())
        .unwrap_or_else(|| "unknown".into());
    let logical = cpuinfo
        .lines()
        .filter(|l| l.starts_with("processor"))
        .count();
    let status = read("/proc/self/status");
    let allowed = status
        .lines()
        .find(|l| l.starts_with("Cpus_allowed_list"))
        .and_then(|l| l.split_once(':'))
        .map(|(_, v)| v.trim().to_string())
        .unwrap_or_default();
    let repo = flag(args, "--repo").unwrap_or_else(|| ".".into());
    let (commit, dirty) = repo_commit(&repo);
    let env = [
        ("env.label", label.to_string()),
        ("env.cpu_model", model),
        ("env.logical_cpus_on_host", logical.to_string()),
        ("env.cpus_allowed", allowed),
        (
            "env.kernel",
            read("/proc/sys/kernel/osrelease").trim().to_string(),
        ),
        ("env.rustc", cmd_out("rustc", &["-V"])),
        (
            "env.build_profile",
            flag(args, "--profile").unwrap_or_else(|| "release".into()),
        ),
        (
            "env.commit",
            if dirty {
                format!("{commit}+uncommitted")
            } else {
                commit
            },
        ),
        (
            "env.display_server",
            format!(
                "Xvfb (DISPLAY={}); X11 shared-memory present, no compositor, no GPU",
                std::env::var("DISPLAY").unwrap_or_default()
            ),
        ),
        (
            "env.scale_factor_override",
            std::env::var("WINIT_X11_SCALE_FACTOR").unwrap_or_else(|_| "none (1)".into()),
        ),
        (
            "env.load_average_at_start",
            read("/proc/loadavg").trim().to_string(),
        ),
    ];
    for (k, v) in env {
        kv.insert(k.to_string(), v);
    }
}

/// Asks the kernel to drop the cached pages of one file. Reading with
/// `iflag=nocache` and a zero count discards the whole file's cache; pages that
/// another process has mapped stay.
fn evict(path: &Path) {
    let _ = Command::new("dd")
        .arg(format!("if={}", path.display()))
        .args(["iflag=nocache", "count=0", "status=none"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Libraries a launch maps: those the dynamic linker reports, plus the ones the
/// windowing code opens at run time.
fn cold_files(bin: &Path) -> Vec<PathBuf> {
    let mut files = vec![bin.to_path_buf()];
    let ldd = cmd_out("ldd", &[&bin.to_string_lossy()]);
    for line in ldd.lines() {
        if let Some(rest) = line.split("=>").nth(1)
            && let Some(path) = rest.split_whitespace().next()
            && path.starts_with('/')
        {
            files.push(PathBuf::from(path));
        }
    }
    let dirs = [
        "/usr/lib/x86_64-linux-gnu",
        "/usr/lib/aarch64-linux-gnu",
        "/usr/lib",
        "/lib64",
    ];
    let names = [
        "libxcb.so.1",
        "libX11.so.6",
        "libX11-xcb.so.1",
        "libXau.so.6",
        "libXdmcp.so.6",
        "libxkbcommon.so.0",
        "libxkbcommon-x11.so.0",
        "libwayland-client.so.0",
        "libwayland-cursor.so.0",
        "libXcursor.so.1",
        "libfontconfig.so.1",
        "libfreetype.so.6",
    ];
    for dir in dirs {
        for name in names {
            let p = Path::new(dir).join(name);
            if p.exists() {
                files.push(p);
            }
        }
    }
    files
}

fn first_frame_series(
    bin: &Path,
    kv: &mut Kv,
    name: &str,
    runs: usize,
    cold: bool,
) -> Result<(), String> {
    let files = if cold { cold_files(bin) } else { Vec::new() };
    let mut totals = Vec::new();
    let mut built = Vec::new();
    let mut internal = Vec::new();
    for _ in 0..runs {
        for f in &files {
            evict(f);
        }
        let (ms, k) = first_frame_once(bin)?;
        totals.push(ms);
        let get = |name: &str| k.get(name).and_then(|v| v.parse::<f64>().ok());
        if let Some(v) = get("internal.event_loop_built_ms") {
            built.push(v);
        }
        if let Some(v) = get("internal.first_frame_presented_ms") {
            internal.push(v);
        }
    }
    put_dist(
        kv,
        &format!("first_frame.{name}.spawn_to_first_present"),
        &totals,
    );
    put_dist(
        kv,
        &format!("first_frame.{name}.inside_binary.event_loop_built"),
        &built,
    );
    put_dist(
        kv,
        &format!("first_frame.{name}.inside_binary.first_present"),
        &internal,
    );
    if cold {
        kv.insert(
            format!("first_frame.{name}.evicted_files"),
            files.len().to_string(),
        );
    }
    Ok(())
}

fn run(args: &[String]) -> Result<(), String> {
    let label = flag(args, "--label").ok_or("--label is required")?;
    let bin = PathBuf::from(flag(args, "--bytescape").ok_or("--bytescape is required")?);
    let out = PathBuf::from(flag(args, "--out").ok_or("--out is required")?);
    let warmups = flag_num(args, "--warmups", 5);
    let runs = flag_num(args, "--first-frame-runs", 30);
    let cold_runs = flag_num(args, "--cold-runs", 15);
    let cycles = flag_num(args, "--cycles", 30);
    let cold_cycles = flag_num(args, "--cold-cycles", 24);
    let hover = flag_num(args, "--hover-moves", 1000);
    std::fs::create_dir_all(&out).map_err(|e| e.to_string())?;
    let mut kv = Kv::new();
    environment(args, &label, &mut kv);

    // First frame. Warm-up launches are not timed.
    for _ in 0..warmups {
        first_frame_once(&bin)?;
    }
    first_frame_series(&bin, &mut kv, "warm", runs, false)?;
    kv.insert("first_frame.warm.warmup_runs".into(), warmups.to_string());
    first_frame_series(&bin, &mut kv, "cold", cold_runs, true)?;

    // Font discovery, which the first frame does not wait for.
    let mut fonts = Vec::new();
    for _ in 0..7 {
        let text = capture(&bin, &["--bench-font-init"])?;
        let mut k = Kv::new();
        parse_kv(&text, &mut k);
        if let Some(v) = k.get("font_init_ms").and_then(|v| v.parse::<f64>().ok()) {
            fonts.push(v);
        }
    }
    put_dist(&mut kv, "fonts.discovery", &fonts);

    // Zoom.
    let c = cycles.to_string();
    let cc = cold_cycles.to_string();
    let h = hover.to_string();
    // Paced runs wait out each 1/60 s frame slot in real time, so background
    // work gets the time it would get live. The unpaced offscreen run draws
    // frames back to back, which is the worst case for that background work.
    for (windowed, paced) in [(false, true), (true, true), (false, false)] {
        let mut a = vec!["--bench-zoom"];
        if !windowed {
            a.push("--offscreen");
        }
        if paced {
            a.push("--paced");
        }
        a.extend(["--cycles", &c, "--cold-cycles", &cc, "--hover-moves", &h]);
        parse_kv(&capture(&bin, &a)?, &mut kv);
    }

    let mut text = String::new();
    for (k, v) in &kv {
        let _ = writeln!(text, "{k}={v}");
    }
    let path = out.join(format!("{label}.kv"));
    std::fs::write(&path, text).map_err(|e| e.to_string())?;
    println!("wrote {}", path.display());
    Ok(())
}

fn json_escape(s: &str) -> String {
    let mut o = String::new();
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            c if (c as u32) < 0x20 => {
                let _ = write!(o, "\\u{:04x}", c as u32);
            }
            c => o.push(c),
        }
    }
    o
}

fn json_value(v: &str) -> String {
    let plain = v
        .chars()
        .all(|c| c.is_ascii_digit() || c == '.' || c == '-')
        && !(v.len() > 1 && v.starts_with('0') && !v.starts_with("0."));
    match v.parse::<f64>() {
        Ok(f) if f.is_finite() && plain => v.to_string(),
        _ => format!("\"{}\"", json_escape(v)),
    }
}

fn num(kv: &Kv, key: &str) -> f64 {
    kv.get(key).and_then(|v| v.parse().ok()).unwrap_or(f64::NAN)
}

/// Gate criteria, written out so the verdicts can be checked by hand.
const FIRST_FRAME_LIMIT_MS: f64 = 100.0;
const FRAME_BUDGET_MS: f64 = 16.7;
const FRAME_HARD_LIMIT_MS: f64 = 33.3;

struct Verdict {
    pass: bool,
    strict: bool,
    detail: String,
}

fn verdict_first_frame(kv: &Kv, series: &str) -> Verdict {
    let key = |s: &str| format!("first_frame.{series}.spawn_to_first_present.{s}");
    let med = num(kv, &key("median_ms"));
    let p95 = num(kv, &key("p95_ms"));
    let max = num(kv, &key("max_ms"));
    Verdict {
        pass: p95 < FIRST_FRAME_LIMIT_MS,
        strict: max < FIRST_FRAME_LIMIT_MS,
        detail: format!(
            "median {med:.1} ms, p95 {p95:.1} ms, max {max:.1} ms against {FIRST_FRAME_LIMIT_MS} ms. Pass: p95 under the limit. Strict: no launch at or over the limit"
        ),
    }
}

fn verdict_zoom(kv: &Kv, layer: &str, series: &str) -> Verdict {
    let key = |s: &str| format!("{layer}.zoom.{series}.all_frames.{s}");
    let p99 = num(kv, &key("p99_ms"));
    let max = num(kv, &key("max_ms"));
    let n = num(kv, &key("n"));
    let over = num(kv, &key("over_16_7ms"));
    let over_hard = num(kv, &key("over_33_3ms"));
    // Missing keys give NaN, which fails every comparison below.
    let allowed = (n * 0.01).floor();
    Verdict {
        pass: p99 <= FRAME_BUDGET_MS && over <= allowed && over_hard == 0.0,
        strict: over == 0.0,
        detail: format!(
            "p99 {p99:.2} ms, max {max:.2} ms, {over} of {n} frames over {FRAME_BUDGET_MS} ms, {over_hard} over {FRAME_HARD_LIMIT_MS} ms. Pass: p99 within {FRAME_BUDGET_MS} ms, at most 1% of frames over it and none over {FRAME_HARD_LIMIT_MS} ms. Strict: no frame over {FRAME_BUDGET_MS} ms"
        ),
    }
}

fn verdicts_for(kv: &Kv) -> Vec<(String, Verdict)> {
    let mut v = Vec::new();
    for series in ["warm", "cold"] {
        v.push((
            format!("first_frame.{series}"),
            verdict_first_frame(kv, series),
        ));
        for (name, layer) in [
            ("zoom_windowed", "windowed"),
            ("zoom_offscreen", "offscreen"),
            ("zoom_offscreen_unpaced", "offscreen_unpaced"),
        ] {
            v.push((format!("{name}.{series}"), verdict_zoom(kv, layer, series)));
        }
    }
    v
}

fn merge(args: &[String]) -> Result<(), String> {
    let out = PathBuf::from(flag(args, "--out").ok_or("--out is required")?);
    let mut runs: BTreeMap<String, Kv> = BTreeMap::new();
    for entry in std::fs::read_dir(&out).map_err(|e| e.to_string())? {
        let path = entry.map_err(|e| e.to_string())?.path();
        if path.extension().and_then(|e| e.to_str()) == Some("kv") {
            let label = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("run")
                .to_string();
            let mut kv = Kv::new();
            parse_kv(&read(&path.to_string_lossy()), &mut kv);
            runs.insert(label, kv);
        }
    }
    if runs.is_empty() {
        return Err("no .kv files found; run `bs-gate run` first".into());
    }

    let mut json = String::from("{\n  \"runs\": {\n");
    let labels: Vec<&String> = runs.keys().collect();
    for (i, label) in labels.iter().enumerate() {
        let kv = &runs[*label];
        let _ = writeln!(json, "    \"{}\": {{", json_escape(label));
        let n = kv.len();
        for (j, (k, v)) in kv.iter().enumerate() {
            let _ = writeln!(
                json,
                "      \"{}\": {}{}",
                json_escape(k),
                json_value(v),
                if j + 1 < n { "," } else { "" }
            );
        }
        let _ = writeln!(
            json,
            "    }}{}",
            if i + 1 < labels.len() { "," } else { "" }
        );
    }
    json.push_str("  },\n  \"verdicts\": {\n");
    let mut lines = Vec::new();
    for label in &labels {
        for (name, v) in verdicts_for(&runs[*label]) {
            lines.push(format!(
                "    \"{label}.{name}\": {{ \"pass\": {}, \"strict_pass\": {}, \"detail\": \"{}\" }}",
                v.pass,
                v.strict,
                json_escape(&v.detail)
            ));
        }
    }
    json.push_str(&lines.join(",\n"));
    json.push_str("\n  }\n}\n");
    std::fs::write(out.join("results.json"), json).map_err(|e| e.to_string())?;

    let word = |b: bool| if b { "pass" } else { "FAIL" };
    let mut md = String::from(
        "# Gate results\n\n## Verdicts\n\n| run | measure | pass rule | strict (no slow frame) |\n|---|---|---|---|\n",
    );
    for label in &labels {
        for (name, v) in verdicts_for(&runs[*label]) {
            let _ = writeln!(
                md,
                "| {label} | {name} | {} | {} |",
                word(v.pass),
                word(v.strict)
            );
        }
    }
    md.push('\n');
    for label in &labels {
        let kv = &runs[*label];
        let _ = writeln!(md, "## Run `{label}`\n");
        md.push_str("Environment:\n\n");
        for (k, v) in kv.iter().filter(|(k, _)| k.starts_with("env.")) {
            let _ = writeln!(md, "- {}: {}", k.trim_start_matches("env."), v);
        }
        let get = |k: &str| kv.get(k).map_or("?", |v| v).to_string();
        let _ = writeln!(
            md,
            "- window: {}x{} at scale {}, {} render threads",
            get("windowed.width"),
            get("windowed.height"),
            get("windowed.scale_factor"),
            get("windowed.render_threads")
        );
        let _ = writeln!(
            md,
            "- scene: {} (seed {}), {} boxes in the root view, {} in the child view, minimum box area {} px², {} other folders available for the cold series\n",
            get("scene.kind"),
            get("scene.seed"),
            get("scene.boxes_root_view"),
            get("scene.boxes_child_view"),
            get("scene.min_area_px2"),
            get("scene.cold_folders_available"),
        );
        md.push_str("### First frame (process start to first present), milliseconds\n\n");
        md.push_str("| measure | n | min | median | p95 | max |\n|---|---|---|---|---|---|\n");
        for (name, key) in [
            (
                "warm: spawn to first present (harness clock)",
                "first_frame.warm.spawn_to_first_present",
            ),
            (
                "warm: inside the binary, window system ready",
                "first_frame.warm.inside_binary.event_loop_built",
            ),
            (
                "warm: inside the binary, first present",
                "first_frame.warm.inside_binary.first_present",
            ),
            (
                "cold: spawn to first present (harness clock)",
                "first_frame.cold.spawn_to_first_present",
            ),
            (
                "cold: inside the binary, window system ready",
                "first_frame.cold.inside_binary.event_loop_built",
            ),
            (
                "cold: inside the binary, first present",
                "first_frame.cold.inside_binary.first_present",
            ),
            (
                "font discovery (not on the first-frame path)",
                "fonts.discovery",
            ),
        ] {
            let _ = writeln!(
                md,
                "| {name} | {} | {} | {} | {} | {} |",
                get(&format!("{key}.n")),
                get(&format!("{key}.min_ms")),
                get(&format!("{key}.median_ms")),
                get(&format!("{key}.p95_ms")),
                get(&format!("{key}.max_ms")),
            );
        }
        md.push('\n');
        for (layer, title) in [
            (
                "windowed",
                "render + present into a 4K window (X11 shared memory), paced in real time",
            ),
            (
                "offscreen",
                "render into a 3840x2160 pixmap, no present, paced in real time",
            ),
            (
                "offscreen_unpaced",
                "render into a 3840x2160 pixmap, no present, frames back to back (worst case for background label shaping)",
            ),
        ] {
            let _ = writeln!(md, "### Zoom, {title}\n");
            md.push_str("Milliseconds per frame (CPU time of the render call, plus the present call for the windowed layer). In the cold series the first frame also carries the layout time of the new view.\n\n");
            md.push_str("| series | frames | n | p50 | p95 | p99 | max | over 16.7 ms | over 33.3 ms |\n|---|---|---|---|---|---|---|---|---|\n");
            for series in ["warm", "cold"] {
                for (name, key) in [
                    ("all zoom frames", "all_frames"),
                    ("animation frames", "anim_frames"),
                    ("first frame (target view rendered)", "first_frames"),
                    ("final frame (labels, full quality)", "final_frames"),
                ] {
                    let k = |s: &str| get(&format!("{layer}.zoom.{series}.{key}.{s}"));
                    let _ = writeln!(
                        md,
                        "| {series} | {name} | {} | {} | {} | {} | {} | {} | {} |",
                        k("n"),
                        k("p50_ms"),
                        k("p95_ms"),
                        k("p99_ms"),
                        k("max_ms"),
                        k("over_16_7ms"),
                        k("over_33_3ms")
                    );
                }
            }
            for (name, key) in [("hover redraw", "hover"), ("selection redraw", "select")] {
                let k = |s: &str| get(&format!("{layer}.{key}.{s}"));
                let _ = writeln!(
                    md,
                    "| - | {name} | {} | {} | {} | {} | {} | {} | {} |",
                    k("n"),
                    k("p50_ms"),
                    k("p95_ms"),
                    k("p99_ms"),
                    k("max_ms"),
                    k("over_16_7ms"),
                    k("over_33_3ms")
                );
            }
            md.push('\n');
        }
        for (name, v) in verdicts_for(kv) {
            let _ = writeln!(
                md,
                "- {name}: pass rule **{}**, strict **{}**. {}.",
                word(v.pass),
                word(v.strict),
                v.detail
            );
        }
        md.push('\n');
    }
    md.push_str("Notes: Xvfb measures X11 shared-memory presentation, not a compositor and not a GPU. The animation clock is virtual (frames are stepped 1/60 s apart) and the paced runs also wait each frame slot out in real time; the unpaced run does not, so background label shaping gets less wall-clock time than it would live. Layout time is excluded from frame times except in the first frame of the cold series. Page-cache eviction for the cold first-frame series is best effort and needs no root. A pass here is a pass on this host and on the pinned-core proxy only; target hardware and Windows are confirmed by a person.\n");
    std::fs::write(out.join("summary.md"), md).map_err(|e| e.to_string())?;
    println!("wrote {}/results.json and summary.md", out.display());
    Ok(())
}
