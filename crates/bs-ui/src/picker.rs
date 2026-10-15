//! The drive picker: what is shown on launch, how the cards are laid out and
//! how the scans running behind them read out. Everything here is pure; the
//! window code supplies the drives and the scan counters.

use std::path::{Path, PathBuf};

use bs_layout::{format_count, format_size};
use bs_platform::drives::{Capacity, Drive, DriveKind};
use bs_render::{Flags, Item, ItemKind, ItemLabel, Rect, SegmentKind, TextRole, Widget};

use crate::capacity::Breakdown;
use crate::chrome::TextMeasure;
use crate::squarify::squarify;

/// What the program does at launch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StartPlan {
    /// Scan this folder or drive path and show its map.
    OpenPath(PathBuf),
    /// Open this drive (an index into the list given to [`plan_start`]).
    OpenDrive(usize),
    /// Show the picker.
    Picker,
}

/// Decides what the first screen is. A path on the command line always wins.
/// With exactly one drive that is scanned automatically there is nothing to
/// choose, so its map opens at once. Otherwise the picker shows, even with no
/// drive at all, because "Scan a folder…" is still useful there.
pub fn plan_start(drives: &[Drive], cli_path: Option<&Path>) -> StartPlan {
    if let Some(p) = cli_path {
        return StartPlan::OpenPath(p.to_path_buf());
    }
    let mut eligible = drives
        .iter()
        .enumerate()
        .filter(|(_, d)| d.eligible_for_background_scan);
    match (eligible.next(), eligible.next()) {
        (Some((i, _)), None) => StartPlan::OpenDrive(i),
        _ => StartPlan::Picker,
    }
}

/// How a card's scan is going.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CardScan {
    /// Not scanned unless the user picks it (removable and network drives).
    NotScanned,
    /// Queued or just starting.
    Starting,
    /// Running. `ratio` is the share of the used bytes counted, when that
    /// figure means something; otherwise the card shows file counts.
    Running {
        ratio: Option<f32>,
        files: u64,
        bytes: u64,
    },
    /// Finished.
    Done { files: u64 },
    /// Stopped before the end; what was counted is partial.
    Stopped,
    /// Could not run.
    Failed,
}

/// One drive in the picker.
#[derive(Clone, Debug)]
pub struct PickCard {
    /// The position of the drive in the list the picker was made from.
    pub drive_index: usize,
    pub title: String,
    pub detail: String,
    pub capacity: Option<Capacity>,
    /// Scanned in the background from launch.
    pub auto: bool,
    pub scan: CardScan,
    /// Bytes counted so far (snapshot figure, never the running counters).
    pub counted: u64,
    /// The walk finished, so the uncounted gap is a real unreadable part.
    pub complete: bool,
    /// Bytes in the drive's Recycle Bin or Trash.
    pub bin: u64,
}

fn kind_word(kind: DriveKind) -> &'static str {
    match kind {
        DriveKind::Fixed => "Internal",
        DriveKind::Removable => "Removable",
        DriveKind::Network => "Network",
        _ => "Other",
    }
}

impl PickCard {
    /// A card for `drive`.
    pub fn new(drive_index: usize, drive: &Drive) -> Self {
        let mount = drive.mount_point.to_string_lossy().into_owned();
        let title = match drive.label.as_deref().filter(|l| !l.is_empty()) {
            Some(label) => format!("{label} ({mount})"),
            None => mount,
        };
        Self {
            drive_index,
            title,
            detail: format!("{} · {}", kind_word(drive.kind), drive.fs_type),
            capacity: drive.capacity,
            auto: drive.eligible_for_background_scan,
            scan: if drive.eligible_for_background_scan {
                CardScan::Starting
            } else {
                CardScan::NotScanned
            },
            counted: 0,
            complete: false,
            bin: 0,
        }
    }

    /// "412.0 GB of 500.0 GB used".
    pub fn used_text(&self) -> String {
        match &self.capacity {
            Some(c) => format!("{} of {} used", format_size(c.used), format_size(c.total)),
            None => "Size unknown".to_string(),
        }
    }

    /// The scan line under the bar.
    pub fn scan_text(&self) -> String {
        match self.scan {
            CardScan::NotScanned => "Not scanned until you pick it".to_string(),
            CardScan::Starting => "Scanning…".to_string(),
            CardScan::Running {
                ratio,
                files,
                bytes,
            } => {
                let counted = format!("≥ {} · {} files", format_size(bytes), format_count(files));
                match ratio {
                    Some(r) => {
                        format!("Scanning… {:.0}% · {counted}", (r * 100.0).clamp(0.0, 99.0))
                    }
                    None => format!("Scanning… {counted}"),
                }
            }
            CardScan::Done { files } => format!("Scanned · {} files", format_count(files)),
            CardScan::Stopped => "Scan stopped — results incomplete".to_string(),
            CardScan::Failed => "Couldn't scan this drive".to_string(),
        }
    }

    /// The bar's parts as fractions of the drive.
    pub fn bar(&self) -> Vec<(SegmentKind, f32)> {
        match &self.capacity {
            Some(c) => Breakdown::new(c, self.counted, self.complete, self.bin)
                .parts()
                .segments(),
            None => Vec::new(),
        }
    }
}

/// What a point can land on in the picker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PickTarget {
    /// The card at this position.
    Card(usize),
    /// The "Scan a folder…" button.
    Folder,
}

/// The picker's drawing: map items, chrome widgets on top of them, and where
/// to click.
#[derive(Clone, Debug, Default)]
pub struct PickerLayout {
    pub items: Vec<Item>,
    pub widgets: Vec<Widget>,
    pub slots: Vec<(PickTarget, Rect)>,
}

impl PickerLayout {
    /// What is at the point.
    pub fn target_at(&self, x: f32, y: f32) -> Option<PickTarget> {
        self.slots
            .iter()
            .rev()
            .find(|(_, r)| r.contains(x, y))
            .map(|(t, _)| *t)
    }

    /// The item id of a target (cards only).
    pub fn item_id(target: PickTarget) -> Option<u32> {
        match target {
            PickTarget::Card(i) => Some(i as u32 + 1),
            PickTarget::Folder => None,
        }
    }
}

/// Words shown above the row of drives that are not scanned automatically.
pub const OTHER_HEADING: &str = "Other drives — not scanned unless you pick one";
/// The folder button's words.
pub const FOLDER_BUTTON: &str = "Scan a folder…";
/// Shown when there is nothing to scan automatically.
pub const NO_DRIVES: &str = "No internal drives found. Scan a folder instead.";

/// A drive's weight in the card layout: its capacity, but never less than a
/// fifth of the biggest drive's, so a small drive still gets a card that can
/// carry its words.
fn weight(cap: Option<&Capacity>, biggest: u64) -> f64 {
    let own = cap.map_or(0, |c| c.total);
    own.max(biggest / 5).max(1) as f64
}

/// The drive picker's state.
#[derive(Clone, Debug, Default)]
pub struct Picker {
    cards: Vec<PickCard>,
}

impl Picker {
    pub fn new(drives: &[Drive]) -> Self {
        Self {
            cards: drives
                .iter()
                .enumerate()
                .map(|(i, d)| PickCard::new(i, d))
                .collect(),
        }
    }

    pub fn cards(&self) -> &[PickCard] {
        &self.cards
    }

    pub fn card_mut(&mut self, index: usize) -> Option<&mut PickCard> {
        self.cards.get_mut(index)
    }

    /// The card showing the drive at `drive_index`.
    pub fn card_for_drive(&mut self, drive_index: usize) -> Option<&mut PickCard> {
        self.cards.iter_mut().find(|c| c.drive_index == drive_index)
    }

    /// Lays the cards out in `map` (device pixels).
    pub fn layout(
        &self,
        map: Rect,
        em: f32,
        scale: f32,
        measure: &dyn TextMeasure,
    ) -> PickerLayout {
        let mut out = PickerLayout::default();
        let gap = (10.0 * scale).round();
        let area = Rect::new(
            map.x + gap,
            map.y + gap,
            (map.w - 2.0 * gap).max(0.0),
            (map.h - 2.0 * gap).max(0.0),
        );
        let others: Vec<usize> = (0..self.cards.len())
            .filter(|&i| !self.cards[i].auto)
            .collect();
        let autos: Vec<usize> = (0..self.cards.len())
            .filter(|&i| self.cards[i].auto)
            .collect();

        // The bottom strip: the heading and the folder button, then the row
        // of drives that are not scanned automatically.
        let head_h = (em * 1.9).round();
        let row_h = if others.is_empty() {
            0.0
        } else {
            (em * 3.4).round()
        };
        let strip_h = (head_h + row_h + if row_h > 0.0 { gap * 0.5 } else { 0.0 }).min(area.h);
        let strip = Rect::new(area.x, area.bottom() - strip_h, area.w, strip_h);
        let top = Rect::new(area.x, area.y, area.w, (area.h - strip_h - gap).max(0.0));

        // The button, right-aligned in the heading row.
        let label_w = measure
            .width(FOLDER_BUTTON)
            .unwrap_or(FOLDER_BUTTON.chars().count() as f32 * em * 0.56);
        let btn_w = (label_w + em * 2.0).round().min(strip.w);
        let btn = Rect::new(
            strip.right() - btn_w,
            strip.y + ((head_h - em * 1.7) / 2.0).round().max(0.0),
            btn_w,
            (em * 1.7).round(),
        );
        out.widgets.push(Widget::Pill {
            rect: btn,
            text: FOLDER_BUTTON.into(),
            accent: autos.is_empty(),
            hot: false,
        });
        out.slots.push((PickTarget::Folder, btn));
        if !others.is_empty() {
            out.widgets.push(Widget::Text {
                rect: Rect::new(strip.x, strip.y, (strip.w - btn_w - gap).max(0.0), head_h),
                text: OTHER_HEADING.into(),
                role: TextRole::Secondary,
                hot: false,
            });
            let row = Rect::new(strip.x, strip.y + head_h + gap * 0.5, strip.w, row_h);
            let n = others.len() as f32;
            let cell = ((row.w - gap * (n - 1.0)) / n)
                .floor()
                .min((em * 22.0).round());
            for (k, &ci) in others.iter().enumerate() {
                let r = Rect::new(row.x + k as f32 * (cell + gap), row.y, cell, row.h);
                self.push_card(&mut out, ci, r, em, true);
            }
        } else if autos.is_empty() {
            out.widgets.push(Widget::Text {
                rect: Rect::new(strip.x, strip.y, (strip.w - btn_w - gap).max(0.0), head_h),
                text: NO_DRIVES.into(),
                role: TextRole::Secondary,
                hot: false,
            });
        }

        // The scanned drives, sized by capacity.
        let biggest = autos
            .iter()
            .filter_map(|&i| self.cards[i].capacity.map(|c| c.total))
            .max()
            .unwrap_or(0);
        let mut weights: Vec<f64> = autos
            .iter()
            .map(|&i| weight(self.cards[i].capacity.as_ref(), biggest))
            .collect();
        // Cards of equal weight keep the order of the list.
        for (k, w) in weights.iter_mut().enumerate() {
            *w -= k as f64 * 1e-6;
        }
        let rects = squarify(&weights, top);
        for (k, &ci) in autos.iter().enumerate() {
            let r = inset(rects[k], gap / 2.0);
            self.push_card(&mut out, ci, r, em, false);
        }
        out
    }

    fn push_card(&self, out: &mut PickerLayout, ci: usize, rect: Rect, em: f32, compact: bool) {
        let card = &self.cards[ci];
        if rect.w < 2.0 || rect.h < 2.0 {
            return;
        }
        let total = card
            .capacity
            .map_or(String::new(), |c| format_size(c.total));
        out.items.push(Item {
            rect,
            kind: if compact {
                ItemKind::FolderBlock
            } else {
                ItemKind::FolderTitled
            },
            depth: 0,
            label: Some(Box::new(ItemLabel {
                name: card.title.clone().into(),
                size: total.into(),
            })),
            id: ci as u32 + 1,
            category: 0,
            flags: Flags::default(),
        });
        out.slots.push((PickTarget::Card(ci), rect));
        let pad = (em * 0.6).round();
        let line = (em * 1.4).round();
        if compact {
            // Under the title line of the block.
            let y = rect.y + (em * 1.9).round();
            out.widgets.push(Widget::Text {
                rect: Rect::new(rect.x + pad, y, (rect.w - 2.0 * pad).max(0.0), line),
                text: card.used_text().into(),
                role: TextRole::Secondary,
                hot: false,
            });
            return;
        }
        let band = (em * 1.6).round() + 1.0;
        let x = rect.x + pad;
        let w = (rect.w - 2.0 * pad).max(0.0);
        let mut y = rect.y + band + (em * 0.7).round();
        if rect.h < band + em * 4.5 || w < em * 6.0 {
            return;
        }
        let bar_h = (em * 0.8).round().max(6.0);
        out.widgets.push(Widget::Capacity {
            rect: Rect::new(x, y, w, bar_h),
            parts: card.bar(),
        });
        y += bar_h + (em * 0.5).round();
        out.widgets.push(Widget::Text {
            rect: Rect::new(x, y, w, line),
            text: card.used_text().into(),
            role: TextRole::Primary,
            hot: false,
        });
        y += line;
        out.widgets.push(Widget::Text {
            rect: Rect::new(x, y, w, line),
            text: card.scan_text().into(),
            role: TextRole::Secondary,
            hot: false,
        });
        y += line;
        if rect.bottom() - y >= line + pad {
            out.widgets.push(Widget::Text {
                rect: Rect::new(x, y, w, line),
                text: card.detail.clone().into(),
                role: TextRole::Secondary,
                hot: false,
            });
        }
    }

    /// The info bar's words for a card: the name, then the size and how the
    /// scan is going.
    pub fn describe(&self, card: usize) -> Option<(String, String)> {
        let c = self.cards.get(card)?;
        let scan = c.scan_text();
        // The bar is narrow: the byte counts of the scan stay on the card.
        let scan = scan.split(" · ").next().unwrap_or_default();
        Some((c.title.clone(), format!("{} · {scan}", c.used_text())))
    }
}

fn inset(r: Rect, by: f32) -> Rect {
    Rect::new(
        r.x + by,
        r.y + by,
        (r.w - 2.0 * by).max(0.0),
        (r.h - 2.0 * by).max(0.0),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use bs_platform::drives::{DriveId, ReservedKind};

    fn drive(mount: &str, kind: DriveKind, total: u64, used: u64, auto: bool) -> Drive {
        Drive {
            id: DriveId::new(mount),
            mount_point: PathBuf::from(mount),
            label: None,
            fs_type: "ext4".into(),
            kind,
            read_only: false,
            eligible_for_background_scan: auto,
            capacity: Some(Capacity::from_bytes(
                total,
                total - used,
                total - used,
                ReservedKind::FilesystemReserve,
            )),
            capacity_problem: None,
            mounts: Vec::new(),
        }
    }

    struct Fixed;
    impl TextMeasure for Fixed {
        fn width(&self, text: &str) -> Option<f32> {
            Some(text.chars().count() as f32 * 7.0)
        }
    }

    #[test]
    fn a_path_on_the_command_line_skips_the_picker() {
        let d = [
            drive("/", DriveKind::Fixed, 100, 50, true),
            drive("/data", DriveKind::Fixed, 100, 50, true),
        ];
        assert_eq!(
            plan_start(&d, Some(Path::new("/home"))),
            StartPlan::OpenPath(PathBuf::from("/home"))
        );
    }

    #[test]
    fn one_eligible_drive_opens_its_map_and_two_show_the_picker() {
        let one = [
            drive("/", DriveKind::Fixed, 100, 50, true),
            drive("/mnt/usb", DriveKind::Removable, 10, 5, false),
        ];
        assert_eq!(plan_start(&one, None), StartPlan::OpenDrive(0));
        let two = [
            drive("/", DriveKind::Fixed, 100, 50, true),
            drive("/data", DriveKind::Fixed, 100, 50, true),
        ];
        assert_eq!(plan_start(&two, None), StartPlan::Picker);
        assert_eq!(plan_start(&[], None), StartPlan::Picker);
        let none_eligible = [drive("/mnt/usb", DriveKind::Removable, 10, 5, false)];
        assert_eq!(plan_start(&none_eligible, None), StartPlan::Picker);
    }

    #[test]
    fn cards_are_sized_by_capacity_and_the_others_stay_separate() {
        let drives = [
            drive(
                "/",
                DriveKind::Fixed,
                1_000_000_000_000,
                400_000_000_000,
                true,
            ),
            drive(
                "/data",
                DriveKind::Fixed,
                250_000_000_000,
                100_000_000_000,
                true,
            ),
            drive(
                "/mnt/usb",
                DriveKind::Removable,
                16_000_000_000,
                4_000_000_000,
                false,
            ),
        ];
        let picker = Picker::new(&drives);
        let map = Rect::new(0.0, 60.0, 1200.0, 700.0);
        let l = picker.layout(map, 13.0, 1.0, &Fixed);
        let item = |id: u32| l.items.iter().find(|i| i.id == id).expect("card");
        let area = |i: &Item| i.rect.w * i.rect.h;
        assert!(
            area(item(1)) > area(item(2)) * 2.0,
            "bigger drive, bigger card"
        );
        // The removable drive is a compact block in the strip below.
        let usb = item(3);
        assert_eq!(usb.kind, ItemKind::FolderBlock);
        assert!(usb.rect.y > item(1).rect.bottom() - 1.0);
        // Everything stays inside the map.
        for i in &l.items {
            assert!(i.rect.x >= map.x && i.rect.right() <= map.right(), "{i:?}");
            assert!(
                i.rect.y >= map.y && i.rect.bottom() <= map.bottom(),
                "{i:?}"
            );
        }
        assert_eq!(
            l.target_at(item(2).rect.x + 5.0, item(2).rect.y + 5.0),
            Some(PickTarget::Card(1))
        );
        assert!(l.slots.iter().any(|(t, _)| *t == PickTarget::Folder));
    }

    #[test]
    fn a_small_drive_still_gets_a_card_that_can_carry_its_words() {
        let drives = [
            drive(
                "/",
                DriveKind::Fixed,
                4_000_000_000_000,
                1_000_000_000_000,
                true,
            ),
            drive(
                "/tiny",
                DriveKind::Fixed,
                8_000_000_000,
                1_000_000_000,
                true,
            ),
        ];
        let l = Picker::new(&drives).layout(Rect::new(0.0, 0.0, 1000.0, 600.0), 13.0, 1.0, &Fixed);
        let tiny = l.items.iter().find(|i| i.id == 2).expect("card");
        assert!(tiny.rect.w * tiny.rect.h > 20_000.0, "{:?}", tiny.rect);
    }

    #[test]
    fn card_words_are_honest_about_progress() {
        let d = drive(
            "/",
            DriveKind::Fixed,
            500_000_000_000,
            412_000_000_000,
            true,
        );
        let mut card = PickCard::new(0, &d);
        assert_eq!(card.used_text(), "412.0 GB of 500.0 GB used");
        assert_eq!(card.scan_text(), "Scanning…");
        card.scan = CardScan::Running {
            ratio: Some(0.37),
            files: 1_200_000,
            bytes: 150_000_000_000,
        };
        let text = card.scan_text();
        assert!(
            text.contains("37%") && text.contains("≥ 150.0 GB"),
            "{text}"
        );
        card.scan = CardScan::Running {
            ratio: None,
            files: 10,
            bytes: 5,
        };
        assert!(!card.scan_text().contains('%'));
        card.scan = CardScan::Stopped;
        assert!(card.scan_text().contains("results incomplete"));
        let banned = ["safe", "junk", "reclaim", "clean up", "freed"];
        for s in [
            card.scan_text(),
            OTHER_HEADING.to_string(),
            NO_DRIVES.to_string(),
        ] {
            for b in banned {
                assert!(!s.to_lowercase().contains(b), "{s}");
            }
        }
    }

    #[test]
    fn hostile_labels_do_not_break_the_layout() {
        let mut d = drive("/", DriveKind::Fixed, 1000, 500, true);
        d.label = Some("\u{202e}evil\u{200b}\u{0301}".repeat(500));
        let d2 = drive("/b", DriveKind::Fixed, 1000, 500, true);
        let l = Picker::new(&[d, d2]).layout(Rect::new(0.0, 0.0, 800.0, 500.0), 13.0, 1.0, &Fixed);
        assert_eq!(l.items.len(), 2);
    }

    #[test]
    fn without_drives_the_folder_button_and_a_message_remain() {
        let l = Picker::new(&[]).layout(Rect::new(0.0, 0.0, 800.0, 500.0), 13.0, 1.0, &Fixed);
        assert!(l.items.is_empty());
        assert!(
            l.widgets
                .iter()
                .any(|w| matches!(w, Widget::Text { text, .. } if &**text == NO_DRIVES))
        );
    }
}
