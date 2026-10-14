//! The words of the info bar: one line about the box under the pointer or the
//! selected boxes.

use crate::ItemInfo;

/// The info bar's text, split so the name can shorten while the facts stay
/// whole.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InfoLine {
    pub name: String,
    pub details: String,
}

/// A share as text: `<0.1%`, `4.2%`, `37%`.
pub fn percent(part: u64, whole: u64) -> Option<String> {
    if whole == 0 {
        return None;
    }
    let p = part as f64 / whole as f64 * 100.0;
    Some(if p < 0.1 {
        "<0.1%".to_string()
    } else if p < 10.0 {
        format!("{p:.1}%")
    } else {
        format!("{:.0}%", p.min(100.0))
    })
}

/// Describes boxes for the info bar. Sizes that are still being counted carry
/// "≥", and so do the shares computed from them. `drive_total` is the size of
/// the whole drive, for "of drive".
pub fn describe(infos: &[ItemInfo], drive_total: Option<u64>) -> Option<InfoLine> {
    match infos {
        [] => None,
        [one] => Some(describe_one(one, drive_total)),
        many => {
            let bytes: u64 = many.iter().map(|i| i.bytes).sum();
            let counting = many.iter().any(|i| i.in_progress);
            let bound = if counting { "≥ " } else { "" };
            Some(InfoLine {
                name: format!(
                    "{} items selected",
                    bs_layout::format_count(many.len() as u64)
                ),
                details: format!("{bound}{}", bs_layout::format_size(bytes)),
            })
        }
    }
}

fn describe_one(i: &ItemInfo, drive_total: Option<u64>) -> InfoLine {
    let bound = if i.in_progress { "≥ " } else { "" };
    let mut parts = vec![format!("{bound}{}", bs_layout::format_size(i.bytes))];
    if let Some(p) = i.parent_bytes.and_then(|w| percent(i.bytes, w)) {
        parts.push(format!("{bound}{p} of parent"));
    }
    if let Some(p) = drive_total.and_then(|w| percent(i.bytes, w)) {
        parts.push(format!("{bound}{p} of drive"));
    }
    if i.is_folder {
        let n = i.files;
        parts.push(format!(
            "{} {}",
            bs_layout::format_count(n),
            if n == 1 { "file" } else { "files" }
        ));
    }
    InfoLine {
        name: i.name.clone(),
        details: parts.join(" · "),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder() -> ItemInfo {
        ItemInfo {
            name: "Videos".into(),
            bytes: 41_200_000_000,
            parent_bytes: Some(100_000_000_000),
            files: 1234,
            in_progress: false,
            is_folder: true,
        }
    }

    #[test]
    fn a_folder_shows_size_shares_and_file_count() {
        let line = describe(&[folder()], Some(500_000_000_000)).expect("line");
        assert_eq!(line.name, "Videos");
        assert_eq!(
            line.details,
            "41.2 GB · 41% of parent · 8.2% of drive · 1,234 files"
        );
    }

    #[test]
    fn sizes_still_being_counted_carry_the_bound() {
        let mut f = folder();
        f.in_progress = true;
        let line = describe(&[f], Some(500_000_000_000)).expect("line");
        assert!(line.details.starts_with("≥ 41.2 GB"), "{}", line.details);
        assert!(line.details.contains("≥ 41% of parent"));
        assert!(line.details.contains("≥ 8.2% of drive"));
    }

    #[test]
    fn a_file_has_no_file_count_and_missing_totals_are_left_out() {
        let file = ItemInfo {
            name: "a.mkv".into(),
            bytes: 5_000,
            parent_bytes: None,
            files: 1,
            in_progress: false,
            is_folder: false,
        };
        let line = describe(&[file], None).expect("line");
        assert_eq!(line.details, "5.0 kB");
    }

    #[test]
    fn tiny_shares_do_not_round_to_zero() {
        assert_eq!(percent(1, 10_000).as_deref(), Some("<0.1%"));
        assert_eq!(percent(5, 100).as_deref(), Some("5.0%"));
        assert_eq!(percent(100, 100).as_deref(), Some("100%"));
        assert_eq!(percent(1, 0), None);
    }

    #[test]
    fn several_boxes_add_up() {
        let mut a = folder();
        a.bytes = 10;
        let mut b = folder();
        b.bytes = 30;
        b.in_progress = true;
        let line = describe(&[a, b], None).expect("line");
        assert_eq!(line.name, "2 items selected");
        assert_eq!(line.details, "≥ 40 B");
    }

    #[test]
    fn hostile_names_pass_through_unchanged() {
        let mut f = folder();
        f.name = "a\u{202e}b\u{200b}\u{0301}".repeat(500);
        let line = describe(&[f.clone()], None).expect("line");
        assert_eq!(line.name, f.name);
    }
}
