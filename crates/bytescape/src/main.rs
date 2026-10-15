//! The Bytescape application. This is the user interface process: it runs as
//! the normal user and never with elevated rights. Work that needs
//! administrator rights goes to the separate bytescape-helper program.

mod access;
mod bench;
mod fast;
mod live;
mod real;
mod shots;
mod synth;
mod system;
mod window;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::OnceLock;
use std::time::Instant;

/// The system UI font size in logical pixels, unless a measurement asks for
/// another one with `--font-px`.
const DEFAULT_FONT_PX: f32 = 13.0;

static FONT_PX: OnceLock<f32> = OnceLock::new();

/// The font size in logical pixels used for chrome, labels and thresholds.
pub fn font_px() -> f32 {
    FONT_PX.get().copied().unwrap_or(DEFAULT_FONT_PX)
}

/// The lines `--help` prints.
const USAGE: &str = "\
Usage: bytescape [FOLDER]

Shows where the space on a drive or in a folder is used.
With no folder, Bytescape lists your drives and scans them.

Options:
  -h, --help       Print this text and exit
  -V, --version    Print the version, build date and target, and exit

Bytescape never connects to the internet.
";

/// The lines `--version` prints: the version, when the program was built, and
/// what it was built for.
fn version_text() -> String {
    format!(
        "Bytescape {}\nBuilt: {}\nTarget: {}\n",
        env!("BYTESCAPE_VERSION"),
        env!("BYTESCAPE_BUILD_DATE"),
        env!("BYTESCAPE_TARGET")
    )
}

/// The folder named on the command line, as the program should scan it.
///
/// The Explorer verb that scans a drive or folder passes the path with a
/// trailing `\.`: a drive's own trailing backslash would otherwise swallow
/// the closing quote of the command. `C:\.` means `C:\`, so the dot is
/// dropped and the separator put back. `windows` says whether `\` separates
/// parts (on other systems it is an ordinary character in a name).
fn clean_path_argument(arg: &str, windows: bool) -> PathBuf {
    let dotted = [Some(("/.", '/')), windows.then_some(("\\.", '\\'))];
    for (suffix, separator) in dotted.into_iter().flatten() {
        if let Some(base) = arg.strip_suffix(suffix) {
            // Only a whole dot counts, and a dot alone stays the current folder.
            if base.is_empty() {
                return PathBuf::from(separator.to_string());
            }
            let mut path = base.to_string();
            if !path.ends_with(['/', '\\']) || path.ends_with(':') {
                path.push(separator);
            }
            return PathBuf::from(path);
        }
    }
    PathBuf::from(arg)
}

/// What this run of the program does.
#[derive(Clone, Debug)]
pub enum Mode {
    /// Print the version and exit.
    Version,
    /// Print the usage and exit.
    Help,
    /// The normal window, scanning `path` (the home folder when absent).
    Interactive { path: Option<PathBuf> },
    /// Open a window, present one frame, report the timeline and exit. With a
    /// path it goes on to scan and also reports when the first frame showing
    /// scan data was presented.
    FirstFrame { path: Option<PathBuf> },
    /// Scan `path` in a window of `size` pixels and report what the live map
    /// cost.
    Live { path: PathBuf, size: (u32, u32) },
    /// Time font discovery alone.
    FontInit,
    /// Script zooms and hover redraws and print frame times.
    Zoom(ZoomOptions),
    /// Render named states to PNG files without a window.
    Screenshot(ShotOptions),
}

#[derive(Clone, Debug)]
pub struct ZoomOptions {
    pub windowed: bool,
    pub cycles: usize,
    pub cold_cycles: usize,
    /// Wait out each frame interval in real time, as a display would, so work
    /// running beside the animation gets the time it would get live.
    pub paced: bool,
    pub boxes: usize,
    pub size: (u32, u32),
    pub scale: f32,
    pub dark: bool,
    pub hover_moves: usize,
    /// Zoom a real scan of this folder instead of the synthetic scene.
    pub path: Option<PathBuf>,
    /// Names the measurement in the output: keys read `<tag>.scene.*` and
    /// `<tag>_windowed.*`. Only used with a real scan.
    pub tag: String,
}

#[derive(Clone, Debug)]
pub struct ShotOptions {
    pub dir: String,
    pub size: (u32, u32),
    pub scale: f32,
    pub boxes: usize,
    pub states: Vec<String>,
    /// Real scan of this folder for the `midscan` and `complete` states.
    pub path: Option<PathBuf>,
    /// Files the scan must have counted before a `midscan` picture is taken.
    pub midscan_files: u32,
}

fn parse_size(s: &str) -> Option<(u32, u32)> {
    let (w, h) = s.split_once('x')?;
    Some((w.parse().ok()?, h.parse().ok()?))
}

fn parse_args(args: &[String]) -> Result<Mode, String> {
    let mut zoom = ZoomOptions {
        windowed: true,
        cycles: 30,
        cold_cycles: 24,
        paced: false,
        boxes: 20_000,
        size: (3840, 2160),
        scale: 1.0,
        dark: false,
        hover_moves: 1_000,
        path: None,
        tag: "real".into(),
    };
    let mut shot = ShotOptions {
        dir: ".".into(),
        size: (3840, 2160),
        scale: 1.0,
        boxes: 20_000,
        states: Vec::new(),
        path: None,
        midscan_files: 200_000,
    };
    let mut kind = "interactive";
    let mut path: Option<PathBuf> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut value = |name: &str| -> Result<String, String> {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{name} needs a value"))
        };
        match a.as_str() {
            "--version" | "-V" => kind = "version",
            "--help" | "-h" => kind = "help",
            "--bench-first-frame" => kind = "first-frame",
            "--bench-font-init" => kind = "font-init",
            "--bench-zoom" => kind = "zoom",
            "--bench-live" => kind = "live",
            "--screenshot" => {
                kind = "shot";
                shot.dir = value("--screenshot")?;
            }
            "--path" => path = Some(PathBuf::from(value("--path")?)),
            "--tag" => zoom.tag = value("--tag")?,
            "--font-px" => {
                let px: f32 = value("--font-px")?.parse().map_err(|_| "bad --font-px")?;
                if !(2.0..=64.0).contains(&px) {
                    return Err("--font-px must be between 2 and 64".into());
                }
                let _ = FONT_PX.set(px);
            }
            "--midscan-files" => {
                shot.midscan_files = value("--midscan-files")?
                    .parse()
                    .map_err(|_| "bad --midscan-files")?
            }
            "--offscreen" => zoom.windowed = false,
            "--paced" => zoom.paced = true,
            "--cold-cycles" => {
                zoom.cold_cycles = value("--cold-cycles")?
                    .parse()
                    .map_err(|_| "bad --cold-cycles")?
            }
            "--cycles" => zoom.cycles = value("--cycles")?.parse().map_err(|_| "bad --cycles")?,
            "--boxes" => {
                let n: usize = value("--boxes")?.parse().map_err(|_| "bad --boxes")?;
                zoom.boxes = n;
                shot.boxes = n;
            }
            "--hover-moves" => {
                zoom.hover_moves = value("--hover-moves")?
                    .parse()
                    .map_err(|_| "bad --hover-moves")?
            }
            "--size" => {
                let s = parse_size(&value("--size")?).ok_or("bad --size, expected WxH")?;
                zoom.size = s;
                shot.size = s;
            }
            "--scale" => {
                let s: f32 = value("--scale")?.parse().map_err(|_| "bad --scale")?;
                zoom.scale = s;
                shot.scale = s;
            }
            "--dark" => zoom.dark = true,
            "--state" => shot.states.push(value("--state")?),
            other if !other.starts_with('-') && path.is_none() => {
                path = Some(clean_path_argument(other, cfg!(windows)));
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    zoom.path = path.clone();
    shot.path = path.clone();
    Ok(match kind {
        "version" => Mode::Version,
        "help" => Mode::Help,
        "first-frame" => Mode::FirstFrame { path },
        "font-init" => Mode::FontInit,
        "zoom" => Mode::Zoom(zoom),
        "shot" => Mode::Screenshot(shot),
        "live" => Mode::Live {
            path: path.ok_or("--bench-live needs a folder")?,
            size: zoom.size,
        },
        _ => Mode::Interactive { path },
    })
}

/// What the About box says about this build.
pub(crate) fn about() -> bs_ui::notice::AboutInfo {
    bs_ui::notice::AboutInfo::new(
        env!("BYTESCAPE_VERSION"),
        env!("BYTESCAPE_BUILD_DATE"),
        env!("BYTESCAPE_TARGET"),
    )
}

fn main() -> ExitCode {
    let t0 = Instant::now();
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    // Measurement-only: run without the screen-reader adapter.
    let no_access = args.iter().any(|a| a == "--bench-no-access");
    args.retain(|a| a != "--bench-no-access");
    let mode = match parse_args(&args) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("bytescape: {e}");
            return ExitCode::from(2);
        }
    };
    match mode {
        Mode::Version => {
            print!("{}", version_text());
            return ExitCode::SUCCESS;
        }
        Mode::Help => {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        _ => {}
    }
    // The window never runs with administrator rights. A full scan and a delete
    // that needs them go through the helper, which the system asks for.
    if is_refused_when_elevated(&mode)
        && refuse_if_elevated(&bs_platform::elevation::ThisProcess, &mut std::io::stderr())
    {
        return ExitCode::FAILURE;
    }
    // Still the only thread: the screen-reader setup changes the environment.
    access::prepare(
        no_access
            || !matches!(
                mode,
                Mode::Interactive { .. } | Mode::FirstFrame { .. } | Mode::Live { .. }
            ),
    );
    // A crash from here on leaves a report on this computer, shown on the next
    // start. Nothing is sent anywhere.
    bs_platform::crash::install(bs_platform::crash::AppInfo::new(
        "bytescape",
        env!("BYTESCAPE_VERSION"),
        about().build_epoch().unwrap_or(0),
        env!("BYTESCAPE_TARGET"),
    ));
    let result = window::run(mode, t0);
    // Reaching here is a normal shutdown: a crash report written by a panic
    // that was caught on the way is not something to tell the user about next
    // time.
    bs_platform::crash::mark_clean_exit();
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("bytescape: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Whether this run is refused when the process has administrator rights:
/// every run that scans or draws, pictures and measurements included. Only
/// printing the version or the help is harmless.
fn is_refused_when_elevated(mode: &Mode) -> bool {
    !matches!(mode, Mode::Version | Mode::Help)
}

/// Says why the program will not run when its process has administrator
/// rights, once, on `out`. Returns whether it refused.
fn refuse_if_elevated(
    rights: &dyn bs_platform::elevation::ProcessRights,
    out: &mut dyn std::io::Write,
) -> bool {
    match bs_platform::elevation::refusal_for(rights) {
        Some(message) => {
            let _ = writeln!(out, "bytescape: {message}");
            true
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &[&str]) -> Vec<String> {
        s.iter().map(|s| s.to_string()).collect()
    }

    struct Rights(bool);

    impl bs_platform::elevation::ProcessRights for Rights {
        fn is_elevated(&self) -> bool {
            self.0
        }
    }

    #[test]
    fn an_elevated_process_is_refused_once_with_the_reason() {
        let mut out = Vec::new();
        assert!(refuse_if_elevated(&Rights(true), &mut out));
        let text = String::from_utf8(out).unwrap();
        assert_eq!(text.lines().count(), 1);
        assert!(
            text.contains("never does") && text.contains("start it again"),
            "{text}"
        );
        let mut out = Vec::new();
        assert!(!refuse_if_elevated(&Rights(false), &mut out));
        assert!(out.is_empty());
    }

    #[test]
    fn only_runs_with_a_window_are_refused() {
        let interactive = Mode::Interactive { path: None };
        assert!(is_refused_when_elevated(&interactive));
        assert!(!is_refused_when_elevated(&Mode::Version));
        assert!(!is_refused_when_elevated(&Mode::Help));
        // Pictures and measurements scan and draw too.
        assert!(is_refused_when_elevated(&Mode::FontInit));
        for argv in [
            &["--screenshot", "dir", "--path", "/"][..],
            &["--bench-first-frame"],
            &["--bench-font-init"],
            &["--bench-zoom"],
            &["--bench-live", "/some/folder"],
            &["/some/folder"],
        ] {
            let mode = parse_args(&args(argv)).expect("parses");
            assert!(is_refused_when_elevated(&mode), "{argv:?}");
        }
        for argv in [&["--version"][..], &["--help"]] {
            let mode = parse_args(&args(argv)).expect("parses");
            assert!(!is_refused_when_elevated(&mode), "{argv:?}");
        }
    }

    #[test]
    fn crate_name_matches() {
        assert_eq!(env!("CARGO_PKG_NAME"), "bytescape");
    }

    #[test]
    fn build_metadata_is_exported() {
        assert_eq!(env!("BYTESCAPE_VERSION"), env!("CARGO_PKG_VERSION"));
        assert!(!env!("BYTESCAPE_BUILD_DATE").is_empty());
        assert!(!env!("BYTESCAPE_TARGET").is_empty());
    }

    #[test]
    fn no_arguments_means_the_normal_window() {
        assert!(matches!(
            parse_args(&[]),
            Ok(Mode::Interactive { path: None })
        ));
    }

    #[test]
    fn zoom_options_parse() {
        let m = parse_args(&args(&[
            "--bench-zoom",
            "--offscreen",
            "--cycles",
            "5",
            "--size",
            "800x600",
            "--dark",
        ]));
        let Ok(Mode::Zoom(z)) = m else {
            panic!("expected zoom options");
        };
        assert!(!z.windowed && z.dark);
        assert_eq!((z.cycles, z.size), (5, (800, 600)));
    }

    #[test]
    fn version_and_help_are_modes_of_their_own() {
        for flag in ["--version", "-V"] {
            assert!(matches!(parse_args(&args(&[flag])), Ok(Mode::Version)));
        }
        for flag in ["--help", "-h"] {
            assert!(matches!(parse_args(&args(&[flag])), Ok(Mode::Help)));
        }
    }

    #[test]
    fn the_version_text_names_the_version_the_date_and_the_target() {
        let text = version_text();
        assert!(text.starts_with(&format!("Bytescape {}\n", env!("CARGO_PKG_VERSION"))));
        assert!(text.contains(&format!("Built: {}\n", env!("BYTESCAPE_BUILD_DATE"))));
        assert!(text.contains(&format!("Target: {}\n", env!("BYTESCAPE_TARGET"))));
        assert_eq!(text.lines().count(), 3);
    }

    #[test]
    fn the_usage_lines_describe_the_folder_and_the_options() {
        assert!(USAGE.starts_with("Usage: bytescape [FOLDER]"));
        for needle in ["--help", "--version", "never connects to the internet"] {
            assert!(USAGE.contains(needle), "{needle}");
        }
    }

    #[test]
    fn a_trailing_dot_from_the_explorer_verb_is_dropped() {
        let p = |s: &str, windows| clean_path_argument(s, windows);
        assert_eq!(p("C:\\.", true), PathBuf::from("C:\\"));
        assert_eq!(p("D:\\Games\\.", true), PathBuf::from("D:\\Games\\"));
        assert_eq!(p("C:/.", true), PathBuf::from("C:/"));
        assert_eq!(
            p("\\\\server\\share\\.", true),
            PathBuf::from("\\\\server\\share\\")
        );
        // Other paths are left alone.
        assert_eq!(p("C:\\", true), PathBuf::from("C:\\"));
        assert_eq!(p("C:\\Users", true), PathBuf::from("C:\\Users"));
        assert_eq!(p(".", true), PathBuf::from("."));
        assert_eq!(p("..", true), PathBuf::from(".."));
        assert_eq!(p("C:\\..", true), PathBuf::from("C:\\.."));
        assert_eq!(p("C:\\.hidden", true), PathBuf::from("C:\\.hidden"));
        assert_eq!(p("C:\\a.b", true), PathBuf::from("C:\\a.b"));
        // On other systems only the slash separates, so a backslash is a name.
        assert_eq!(p("/data/.", false), PathBuf::from("/data/"));
        assert_eq!(p("/.", false), PathBuf::from("/"));
        assert_eq!(p("odd\\.", false), PathBuf::from("odd\\."));
    }

    #[test]
    fn a_dotted_drive_argument_opens_the_drive() {
        let Ok(Mode::Interactive { path }) = parse_args(&args(&["/data/."])) else {
            panic!("expected the normal window");
        };
        assert_eq!(path, Some(PathBuf::from("/data/")));
    }

    #[test]
    fn bad_arguments_are_reported() {
        assert!(parse_args(&args(&["--nope"])).is_err());
        assert!(parse_args(&args(&["--size", "wide"])).is_err());
        assert!(parse_args(&args(&["--cycles"])).is_err());
    }

    #[test]
    fn a_folder_argument_starts_a_scan() {
        let Ok(Mode::Interactive { path }) = parse_args(&args(&["/data"])) else {
            panic!("expected the normal window");
        };
        assert_eq!(path, Some(PathBuf::from("/data")));
        let Ok(Mode::FirstFrame { path }) = parse_args(&args(&["--bench-first-frame", "/x"]))
        else {
            panic!("expected the first-frame run");
        };
        assert_eq!(path, Some(PathBuf::from("/x")));
        let Ok(Mode::Zoom(z)) = parse_args(&args(&["--bench-zoom", "--path", "/x"])) else {
            panic!("expected zoom options");
        };
        assert_eq!(z.path, Some(PathBuf::from("/x")));
        assert!(parse_args(&args(&["--bench-live"])).is_err());
    }
}
