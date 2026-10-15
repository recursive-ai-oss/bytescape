//! The note after a crash, the report viewer, the About box and the menu of
//! the program itself, driven through the shell. A planted report in a
//! temporary folder stands in for a real crash; the clock is a fixed one.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use accesskit::Role;
use bs_layout::Metrics;
use bs_render::{CardPart, Rect, TextEngine, Widget};
use bs_tree::{Aggregator, Batch, Category, DirKey, Meta, NodeKind, Snapshot};
use bs_ui::chrome::TextMeasure;
use bs_ui::notice::{AboutInfo, CRASH_WORDS, CrashNotice, RELEASES_URL};
use bs_ui::picker::Picker;
use bs_ui::shell::access::{AccessTarget, AccessTree};
use bs_ui::shell::{Look, Shell, ShellEffect};
use bs_ui::tree::TreeSource;
use bs_ui::{App, Direction, InputEvent, Key};

struct Fixed(f32);

impl TextMeasure for Fixed {
    fn width(&self, text: &str) -> Option<f32> {
        Some(text.chars().count() as f32 * self.0)
    }
}

fn t(ms: u64) -> Duration {
    Duration::from_millis(ms)
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

fn tree() -> Arc<Snapshot> {
    let mut agg = Aggregator::new();
    let mut b = Batch::new();
    b.dir(DirKey(0), DirKey::NONE, b"/data", &meta(4096))
        .unwrap();
    for i in 0..3u32 {
        let key = DirKey(i + 1);
        b.dir(
            key,
            DirKey(0),
            format!("folder{}", i + 1).as_bytes(),
            &meta(4096),
        )
        .unwrap();
        for f in 0..20u32 {
            b.file(
                NodeKind::File,
                key,
                format!("file{f}").as_bytes(),
                &meta(1_000_000 * (3 - u64::from(i)) + u64::from(f)),
            )
            .unwrap();
        }
        b.listing_done(key);
    }
    b.listing_done(DirKey(0));
    agg.apply(&b);
    agg.publish()
}

fn shell_of(size: (u32, u32)) -> Shell<TreeSource> {
    let mut shell: Shell<TreeSource> = Shell::new(
        size,
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
    let source = TreeSource::new(tree(), Metrics::from_logical(13.0, 1.0)).expect("root");
    shell.set_app(App::new(source, map));
    shell
}

fn shell() -> Shell<TreeSource> {
    shell_of((1200, 800))
}

// ---- a planted crash report ---------------------------------------------------------

static COUNTER: AtomicU32 = AtomicU32::new(0);

struct Planted {
    dir: PathBuf,
}

impl Planted {
    fn new(body: &str) -> Self {
        let dir = std::fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!(
                "bs-ui-notices-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("crash-20261015T010203-4242-0000.txt"), body).unwrap();
        Self { dir }
    }

    fn path(&self) -> &Path {
        &self.dir
    }

    fn notice(&self) -> CrashNotice {
        let report = bs_platform::crash::pending_report_in(&self.dir).expect("a pending report");
        CrashNotice::from_report(&report)
    }
}

impl Drop for Planted {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

const REPORT: &str = "Bytescape crash report\nFormat: 1\nProgram: bytescape 0.1.0\nBuilt: 2026-10-14\nTarget: x86_64-unknown-linux-gnu\nThread: main\nPanic:\n    could not read /home/zed/Documents/taxes 2025.pdf: gone\nLocation: /home/zed/src/app.rs:12:5\nBacktrace:\n   0: bytescape::scan\n             at /home/zed/src/scan.rs:40:9\n";

fn with_banner(shell: &mut Shell<TreeSource>, planted: &Planted) -> Vec<ShellEffect> {
    shell.set_crash_notice(Some(planted.notice()), t(10))
}

fn widget_texts(shell: &mut Shell<TreeSource>, now: u64) -> (Vec<String>, Vec<(String, bool)>) {
    let c = shell.chrome(t(now));
    let mut texts = Vec::new();
    let mut pills = Vec::new();
    for w in &c.widgets {
        match w {
            Widget::Text { text, .. } => texts.push(text.to_string()),
            Widget::Pill { text, hot, .. } => pills.push((text.to_string(), *hot)),
            _ => {}
        }
    }
    (texts, pills)
}

fn pill_rect(shell: &mut Shell<TreeSource>, label: &str) -> Rect {
    let c = shell.chrome(t(0));
    c.widgets
        .iter()
        .find_map(|w| match w {
            Widget::Pill { rect, text, .. } if &**text == label => Some(*rect),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no button {label}"))
}

fn centre(r: Rect) -> (f32, f32) {
    (r.x + r.w / 2.0, r.y + r.h / 2.0)
}

fn click_pill(shell: &mut Shell<TreeSource>, label: &str, at: u64) -> Vec<ShellEffect> {
    let (x, y) = centre(pill_rect(shell, label));
    shell.update(InputEvent::PointerMoved { x, y }, t(at));
    shell.update(InputEvent::Click { x, y }, t(at + 1))
}

fn popup_button(shell: &Shell<TreeSource>, label: &str) -> (f32, f32) {
    let popup = shell.popup().expect("a popup");
    popup
        .card
        .parts
        .iter()
        .find_map(|p| match p {
            CardPart::Button { rect, label: l, .. } | CardPart::Row { rect, label: l, .. }
                if &**l == label =>
            {
                Some(centre(*rect))
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("no control {label}"))
}

fn click_popup(shell: &mut Shell<TreeSource>, label: &str, at: u64) -> Vec<ShellEffect> {
    let (x, y) = popup_button(shell, label);
    shell.update(InputEvent::PointerMoved { x, y }, t(at));
    shell.update(InputEvent::Click { x, y }, t(at + 1))
}

fn has_url(fx: &[ShellEffect], start: &str) -> bool {
    fx.iter()
        .any(|e| matches!(e, ShellEffect::OpenUrl(u) if u.starts_with(start)))
}

// ---- the banner ------------------------------------------------------------------------

#[test]
fn a_planted_report_shows_the_banner_with_its_words_and_buttons() {
    let planted = Planted::new(REPORT);
    let mut s = shell();
    let before = s.map_rect();
    let fx = with_banner(&mut s, &planted);
    assert!(fx.iter().any(|e| matches!(e, ShellEffect::MapMoved)));
    assert_eq!(
        s.crash_banner(),
        Some((
            "Bytescape closed unexpectedly last time.",
            ["View report", "Report on GitHub", "Dismiss"]
        ))
    );
    let (texts, pills) = widget_texts(&mut s, 20);
    assert!(texts.contains(&CRASH_WORDS.to_string()), "{texts:?}");
    for label in ["View report", "Report on GitHub", "Dismiss"] {
        assert!(pills.iter().any(|p| p.0 == label), "{label}");
    }
    // The strip takes its room off the top of the map, so nothing is covered.
    let after = s.map_rect();
    assert!(after.y > before.y && after.h < before.h);
    assert!((after.bottom() - before.bottom()).abs() < 0.01);
    let app = s.app().unwrap();
    for item in app.items().iter() {
        assert!(
            item.rect.y >= after.y - 0.01,
            "{:?} under the banner",
            item.rect
        );
    }
}

#[test]
fn no_report_no_banner() {
    let mut s = shell();
    assert!(s.crash_banner().is_none());
    let (texts, _) = widget_texts(&mut s, 20);
    assert!(!texts.contains(&CRASH_WORDS.to_string()));
    let fx = s.set_crash_notice(None, t(10));
    assert!(fx.is_empty(), "nothing moved");
}

#[test]
fn view_report_shows_it_with_paths_hidden_until_asked() {
    let planted = Planted::new(REPORT);
    let mut s = shell();
    with_banner(&mut s, &planted);
    click_pill(&mut s, "View report", 100);
    assert!(s.modal_open());
    let hidden = s.report_text().expect("the report viewer");
    assert!(hidden.contains("Crash report"), "{hidden}");
    assert!(hidden.contains("Paths are hidden"), "{hidden}");
    assert!(hidden.contains("could not read <path-1>"), "{hidden}");
    assert!(!hidden.contains("/home/zed"), "{hidden}");
    assert!(!hidden.contains("taxes"), "{hidden}");
    assert_eq!(
        s.dialog_buttons(),
        ["Show paths", "Report on GitHub", "Close"]
    );
    // Show the paths.
    click_popup(&mut s, "Show paths", 200);
    let shown = s.report_text().unwrap();
    assert!(
        shown.contains("/home/zed/Documents/taxes 2025.pdf"),
        "{shown}"
    );
    assert!(shown.contains("Paths are shown"), "{shown}");
    assert_eq!(s.dialog_buttons()[0], "Hide paths");
    // And hide them again.
    click_popup(&mut s, "Hide paths", 300);
    assert!(!s.report_text().unwrap().contains("/home/zed"));
    click_popup(&mut s, "Close", 400);
    assert!(!s.modal_open());
    assert!(
        s.crash_banner().is_some(),
        "closing the viewer keeps the note"
    );
}

#[test]
fn report_on_github_opens_the_link_with_paths_hidden() {
    let planted = Planted::new(REPORT);
    let mut s = shell();
    with_banner(&mut s, &planted);
    // From the banner.
    let fx = click_pill(&mut s, "Report on GitHub", 100);
    assert!(has_url(&fx, bs_platform::crash::ISSUE_URL), "{fx:?}");
    let url = fx
        .iter()
        .find_map(|e| match e {
            ShellEffect::OpenUrl(u) => Some(u.clone()),
            _ => None,
        })
        .unwrap();
    assert!(!url.contains("zed") && !url.contains("taxes"), "{url}");
    assert!(url.len() <= bs_platform::crash::MAX_URL_BYTES);
    // And from the viewer, even while the paths are shown on screen.
    click_pill(&mut s, "View report", 200);
    click_popup(&mut s, "Show paths", 300);
    let fx = click_popup(&mut s, "Report on GitHub", 400);
    let url2 = fx
        .iter()
        .find_map(|e| match e {
            ShellEffect::OpenUrl(u) => Some(u.clone()),
            _ => None,
        })
        .expect("a link");
    assert_eq!(url, url2);
    assert!(s.modal_open(), "the viewer stays while the browser opens");
}

#[test]
fn dismiss_marks_the_report_seen_and_gives_the_room_back() {
    let planted = Planted::new(REPORT);
    let mut s = shell();
    let before = s.map_rect();
    with_banner(&mut s, &planted);
    let fx = click_pill(&mut s, "Dismiss", 100);
    assert!(fx.iter().any(|e| matches!(e, ShellEffect::CrashSeen)));
    assert!(fx.iter().any(|e| matches!(e, ShellEffect::MapMoved)));
    assert!(s.crash_banner().is_none());
    assert_eq!(s.map_rect(), before);
    // The host acknowledges the report it holds; it is not offered again.
    let report = bs_platform::crash::pending_report_in(planted.path()).unwrap();
    bs_platform::crash::acknowledge(&report).unwrap();
    assert!(bs_platform::crash::pending_report_in(planted.path()).is_none());
}

#[test]
fn the_banner_is_reached_with_tab_and_pressed_with_enter() {
    let planted = Planted::new(REPORT);
    let mut s = shell();
    with_banner(&mut s, &planted);
    // Tab moves onto the first button, which shows as hot.
    s.update(InputEvent::Key(Key::Tab), t(100));
    let (_, pills) = widget_texts(&mut s, 110);
    assert_eq!(
        pills.iter().find(|p| p.1).map(|p| p.0.as_str()),
        Some("View report")
    );
    s.update(InputEvent::Key(Key::Tab), t(120));
    let fx = s.update(InputEvent::Key(Key::Enter), t(130));
    assert!(has_url(&fx, bs_platform::crash::ISSUE_URL), "{fx:?}");
    // Shift+Tab goes back; Enter on the first opens the viewer.
    s.update(InputEvent::Key(Key::BackTab), t(140));
    s.update(InputEvent::Key(Key::Enter), t(150));
    assert!(s.report_text().is_some());
    s.update(InputEvent::Key(Key::Escape), t(160));
    assert!(!s.modal_open());
    // Any other key lets go of the banner.
    s.update(InputEvent::Key(Key::Tab), t(170));
    s.update(InputEvent::Key(Key::Home), t(180));
    let (_, pills) = widget_texts(&mut s, 190);
    assert!(pills.iter().all(|p| !p.1));
}

#[test]
fn a_narrow_window_puts_the_buttons_on_a_second_row_inside_the_strip() {
    let planted = Planted::new(REPORT);
    let mut s = shell_of((420, 600));
    with_banner(&mut s, &planted);
    let c = s.chrome(t(0));
    let panel = c
        .widgets
        .iter()
        .find_map(|w| match w {
            Widget::Panel { rect } => Some(*rect),
            _ => None,
        })
        .expect("the strip");
    let text = c
        .widgets
        .iter()
        .find_map(|w| match w {
            Widget::Text { rect, text, .. } if &**text == CRASH_WORDS => Some(*rect),
            _ => None,
        })
        .unwrap();
    let mut buttons = Vec::new();
    for w in &c.widgets {
        if let Widget::Pill { rect, .. } = w {
            buttons.push(*rect);
        }
    }
    assert_eq!(buttons.len(), 3);
    for b in &buttons {
        assert!(b.y >= text.bottom() - 0.01, "{b:?} over the words");
        assert!(b.right() <= panel.right() && b.bottom() <= panel.bottom() + 0.01);
    }
    assert!(panel.h > text.h);
    assert!(s.map_rect().y >= panel.bottom() - 0.01);
}

#[test]
fn the_banner_sits_above_a_stopped_scan_note() {
    let planted = Planted::new(REPORT);
    let mut s = shell();
    s.set_stopped(true);
    let stopped_only = s.map_rect();
    with_banner(&mut s, &planted);
    assert!(s.map_rect().y > stopped_only.y);
    let c = s.chrome(t(0));
    let panels: Vec<Rect> = c
        .widgets
        .iter()
        .filter_map(|w| match w {
            Widget::Panel { rect } => Some(*rect),
            _ => None,
        })
        .collect();
    assert_eq!(panels.len(), 2);
    assert!(panels[0].bottom() <= panels[1].y + 0.01, "{panels:?}");
}

#[test]
fn the_banner_keeps_the_picker_below_it() {
    let planted = Planted::new(REPORT);
    let mut s: Shell<TreeSource> = Shell::new(
        (1200, 800),
        1.0,
        13.0,
        TextEngine::new(),
        Look {
            dark: false,
            accent: None,
        },
    )
    .with_measure(Box::new(Fixed(7.0)));
    s.set_picker(Picker::new(&[]));
    let fx = s.set_crash_notice(Some(planted.notice()), t(10));
    assert!(fx.iter().any(|e| matches!(e, ShellEffect::MapMoved)));
    let strip = s.chrome(t(0)).widgets.iter().find_map(|w| match w {
        Widget::Panel { rect } => Some(*rect),
        _ => None,
    });
    assert!(strip.is_some());
    for item in s.picker_items().unwrap().iter() {
        assert!(item.rect.y >= s.map_rect().y - 0.01);
    }
}

// ---- the viewer -------------------------------------------------------------------------

fn long_report() -> String {
    let mut text = String::from(
        "Bytescape crash report\nFormat: 1\nProgram: bytescape 0.1.0\nPanic:\n    boom\nBacktrace:\n",
    );
    for n in 0..200 {
        text.push_str(&format!("   {n}: some::function_{n}\n"));
    }
    text
}

#[test]
fn a_long_report_scrolls_with_the_wheel_and_the_arrow_keys() {
    let planted = Planted::new(&long_report());
    let mut s = shell();
    with_banner(&mut s, &planted);
    click_pill(&mut s, "View report", 100);
    let first = s.report_text().unwrap();
    assert!(first.contains("Lines 1–"), "{first}");
    assert!(first.contains(" of 206 "), "{first}");
    assert!(first.contains("Format: 1"), "{first}");
    // Wheel down (negative steps) moves further into the report.
    s.update(
        InputEvent::Wheel {
            x: 10.0,
            y: 10.0,
            steps: -2.0,
        },
        t(200),
    );
    let later = s.report_text().unwrap();
    assert!(!later.contains("Format: 1"), "{later}");
    assert!(later.contains("0: some::function_0"), "{later}");
    assert!(later.contains("Lines 7–"), "{later}");
    // The arrow keys move a line at a time, Home goes back to the top.
    s.update(InputEvent::Key(Key::Arrow(Direction::Up)), t(210));
    assert!(s.report_text().unwrap().contains("Lines 6–"));
    s.update(InputEvent::Key(Key::Home), t(220));
    assert!(s.report_text().unwrap().contains("Lines 1–"));
    // It stops at the end.
    s.update(
        InputEvent::Wheel {
            x: 0.0,
            y: 0.0,
            steps: -1000.0,
        },
        t(230),
    );
    let end = s.report_text().unwrap();
    assert!(end.contains("199: some::function_199"), "{end}");
    assert!(end.contains("–206 of 206"), "{end}");
    s.update(InputEvent::Key(Key::Arrow(Direction::Down)), t(240));
    assert!(s.report_text().unwrap().contains("–206 of 206"));
}

#[test]
fn a_hostile_report_cannot_make_the_viewer_larger_than_the_window() {
    let mut body = String::from("Bytescape crash report\nProgram: x\n");
    for n in 0..5_000 {
        body.push_str(&format!(
            "{n} {}\u{7}\u{202e}{}\n",
            "w".repeat(900),
            "é\u{301}".repeat(300)
        ));
    }
    let planted = Planted::new(&body);
    let mut s = shell_of((640, 480));
    with_banner(&mut s, &planted);
    click_pill(&mut s, "View report", 100);
    let popup = s.popup().expect("the viewer");
    let r = popup.card.rect;
    assert!(r.y >= 0.0 && r.bottom() <= 480.0 + 0.01, "{r:?}");
    assert!(r.x >= 0.0 && r.right() <= 640.0 + 0.01, "{r:?}");
    for part in &popup.card.parts {
        if let CardPart::Button { rect, .. } = part {
            assert!(rect.bottom() <= r.bottom() + 0.01 && rect.right() <= r.right() + 0.01);
        }
    }
    let text = s.report_text().unwrap();
    assert!(!text.contains('\u{7}'));
}

// ---- About -------------------------------------------------------------------------------

/// Unix seconds at midnight UTC of a date, counted independently of the code
/// under test.
fn at(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    (era * 146_097 + doe - 719_468) * 86_400
}

fn about_shell(built: &str, now: i64) -> Shell<TreeSource> {
    let mut s = shell();
    s.set_about(AboutInfo::new("0.1.0", built, "x86_64-unknown-linux-gnu"));
    s.set_unix_clock(Box::new(move || now));
    s
}

#[test]
fn f1_opens_the_about_box_with_the_version_and_the_build_age() {
    let mut s = about_shell("2025-08-20", at(2026, 10, 20));
    s.update(InputEvent::Key(Key::About), t(10));
    let text = s.dialog_text().expect("the About box");
    assert!(text.contains("About Bytescape"), "{text}");
    assert!(text.contains("Version 0.1.0 · built 2025-08-20"), "{text}");
    assert!(text.contains("x86_64-unknown-linux-gnu"), "{text}");
    assert!(text.contains("This version is 14 months old"), "{text}");
    assert!(text.contains("never connects to the internet"), "{text}");
    assert_eq!(
        s.dialog_buttons(),
        [
            "Find more free software at RecursiveAI.org",
            "Check for updates",
            "Close"
        ]
    );
}

#[test]
fn a_recent_or_undated_build_has_no_age_line() {
    let mut s = about_shell("2026-10-01", at(2026, 10, 20));
    s.update(InputEvent::Key(Key::About), t(10));
    let text = s.dialog_text().unwrap();
    assert!(text.contains("built 2026-10-01"), "{text}");
    assert!(!text.contains("months old"), "{text}");
    let mut s = about_shell("unknown", at(2030, 1, 1));
    s.update(InputEvent::Key(Key::About), t(10));
    let text = s.dialog_text().unwrap();
    assert!(text.contains("Version 0.1.0"), "{text}");
    assert!(!text.contains("built"), "{text}");
    assert!(!text.contains("months old"), "{text}");
}

#[test]
fn the_about_links_open_the_browser_and_close_closes() {
    let mut s = about_shell("2025-08-20", at(2026, 10, 20));
    s.update(InputEvent::Key(Key::About), t(10));
    let fx = click_popup(&mut s, "Find more free software at RecursiveAI.org", 100);
    assert!(
        fx.iter()
            .any(|e| matches!(e, ShellEffect::OpenLink(u) if *u == "https://recursiveai.org/")),
        "{fx:?}"
    );
    let fx = click_popup(&mut s, "Check for updates", 200);
    assert!(
        fx.iter()
            .any(|e| matches!(e, ShellEffect::OpenLink(u) if *u == RELEASES_URL)),
        "{fx:?}"
    );
    assert!(s.modal_open(), "the box stays while the browser opens");
    click_popup(&mut s, "Close", 300);
    assert!(!s.modal_open());
    // Esc closes it too, and Enter on the default button.
    s.update(InputEvent::Key(Key::About), t(400));
    s.update(InputEvent::Key(Key::Escape), t(410));
    assert!(!s.modal_open());
    s.update(InputEvent::Key(Key::About), t(420));
    s.update(InputEvent::Key(Key::Enter), t(430));
    assert!(!s.modal_open(), "the keyboard starts on Close");
}

#[test]
fn the_about_buttons_stay_inside_the_box_in_a_narrow_window() {
    let mut s = shell_of((420, 500));
    s.set_unix_clock(Box::new(|| at(2026, 10, 20)));
    s.update(InputEvent::Key(Key::About), t(10));
    let popup = s.popup().unwrap();
    let r = popup.card.rect;
    let mut buttons = 0;
    for part in &popup.card.parts {
        if let CardPart::Button { rect, .. } = part {
            buttons += 1;
            assert!(
                rect.x >= r.x - 0.01
                    && rect.right() <= r.right() + 0.01
                    && rect.bottom() <= r.bottom() + 0.01,
                "{rect:?} outside {r:?}"
            );
        }
    }
    assert_eq!(buttons, 3);
}

#[test]
fn the_menu_of_the_program_reaches_about_and_the_releases_page() {
    let mut s = about_shell("2025-08-20", at(2026, 10, 20));
    // A right click on nothing: the status bar.
    let g = s.geometry().clone();
    let (x, y) = centre(g.status);
    s.update(InputEvent::RightClick { x, y }, t(10));
    let rows = s.menu_items();
    assert_eq!(
        rows,
        vec![
            ("About Bytescape".to_string(), "F1".to_string(), true),
            ("Check for updates".to_string(), String::new(), true),
        ]
    );
    let (rx, ry) = popup_button(&s, "About Bytescape");
    s.update(InputEvent::Click { x: rx, y: ry }, t(20));
    assert!(s.dialog_text().unwrap().contains("About Bytescape"));
    s.update(InputEvent::Key(Key::Escape), t(30));
    // The keyboard route: the menu key with nothing selected.
    s.update(InputEvent::Key(Key::ContextMenu), t(40));
    assert_eq!(s.menu_items().len(), 2);
    s.update(InputEvent::Key(Key::Arrow(Direction::Down)), t(50));
    let fx = s.update(InputEvent::Key(Key::Enter), t(60));
    assert!(
        fx.iter()
            .any(|e| matches!(e, ShellEffect::OpenLink(u) if *u == RELEASES_URL)),
        "{fx:?}"
    );
    // A right click on empty map space opens the same menu; on a box, the box's.
    let map = s.map_rect();
    s.update(
        InputEvent::RightClick {
            x: map.x + 1.0,
            y: map.bottom() - 1.0,
        },
        t(70),
    );
    let labels: Vec<String> = s.menu_items().into_iter().map(|r| r.0).collect();
    assert!(
        labels.contains(&"About Bytescape".to_string()) || labels.contains(&"Open".to_string()),
        "{labels:?}"
    );
}

#[test]
fn about_works_from_the_drive_picker_too() {
    let mut s: Shell<TreeSource> = Shell::new(
        (1200, 800),
        1.0,
        13.0,
        TextEngine::new(),
        Look {
            dark: false,
            accent: None,
        },
    )
    .with_measure(Box::new(Fixed(7.0)));
    s.set_picker(Picker::new(&[]));
    s.update(InputEvent::Key(Key::About), t(10));
    assert!(s.dialog_text().unwrap().contains("About Bytescape"));
}

// ---- the accessibility tree --------------------------------------------------------------

fn nodes_of(update: &accesskit::TreeUpdate) -> Vec<&accesskit::Node> {
    update.nodes.iter().map(|(_, n)| n).collect()
}

#[test]
fn the_banner_is_a_group_with_a_working_button_for_each_choice() {
    let planted = Planted::new(REPORT);
    let mut s = shell();
    with_banner(&mut s, &planted);
    let mut tree = AccessTree::new();
    let update = s.access_update(&mut tree, t(20));
    let nodes = nodes_of(&update);
    let group = nodes
        .iter()
        .find(|n| n.role() == Role::Group && n.label() == Some("Crash report"))
        .expect("the banner group");
    assert_eq!(group.children().len(), 4);
    for label in ["View report", "Report on GitHub", "Dismiss"] {
        let n = nodes
            .iter()
            .find(|n| n.role() == Role::Button && n.label() == Some(label))
            .unwrap_or_else(|| panic!("{label}"));
        assert!(n.bounds().is_some() && !n.is_disabled());
    }
    // A screen reader's click on Dismiss does what a pointer's does.
    let id = tree.node_for(AccessTarget::Crash(2)).expect("a node");
    let fx = s.access_action(
        &tree,
        &accesskit::ActionRequest {
            action: accesskit::Action::Click,
            target_tree: accesskit::TreeId::ROOT,
            target_node: id,
            data: None,
        },
        t(30),
    );
    assert!(
        fx.iter().any(|e| matches!(e, ShellEffect::CrashSeen)),
        "{fx:?}"
    );
    s.access_update(&mut tree, t(40));
    assert!(
        tree.node_for(AccessTarget::Crash(0)).is_none(),
        "the banner is gone from the tree"
    );
}

#[test]
fn the_hover_card_is_a_node_with_its_text_path_and_buttons() {
    let mut s = shell();
    let item = s
        .app()
        .unwrap()
        .items()
        .iter()
        .find(|i| i.label.as_deref().is_some_and(|l| &*l.name == "folder1"))
        .cloned()
        .unwrap();
    let mut tree = AccessTree::new();
    // Before the card shows, the tree has none.
    let first = s.access_update(&mut tree, t(1));
    assert!(
        !nodes_of(&first)
            .iter()
            .any(|n| n.description() == Some("Details"))
    );
    let p = (item.rect.x + item.rect.w / 2.0, item.rect.y + 3.0);
    s.update(InputEvent::PointerMoved { x: p.0, y: p.1 }, t(100));
    s.tick(t(700));
    assert!(s.card().is_some());
    let update = s.access_update(&mut tree, t(710));
    let nodes = nodes_of(&update);
    let card = nodes
        .iter()
        .find(|n| {
            n.role() == Role::Group
                && n.label() == Some("folder1")
                && n.description() == Some("Details")
        })
        .expect("the card");
    assert!(card.bounds().is_some());
    assert!(card.children().len() >= 6, "{}", card.children().len());
    // The path copies on a click; the delete button works.
    let path = nodes
        .iter()
        .find(|n| n.role() == Role::Button && n.label() == Some("Copy path"))
        .expect("the path");
    assert_eq!(path.value(), Some("/data/folder1"));
    for label in ["Delete", "Show in file manager", "Open"] {
        let b = nodes
            .iter()
            .find(|n| n.role() == Role::Button && n.label() == Some(label))
            .unwrap_or_else(|| panic!("{label}"));
        assert!(b.bounds().is_some());
    }
    // The text lines are there too.
    assert!(
        nodes
            .iter()
            .any(|n| n.role() == Role::Label && n.value().is_some_and(|v| v.contains("on disk")))
    );
    // Its controls can be clicked through the tree.
    assert!(
        (0..20).any(|i| tree.node_for(AccessTarget::CardPart(i)).is_some()),
        "the card's controls can be clicked"
    );
    // When the card goes, its node goes with it.
    s.update(InputEvent::PointerLeft, t(800));
    assert!(s.card().is_none());
    s.access_update(&mut tree, t(810));
    assert!(tree.node_for(AccessTarget::CardPart(0)).is_none());
}

#[test]
fn every_disabled_card_button_names_its_reason_to_a_screen_reader() {
    let mut s = shell();
    let item = s
        .app()
        .unwrap()
        .items()
        .iter()
        .find(|i| i.label.as_deref().is_some_and(|l| &*l.name == "folder1"))
        .cloned()
        .unwrap();
    let p = (item.rect.x + item.rect.w / 2.0, item.rect.y + 3.0);
    s.update(InputEvent::PointerMoved { x: p.0, y: p.1 }, t(100));
    s.tick(t(700));
    // Every disabled button carries its reason.
    let mut tree = AccessTree::new();
    let update = s.access_update(&mut tree, t(710));
    for n in nodes_of(&update) {
        if n.role() == Role::Button && n.is_disabled() {
            assert!(
                n.description().is_some_and(|d| !d.is_empty()),
                "{:?}",
                n.label()
            );
        }
    }
}

#[test]
fn the_about_box_and_the_report_have_their_own_names_in_the_tree() {
    let planted = Planted::new(REPORT);
    let mut s = about_shell("2025-08-20", at(2026, 10, 20));
    s.update(InputEvent::Key(Key::About), t(10));
    let mut tree = AccessTree::new();
    let update = s.access_update(&mut tree, t(20));
    assert!(
        nodes_of(&update)
            .iter()
            .any(|n| n.role() == Role::Dialog && n.label() == Some("About Bytescape"))
    );
    s.update(InputEvent::Key(Key::Escape), t(30));
    with_banner(&mut s, &planted);
    click_pill(&mut s, "View report", 100);
    let update = s.access_update(&mut tree, t(120));
    assert!(
        nodes_of(&update)
            .iter()
            .any(|n| n.role() == Role::Dialog && n.label() == Some("Crash report"))
    );
}
