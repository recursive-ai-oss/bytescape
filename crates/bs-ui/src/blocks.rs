//! The drive-level blocks beside a drive's folders: free space, what could not
//! be read, what the filesystem or a quota holds back, and the Recycle Bin or
//! Trash. They are laid out with the drive's top view, and each states its own
//! bytes in words, so the picture never claims more than the system reported.

use bs_layout::{Metrics, format_size};
use bs_platform::bins::BinName;
use bs_render::{Flags, Item, ItemKind, ItemLabel, Rect};

use crate::ItemInfo;
use crate::capacity::Breakdown;
use crate::elevation::UnreadableAs;
use crate::squarify::squarify;

/// Ids of these blocks start here, far above any folder or group id.
pub const SPECIAL_ID_BASE: u32 = 0xF000_0000;

/// A special block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Special {
    Free,
    Unreadable,
    Reserved,
    RecycleBin,
}

/// Each block never takes less than this share of the drive view, so a small
/// one can still be seen and clicked.
const MIN_SHARE: f64 = 0.02;

/// Bytes of the special blocks for one drive.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DriveBlocks {
    pub free: u64,
    pub unreadable: u64,
    pub reserved: u64,
    pub recycle_bin: u64,
    /// The reserved bytes are a per-user quota rather than a filesystem
    /// reserve.
    pub quota: bool,
    /// What the Unreadable block says about the gap.
    pub unreadable_as: UnreadableAs,
}

impl From<&Breakdown> for DriveBlocks {
    fn from(b: &Breakdown) -> Self {
        Self {
            free: b.free,
            unreadable: b.unreadable,
            reserved: b.reserved,
            recycle_bin: b.recycle_bin,
            quota: b.quota,
            unreadable_as: UnreadableAs::Grant,
        }
    }
}

/// A size from `format_size` rounded to a whole number of its unit:
/// "14.1 GB" becomes "14 GB". The number is rounded, never cut off.
fn whole_size(size: &str) -> String {
    match size.split_once(' ') {
        Some((number, unit)) => match number.parse::<f64>() {
            Ok(n) => format!("{} {unit}", n.round() as u64),
            Err(_) => size.to_string(),
        },
        None => size.to_string(),
    }
}

impl DriveBlocks {
    fn entries(&self) -> Vec<(Special, u64)> {
        [
            (Special::Free, self.free),
            (Special::Unreadable, self.unreadable),
            (Special::Reserved, self.reserved),
            (Special::RecycleBin, self.recycle_bin),
        ]
        .into_iter()
        .filter(|&(_, b)| b > 0)
        .collect()
    }

    /// The bytes of one block.
    pub fn bytes(&self, special: Special) -> u64 {
        match special {
            Special::Free => self.free,
            Special::Unreadable => self.unreadable,
            Special::Reserved => self.reserved,
            Special::RecycleBin => self.recycle_bin,
        }
    }

    /// The words on the block.
    pub fn label(&self, special: Special) -> String {
        let size = format_size(self.bytes(special));
        match special {
            Special::Free => format!("Free · {size}"),
            Special::Unreadable => match self.unreadable_as {
                UnreadableAs::Grant => {
                    format!("Couldn't read: {size} — Grant access to see inside")
                }
                UnreadableAs::Reading => {
                    format!("Couldn't read: {size} — Reading with administrator rights…")
                }
                UnreadableAs::Plain => format!("Couldn't read: {size}"),
                UnreadableAs::Overhead => {
                    format!("Filesystem overhead: {size}")
                }
                UnreadableAs::Unavailable => {
                    format!("Couldn't read: {size} — Administrator scan isn't available here")
                }
                UnreadableAs::Edition => format!(
                    "Couldn't read: {size} — System folders can't be scanned in this edition"
                ),
            },
            Special::Reserved if self.quota => format!("Beyond your disk quota · {size}"),
            Special::Reserved => format!("Reserved by filesystem · {size}"),
            Special::RecycleBin => format!("{} · {size}", BinName::platform().as_str()),
        }
    }

    /// The words a block can wear, from the fullest to the shortest. A narrow
    /// block takes the first that fits: the name is shortened or dropped
    /// first, then the size loses its tenth, and the number itself is never
    /// cut.
    pub fn label_variants(&self, special: Special) -> Vec<String> {
        let size = format_size(self.bytes(special));
        let whole = whole_size(&size);
        let mut out = vec![self.label(special)];
        match special {
            Special::Free => {}
            Special::Unreadable => {
                let name = if self.unreadable_as == UnreadableAs::Overhead {
                    "Filesystem overhead"
                } else {
                    "Couldn't read"
                };
                out.push(format!("{name}: {size}"));
            }
            Special::Reserved if self.quota => out.push(format!("Quota · {size}")),
            Special::Reserved => out.push(format!("Reserved · {size}")),
            Special::RecycleBin => {}
        }
        out.push(size);
        out.push(whole);
        out.dedup();
        out
    }

    /// The id of a block's box.
    pub fn id(special: Special) -> u32 {
        SPECIAL_ID_BASE
            + match special {
                Special::Free => 0,
                Special::Unreadable => 1,
                Special::Reserved => 2,
                Special::RecycleBin => 3,
            }
    }

    /// Which block an id belongs to.
    pub fn special_of(id: u32) -> Option<Special> {
        match id.checked_sub(SPECIAL_ID_BASE)? {
            0 => Some(Special::Free),
            1 => Some(Special::Unreadable),
            2 => Some(Special::Reserved),
            3 => Some(Special::RecycleBin),
            _ => None,
        }
    }

    /// What the info bar says about a block.
    pub fn info(&self, id: u32) -> Option<ItemInfo> {
        let special = Self::special_of(id)?;
        let name = match special {
            Special::Free => "Free space".to_string(),
            Special::Unreadable if self.unreadable_as == UnreadableAs::Overhead => {
                "Filesystem overhead".to_string()
            }
            Special::Unreadable => "Couldn't read".to_string(),
            Special::Reserved if self.quota => "Beyond your disk quota".to_string(),
            Special::Reserved => "Reserved by filesystem".to_string(),
            Special::RecycleBin => BinName::platform().as_str().to_string(),
        };
        let bytes = self.bytes(special);
        if bytes == 0 {
            return None;
        }
        Some(ItemInfo {
            name,
            bytes,
            parent_bytes: None,
            files: 0,
            in_progress: false,
            is_folder: false,
        })
    }

    /// The fullest wording of a block that fits `width` device pixels, judged
    /// by a generous width per character (the real font is not at hand here).
    /// When nothing fits, the shortest: the number alone.
    fn fitted_label(&self, special: Special, width: f32, metrics: Metrics) -> String {
        let room = width - 2.0 * (4.0 * metrics.scale).round();
        let variants = self.label_variants(special);
        variants
            .iter()
            .find(|v| v.chars().count() as f32 * metrics.em * 0.62 <= room)
            .or(variants.last())
            .cloned()
            .unwrap_or_default()
    }

    /// Splits `area` between the drive's folders (`folders_bytes` counted)
    /// and the blocks. Returns the part for the folders and the block boxes.
    /// With no blocks the folders keep the whole area.
    pub fn place(&self, folders_bytes: u64, area: Rect, metrics: Metrics) -> (Rect, Vec<Item>) {
        let entries = self.entries();
        if entries.is_empty() {
            return (area, Vec::new());
        }
        let total: f64 = folders_bytes as f64 + entries.iter().map(|e| e.1 as f64).sum::<f64>();
        let floor = total * MIN_SHARE;
        let mut weights = vec![(folders_bytes as f64).max(floor)];
        weights.extend(entries.iter().map(|e| (e.1 as f64).max(floor)));
        let rects = squarify(&weights, area);
        let items = entries
            .iter()
            .enumerate()
            .map(|(k, &(special, _))| Item {
                rect: rects[k + 1],
                kind: match special {
                    Special::Free => ItemKind::Free,
                    Special::Unreadable => ItemKind::Unreadable,
                    Special::Reserved => ItemKind::Reserved,
                    Special::RecycleBin => ItemKind::RecycleBin,
                },
                depth: 0,
                label: Some(Box::new(ItemLabel {
                    name: self.fitted_label(special, rects[k + 1].w, metrics).into(),
                    size: "".into(),
                })),
                id: Self::id(special),
                category: 0,
                flags: Flags::default(),
            })
            .collect();
        (rects[0], items)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const METRICS: Metrics = Metrics {
        em: 13.0,
        scale: 1.0,
    };

    fn blocks() -> DriveBlocks {
        DriveBlocks {
            free: 112_000_000_000,
            unreadable: 23_400_000_000,
            reserved: 18_000_000_000,
            recycle_bin: 14_100_000_000,
            quota: false,
            unreadable_as: UnreadableAs::Grant,
        }
    }

    #[test]
    fn the_four_blocks_carry_their_own_bytes_in_words() {
        let b = blocks();
        assert_eq!(b.label(Special::Free), "Free · 112.0 GB");
        assert_eq!(
            b.label(Special::Unreadable),
            "Couldn't read: 23.4 GB — Grant access to see inside"
        );
        assert_eq!(
            b.label(Special::Reserved),
            "Reserved by filesystem · 18.0 GB"
        );
        let bin = b.label(Special::RecycleBin);
        assert!(
            bin.ends_with("· 14.1 GB") && (bin.starts_with("Trash") || bin.starts_with("Recycle"))
        );
    }

    #[test]
    fn a_quota_is_not_called_a_filesystem_reserve() {
        let b = DriveBlocks {
            reserved: 5_000_000_000,
            quota: true,
            ..DriveBlocks::default()
        };
        assert_eq!(
            b.label(Special::Reserved),
            "Beyond your disk quota · 5.0 GB"
        );
        assert_eq!(
            b.info(DriveBlocks::id(Special::Reserved))
                .expect("info")
                .name,
            "Beyond your disk quota"
        );
    }

    #[test]
    fn blocks_sit_beside_the_folders_without_overlap_and_all_four_show() {
        let area = Rect::new(0.0, 60.0, 1200.0, 700.0);
        let (files, items) = blocks().place(380_000_000_000, area, METRICS);
        assert_eq!(items.len(), 4);
        let mut all = vec![files];
        all.extend(items.iter().map(|i| i.rect));
        for (i, a) in all.iter().enumerate() {
            assert!(a.w > 0.0 && a.h > 0.0, "{a:?}");
            assert!(a.x >= area.x && a.right() <= area.right() && a.bottom() <= area.bottom());
            for b in &all[i + 1..] {
                let ox = (a.right().min(b.right()) - a.x.max(b.x)).max(0.0);
                let oy = (a.bottom().min(b.bottom()) - a.y.max(b.y)).max(0.0);
                assert!(ox * oy < 0.5, "{a:?} overlaps {b:?}");
            }
        }
        let kinds: Vec<_> = items.iter().map(|i| i.kind).collect();
        for k in [
            ItemKind::Free,
            ItemKind::Unreadable,
            ItemKind::Reserved,
            ItemKind::RecycleBin,
        ] {
            assert!(kinds.contains(&k));
        }
        assert!(
            items
                .iter()
                .all(|i| i.id >= SPECIAL_ID_BASE && i.depth == 0)
        );
    }

    #[test]
    fn empty_blocks_are_left_out_and_a_tiny_one_stays_clickable() {
        let b = DriveBlocks {
            free: 1_000_000_000_000,
            unreadable: 1_000,
            ..DriveBlocks::default()
        };
        let (_, items) = b.place(
            1_000_000_000_000,
            Rect::new(0.0, 0.0, 1000.0, 600.0),
            METRICS,
        );
        assert_eq!(items.len(), 2);
        let tiny = items
            .iter()
            .find(|i| i.kind == ItemKind::Unreadable)
            .expect("block");
        assert!(tiny.rect.w * tiny.rect.h > 5000.0, "{:?}", tiny.rect);
        assert_eq!(
            DriveBlocks::default()
                .place(5, Rect::new(0.0, 0.0, 10.0, 10.0), METRICS)
                .1
                .len(),
            0
        );
    }

    #[test]
    fn ids_round_trip_and_normal_ids_are_not_blocks() {
        for s in [
            Special::Free,
            Special::Unreadable,
            Special::Reserved,
            Special::RecycleBin,
        ] {
            assert_eq!(DriveBlocks::special_of(DriveBlocks::id(s)), Some(s));
        }
        assert_eq!(DriveBlocks::special_of(17), None);
        assert_eq!(DriveBlocks::special_of(SPECIAL_ID_BASE + 9), None);
    }

    #[test]
    fn the_unreadable_block_words_itself_by_what_can_be_done() {
        let mk = |unreadable_as| DriveBlocks {
            unreadable: 23_400_000_000,
            unreadable_as,
            ..DriveBlocks::default()
        };
        let grant = mk(UnreadableAs::Grant).label(Special::Unreadable);
        assert!(grant.contains("Grant access to see inside"), "{grant}");
        let reading = mk(UnreadableAs::Reading).label(Special::Unreadable);
        assert!(reading.contains("administrator rights"), "{reading}");
        let overhead = mk(UnreadableAs::Overhead);
        let text = overhead.label(Special::Unreadable);
        assert!(text.starts_with("Filesystem overhead: 23.4 GB"), "{text}");
        assert!(!text.contains("Couldn't read"));
        assert_eq!(
            overhead
                .info(DriveBlocks::id(Special::Unreadable))
                .map(|i| i.name),
            Some("Filesystem overhead".to_string())
        );
        let edition = mk(UnreadableAs::Edition).label(Special::Unreadable);
        assert!(
            edition.contains("System folders can't be scanned in this edition"),
            "{edition}"
        );
    }

    #[test]
    fn a_narrow_block_keeps_its_size_and_loses_the_name_first() {
        let b = DriveBlocks {
            recycle_bin: 14_100_000_000,
            free: 112_000_000_000,
            unreadable: 23_400_000_000,
            reserved: 18_000_000_000,
            ..DriveBlocks::default()
        };
        // The fullest wording first, the number alone last.
        let bin = b.label_variants(Special::RecycleBin);
        assert_eq!(bin.last().map(String::as_str), Some("14 GB"));
        assert!(bin.contains(&"14.1 GB".to_string()), "{bin:?}");
        let unreadable = b.label_variants(Special::Unreadable);
        assert!(unreadable[0].contains("Grant access"));
        assert!(unreadable.contains(&"Couldn't read: 23.4 GB".to_string()));
        let reserved = b.label_variants(Special::Reserved);
        assert!(reserved.contains(&"Reserved \u{b7} 18.0 GB".to_string()));
        // Every wording of every block carries its number.
        for (special, number) in [
            (Special::Free, "112"),
            (Special::Unreadable, "23"),
            (Special::Reserved, "18"),
            (Special::RecycleBin, "14"),
        ] {
            for v in b.label_variants(special) {
                assert!(v.contains(number), "{v}");
            }
        }
        // A narrower and narrower bin block never shows "Trash\u{2026} GB".
        let mut seen = std::collections::BTreeSet::new();
        for w in [400.0, 160.0, 110.0, 90.0, 60.0] {
            let text = b.fitted_label(Special::RecycleBin, w, METRICS);
            assert!(text.contains("14"), "{w}: {text}");
            assert!(!text.contains('\u{2026}'), "{w}: {text}");
            seen.insert(text);
        }
        assert!(
            seen.len() >= 3,
            "the wording changes with the width: {seen:?}"
        );
        assert!(seen.contains("14 GB"), "{seen:?}");
        // Placed in a real layout, the label is the fitted one.
        let (_, items) = b.place(380_000_000_000, Rect::new(0.0, 0.0, 600.0, 400.0), METRICS);
        for item in items {
            let name = item
                .label
                .as_ref()
                .map(|l| l.name.to_string())
                .unwrap_or_default();
            assert!(!name.is_empty());
            assert!(
                name.chars().count() as f32 * METRICS.em * 0.62 <= item.rect.w - 8.0
                    || !name.contains(' '),
                "{name} in {}",
                item.rect.w
            );
        }
    }
}
