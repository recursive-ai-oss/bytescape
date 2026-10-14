//! The Bytescape application. This is the user interface process: it runs as
//! the normal user and never with elevated rights. Work that needs
//! administrator rights goes to the separate bytescape-helper program.

mod bench;
mod clipboard;
mod live;
mod open;
mod real;
mod synth;
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

/// What this run of the program does.
#[derive(Clone, Debug)]
pub enum Mode {
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
                path = Some(PathBuf::from(other));
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    zoom.path = path.clone();
    shot.path = path.clone();
    Ok(match kind {
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

fn main() -> ExitCode {
    let t0 = Instant::now();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mode = match parse_args(&args) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("bytescape: {e}");
            return ExitCode::from(2);
        }
    };
    match window::run(mode, t0) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("bytescape: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &[&str]) -> Vec<String> {
        s.iter().map(|s| s.to_string()).collect()
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
