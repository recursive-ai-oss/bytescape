//! The Recycle Bin rules that need no Windows call, so they are tested on
//! every system.

use super::PermanentReason;
use crate::bins::BinPolicy;
use crate::drives::{Drive, DriveKind};
use std::path::Path;

#[cfg(test)]
pub(crate) use bs_fsops::winpath::same_path;
#[cfg(test)]
pub(crate) use bs_fsops::winpath::strip_extended_prefix;
use bs_fsops::winpath::{fold_units, is_inside_units, strip_extended_prefix_units};

/// Why the Recycle Bin would not take an item, if it would not.
///
/// - Only fixed drives have a Recycle Bin. A file deleted from a removable
///   drive, a network share or an optical disc is removed at once.
/// - The user may have set the volume to remove files at once.
/// - An item larger than the volume's bin limit is removed at once.
///
/// When the drive is not known the answer is unknown, which is not "yes".
pub(crate) fn objection(
    drive: Option<&Drive>,
    policy: BinPolicy,
    item_bytes: u64,
) -> Option<PermanentReason> {
    let Some(drive) = drive else {
        return Some(PermanentReason::BinUnknown);
    };
    if drive.kind != DriveKind::Fixed {
        return Some(PermanentReason::NoTrashOnVolume);
    }
    if policy.delete_permanently == Some(true) {
        return Some(PermanentReason::BinDisabled);
    }
    if let Some(limit) = policy.max_bytes
        && item_bytes > limit
    {
        return Some(PermanentReason::TooLargeForBin {
            item_bytes,
            limit_bytes: limit,
        });
    }
    None
}

/// Whether the original path a Recycle Bin record gives is the place whose
/// 16-bit units are `path`, ignoring case and the `\\?\` prefix.
///
/// The record holds text, and a path can hold a lone surrogate that no text
/// can: compared as text, such a path would count as the same place as one
/// with the replacement character at that spot, and the wrong bin item would
/// be taken for the one that was recycled.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn same_place(original: &str, path: &[u16]) -> bool {
    let original: Vec<u16> = original.encode_utf16().collect();
    bs_fsops::winpath::same_path_units(&original, path)
}

/// The 16-bit units of a path. Paths are compared and handed to the shell as
/// units, never as text: a name can hold a lone surrogate that text would turn
/// into U+FFFD, which is the name of a different file.
#[cfg(windows)]
pub(crate) fn units_of_path(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str().encode_wide().collect()
}

/// The units of a path on a system whose paths are not 16-bit; only the tests
/// of the pure rules run there.
#[cfg(not(windows))]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn units_of_path(path: &Path) -> Vec<u16> {
    path.to_string_lossy().encode_utf16().collect()
}

/// Whether `path` is `root` or lies below it, comparing as Windows does:
/// without regard to case, to `/` against `\`, or to the `\\?\` prefix, and
/// by exact 16-bit units.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn is_inside(root: &[u16], path: &[u16]) -> bool {
    is_inside_units(root, path)
}

/// The form of a path the shell's item parser accepts. It refuses the verbatim
/// prefix (`\\?\`) with an invalid-argument error, so a drive path loses it and a
/// UNC path becomes `\\host\share\...`. Forward slashes become backslashes. A
/// path that still has a verbatim or device prefix (`\\?\Volume{...}`,
/// `\\.\`) cannot be turned into an ordinary one, and gives `None`.
///
/// Every unit is kept as it is, so the shell is asked for the item that was
/// chosen and not for a look-alike.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn shell_path(path: &[u16]) -> Option<Vec<u16>> {
    const BACKSLASH: u16 = b'\\' as u16;
    const SLASH: u16 = b'/' as u16;
    const QUESTION: u16 = b'?' as u16;
    const DOT: u16 = b'.' as u16;
    let plain: Vec<u16> = strip_extended_prefix_units(path)
        .into_iter()
        .map(|unit| if unit == SLASH { BACKSLASH } else { unit })
        .collect();
    let ordinary = !plain.is_empty()
        && !matches!(
            plain.as_slice(),
            [BACKSLASH, BACKSLASH, QUESTION | DOT, BACKSLASH, ..]
        )
        && !plain.contains(&0);
    ordinary.then_some(plain)
}

/// The drive that holds a path: the one whose root is the longest match.
pub(crate) fn drive_for<'a>(drives: &'a [Drive], path: &Path) -> Option<&'a Drive> {
    drive_for_units(drives, &units_of_path(path))
}

/// [`drive_for`] for a path held as 16-bit units.
pub(crate) fn drive_for_units<'a>(drives: &'a [Drive], path: &[u16]) -> Option<&'a Drive> {
    drives
        .iter()
        .filter_map(|drive| {
            let root = units_of_path(&drive.mount_point);
            if !is_inside_units(&root, path) {
                return None;
            }
            // Longer roots are the more specific ones.
            let length = fold_units(&strip_extended_prefix_units(&root))
                .iter()
                .rposition(|unit| *unit != u16::from(b'\\') && *unit != u16::from(b'/'))
                .map_or(0, |last| last + 1);
            Some((length, drive))
        })
        .max_by_key(|(length, _)| *length)
        .map(|(_, drive)| drive)
}

/// What a `$I` file in the Recycle Bin records.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RecycleRecord {
    /// The file size before it was recycled.
    pub size: u64,
    /// When it was recycled, as a Windows file time.
    pub deleted_at: u64,
    /// The original path.
    pub original: String,
}

const MAX_RECORD_CHARS: usize = 32 * 1024;

fn utf16(bytes: &[u8]) -> Option<String> {
    let (pairs, rest) = bytes.as_chunks::<2>();
    if !rest.is_empty() {
        return None;
    }
    let units: Vec<u16> = pairs
        .iter()
        .map(|pair| u16::from_le_bytes(*pair))
        .take_while(|&unit| unit != 0)
        .collect();
    String::from_utf16(&units).ok()
}

/// Reads a `$I` file. Version 1 (before Windows 10) has a fixed 520-byte path
/// field; version 2 has the path length in characters first.
pub(crate) fn parse_record(bytes: &[u8]) -> Option<RecycleRecord> {
    let word = |at: usize| -> Option<u64> {
        let raw: [u8; 8] = bytes.get(at..at + 8)?.try_into().ok()?;
        Some(u64::from_le_bytes(raw))
    };
    let version = word(0)?;
    let size = word(8)?;
    let deleted_at = word(16)?;
    let original = match version {
        1 => utf16(
            bytes
                .get(24..)?
                .get(..520.min(bytes.len().saturating_sub(24)))?,
        )?,
        2 => {
            let raw: [u8; 4] = bytes.get(24..28)?.try_into().ok()?;
            let chars = usize::try_from(u32::from_le_bytes(raw)).ok()?;
            if chars == 0 || chars > MAX_RECORD_CHARS {
                return None;
            }
            utf16(bytes.get(28..)?.get(..chars * 2)?)?
        }
        _ => return None,
    };
    (!original.is_empty()).then_some(RecycleRecord {
        size,
        deleted_at,
        original,
    })
}

/// The `$I` file that goes with a recycled `$R` file: same name with the
/// letter after the dollar sign changed. `None` for any other name. The name
/// is kept unit for unit, since the bin names its files after the original's
/// extension.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn info_for(recycled: &[u16]) -> Option<Vec<u16>> {
    let split = recycled
        .iter()
        .rposition(|unit| *unit == u16::from(b'\\') || *unit == u16::from(b'/'))?;
    let (folder, name) = recycled.split_at(split + 1);
    let rest = name.strip_prefix(&[u16::from(b'$'), u16::from(b'R')])?;
    if rest.is_empty() {
        return None;
    }
    let mut out = folder.to_vec();
    out.extend([u16::from(b'$'), u16::from(b'I')]);
    out.extend_from_slice(rest);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drives::{DriveId, Mount};
    use std::path::PathBuf;

    fn drive(kind: DriveKind, root: &str) -> Drive {
        Drive {
            id: DriveId::new(format!("volume:{root}")),
            mount_point: PathBuf::from(root),
            label: None,
            fs_type: "NTFS".into(),
            kind,
            read_only: false,
            eligible_for_background_scan: false,
            capacity: None,
            capacity_problem: None,
            mounts: vec![Mount {
                mount_point: PathBuf::from(root),
                mount_id: None,
                root: PathBuf::from("\\"),
                covered_by_other_mount: false,
            }],
        }
    }

    fn policy(max: Option<u64>, nuke: Option<bool>) -> BinPolicy {
        BinPolicy {
            max_bytes: max,
            delete_permanently: nuke,
        }
    }

    #[test]
    fn a_fixed_drive_with_room_takes_the_item() {
        let c = drive(DriveKind::Fixed, r"C:\");
        assert_eq!(
            objection(Some(&c), policy(Some(1000), Some(false)), 1000),
            None
        );
        assert_eq!(objection(Some(&c), policy(None, None), u64::MAX), None);
    }

    #[test]
    fn an_item_over_the_limit_would_be_deleted_for_good() {
        let c = drive(DriveKind::Fixed, r"C:\");
        assert_eq!(
            objection(Some(&c), policy(Some(1000), None), 1001),
            Some(PermanentReason::TooLargeForBin {
                item_bytes: 1001,
                limit_bytes: 1000
            })
        );
    }

    #[test]
    fn a_volume_set_to_delete_at_once_has_no_bin_to_use() {
        let c = drive(DriveKind::Fixed, r"C:\");
        assert_eq!(
            objection(Some(&c), policy(Some(u64::MAX), Some(true)), 1),
            Some(PermanentReason::BinDisabled)
        );
    }

    #[test]
    fn only_fixed_drives_have_a_bin() {
        for kind in [DriveKind::Removable, DriveKind::Network, DriveKind::Other] {
            let d = drive(kind, r"E:\");
            assert_eq!(
                objection(Some(&d), policy(None, None), 1),
                Some(PermanentReason::NoTrashOnVolume),
                "{kind:?}"
            );
        }
    }

    #[test]
    fn an_unknown_drive_is_not_assumed_to_have_a_bin() {
        assert_eq!(
            objection(None, policy(None, None), 1),
            Some(PermanentReason::BinUnknown)
        );
    }

    #[test]
    fn extended_prefixes_are_removed_for_the_table() {
        assert_eq!(strip_extended_prefix(r"\\?\C:\Users\a"), r"C:\Users\a");
        assert_eq!(strip_extended_prefix(r"\\?\UNC\h\s\f"), r"\\h\s\f");
        assert_eq!(strip_extended_prefix(r"C:\x"), r"C:\x");
        // A volume GUID path has no drive letter to show.
        assert_eq!(
            strip_extended_prefix(r"\\?\Volume{1234}\x"),
            r"\\?\Volume{1234}\x"
        );
    }

    #[test]
    fn the_longest_drive_root_wins() {
        let drives = [
            drive(DriveKind::Fixed, r"C:\"),
            drive(DriveKind::Removable, r"C:\mnt\usb\"),
            drive(DriveKind::Fixed, r"D:\"),
        ];
        let find = |p: &str| drives_for(&drives, p);
        assert_eq!(find(r"C:\Users\a"), Some(r"C:\"));
        assert_eq!(find(r"c:\USERS"), Some(r"C:\"));
        assert_eq!(find(r"C:\mnt\usb\file"), Some(r"C:\mnt\usb\"));
        assert_eq!(find(r"C:\mnt\usbstick\file"), Some(r"C:\"));
        assert_eq!(find(r"\\?\D:\x"), Some(r"D:\"));
        assert_eq!(find(r"E:\x"), None);
        assert_eq!(find(r"C:"), Some(r"C:\"));
    }

    fn drives_for<'a>(drives: &'a [Drive], path: &str) -> Option<&'a str> {
        drive_for(drives, Path::new(path)).map(|d| d.mount_point.to_str().unwrap())
    }

    fn u(text: &str) -> Vec<u16> {
        text.encode_utf16().collect()
    }

    fn text(units: Option<Vec<u16>>) -> Option<String> {
        units.map(|u| String::from_utf16(&u).unwrap())
    }

    #[test]
    fn the_shell_gets_an_ordinary_path() {
        let shell = |path: &str| text(shell_path(&u(path)));
        assert_eq!(
            shell(r"\\?\C:\Users\a\f.txt").as_deref(),
            Some(r"C:\Users\a\f.txt")
        );
        assert_eq!(
            shell(r"\\?\UNC\host\share\f.txt").as_deref(),
            Some(r"\\host\share\f.txt")
        );
        assert_eq!(shell(r"C:\x\y").as_deref(), Some(r"C:\x\y"));
        assert_eq!(shell("C:/x/y").as_deref(), Some(r"C:\x\y"));
        // Nothing ordinary can name these.
        for path in [
            r"\\?\Volume{1234}\x",
            r"\\.\C:\x",
            r"\\?\GLOBALROOT\Device\x",
            "",
            "C:\\a\0b",
        ] {
            assert_eq!(shell(path), None, "{path}");
        }
    }

    #[test]
    fn the_shell_is_asked_for_the_item_that_was_chosen_and_not_for_a_look_alike() {
        // `a<lone surrogate>` and `a<U+FFFD>` are two different files that can
        // sit side by side. As text the first becomes the second; as units it
        // stays itself.
        let lone = [0xD800u16];
        let mut chosen = u(r"\\?\C:\Users\me\a");
        chosen.extend(lone);
        let given = shell_path(&chosen).unwrap();
        let mut want = u(r"C:\Users\me\a");
        want.extend(lone);
        assert_eq!(given, want);
        let mut look_alike = u(r"C:\Users\me\a");
        look_alike.extend(u("\u{fffd}"));
        assert_ne!(given, look_alike);
        // A lone surrogate in the middle, or the low half alone, is kept too.
        for unit in [0xD800u16, 0xDBFF, 0xDC00, 0xDFFF] {
            let mut path = u(r"C:\x\");
            path.push(unit);
            path.extend(u(".txt"));
            assert_eq!(shell_path(&path).unwrap(), path, "{unit:#x}");
        }
        // A path is not "ordinary" because of a NUL or a prefix hidden behind
        // a surrogate; those are judged on the units too.
        let mut with_nul = u(r"C:\x");
        with_nul.extend([0xD800, 0]);
        assert_eq!(shell_path(&with_nul), None);
    }

    #[test]
    fn inside_means_the_root_or_below_it() {
        let inside = |root: &str, path: &str| is_inside(&u(root), &u(path));
        assert!(inside(r"\\?\C:\Temp\fx", r"C:\temp\FX\a\b"));
        assert!(inside(r"C:\Temp\fx\", r"c:/temp/fx"));
        assert!(!inside(r"C:\Temp\fx", r"C:\Temp\fx2"));
        assert!(!inside(r"C:\Temp\fx", r"C:\Temp"));
        assert!(!inside(r"C:\Temp\fx", r"D:\Temp\fx\a"));
    }

    #[test]
    fn inside_and_the_drive_are_judged_by_exact_units() {
        // A lone surrogate is not the replacement character, whether it is in
        // the root or below it.
        let with = |prefix: &str, unit: u16, suffix: &str| {
            let mut out = u(prefix);
            out.push(unit);
            out.extend(u(suffix));
            out
        };
        let root_lone = with(r"C:\Temp\fx", 0xD800, "");
        let root_fffd = u("C:\\Temp\\fx\u{fffd}");
        assert!(is_inside(&root_lone, &with(r"C:\Temp\fx", 0xD800, r"\a")));
        assert!(is_inside(&root_lone, &root_lone));
        assert!(!is_inside(&root_lone, &root_fffd));
        assert!(!is_inside(&root_fffd, &root_lone));
        assert!(!is_inside(&root_lone, &with(r"C:\Temp\fx", 0xD801, r"\a")));
        // The same for the root of a drive: a folder whose name ends in a lone
        // surrogate is not inside the drive mounted on the same name ending in
        // U+FFFD, so it is on the drive that really holds it.
        let mut lookalike = drive(DriveKind::Removable, r"C:\mnt\x");
        lookalike.mount_point = PathBuf::from("C:\\mnt\\x\u{fffd}\\");
        let drives = [drive(DriveKind::Fixed, r"C:\"), lookalike];
        let under_lone = with(r"C:\mnt\x", 0xD800, r"\file");
        let found = drive_for_units(&drives, &under_lone).unwrap();
        assert_eq!(found.kind, DriveKind::Fixed);
        let under_fffd = u("C:\\mnt\\x\u{fffd}\\file");
        assert_eq!(
            drive_for_units(&drives, &under_fffd).unwrap().kind,
            DriveKind::Removable
        );
        assert!(drive_for_units(&drives, &[]).is_none());
    }

    #[test]
    fn a_recorded_path_is_the_same_place_only_by_its_exact_units() {
        let units = |text: &str| -> Vec<u16> { text.encode_utf16().collect() };
        assert!(same_place(
            r"C:\Users\Me\a.txt",
            &units(r"\\?\c:\users\me\A.TXT")
        ));
        assert!(!same_place(
            r"C:\Users\Me\a.txt",
            &units(r"C:\Users\Me\b.txt")
        ));
        // A record is text; a path with a lone surrogate is not the file whose
        // name has the replacement character in that place.
        let mut lone = units(r"C:\Users\Me\a");
        lone.push(0xD800);
        assert!(!same_place("C:\\Users\\Me\\a\u{FFFD}", &lone));
        assert!(same_place(
            "C:\\Users\\Me\\a\u{FFFD}",
            &units("C:\\Users\\Me\\a\u{FFFD}")
        ));
    }

    #[test]
    fn paths_compare_without_case_or_prefix() {
        assert!(same_path(r"C:\Users\A", r"\\?\c:\users\a"));
        assert!(!same_path(r"C:\Users\A", r"C:\Users\B"));
    }

    fn units(text: &str) -> Vec<u8> {
        text.encode_utf16().flat_map(u16::to_le_bytes).collect()
    }

    #[test]
    fn version_two_records_are_read() {
        let path = r"C:\Users\me\file.txt";
        let mut bytes = Vec::new();
        bytes.extend(2u64.to_le_bytes());
        bytes.extend(1234u64.to_le_bytes());
        bytes.extend(55u64.to_le_bytes());
        bytes.extend((path.encode_utf16().count() as u32 + 1).to_le_bytes());
        bytes.extend(units(path));
        bytes.extend([0, 0]);
        assert_eq!(
            parse_record(&bytes),
            Some(RecycleRecord {
                size: 1234,
                deleted_at: 55,
                original: path.into()
            })
        );
    }

    #[test]
    fn version_one_records_are_read() {
        let path = r"C:\old\file.txt";
        let mut bytes = Vec::new();
        bytes.extend(1u64.to_le_bytes());
        bytes.extend(77u64.to_le_bytes());
        bytes.extend(66u64.to_le_bytes());
        let mut field = units(path);
        field.resize(520, 0);
        bytes.extend(field);
        assert_eq!(
            parse_record(&bytes),
            Some(RecycleRecord {
                size: 77,
                deleted_at: 66,
                original: path.into()
            })
        );
    }

    #[test]
    fn damaged_records_are_not_believed() {
        assert_eq!(parse_record(&[]), None);
        assert_eq!(parse_record(&[0; 23]), None);
        // Unknown version.
        let mut unknown = vec![0u8; 40];
        unknown[0] = 9;
        assert_eq!(parse_record(&unknown), None);
        // A length that runs past the end.
        let mut long = Vec::new();
        long.extend(2u64.to_le_bytes());
        long.extend(0u64.to_le_bytes());
        long.extend(0u64.to_le_bytes());
        long.extend(1000u32.to_le_bytes());
        long.extend(units("C:\\a"));
        assert_eq!(parse_record(&long), None);
        // A zero length, and an absurd one.
        for chars in [0u32, u32::MAX] {
            let mut bad = Vec::new();
            bad.extend(2u64.to_le_bytes());
            bad.extend(0u64.to_le_bytes());
            bad.extend(0u64.to_le_bytes());
            bad.extend(chars.to_le_bytes());
            bad.extend(units("C:\\a"));
            assert_eq!(parse_record(&bad), None, "{chars}");
        }
        // Unpaired surrogate.
        let mut surrogate = Vec::new();
        surrogate.extend(2u64.to_le_bytes());
        surrogate.extend(0u64.to_le_bytes());
        surrogate.extend(0u64.to_le_bytes());
        surrogate.extend(2u32.to_le_bytes());
        surrogate.extend([0x00, 0xd8, 0, 0]);
        assert_eq!(parse_record(&surrogate), None);
    }

    #[test]
    fn the_info_file_of_a_recycled_item_is_found_by_name() {
        let u = |text: &str| -> Vec<u16> { text.encode_utf16().collect() };
        assert_eq!(
            info_for(&u(r"C:\$Recycle.Bin\S-1-5-21\$R1A2B3C.txt")),
            Some(u(r"C:\$Recycle.Bin\S-1-5-21\$I1A2B3C.txt"))
        );
        for bad in [r"C:\x\$I1A2B3C.txt", r"C:\x\file", r"C:\x\$R", r"C:\x\R12"] {
            assert_eq!(info_for(&u(bad)), None, "{bad}");
        }
    }

    #[test]
    fn the_info_file_keeps_an_extension_that_is_not_text() {
        // The bin names the item after the original's extension, which can
        // end in a lone surrogate; the record is looked up under that name.
        let mut recycled: Vec<u16> = r"C:\$Recycle.Bin\S-1-5-21\$R1A2B3C.t"
            .encode_utf16()
            .collect();
        recycled.push(0xD800);
        let mut want: Vec<u16> = r"C:\$Recycle.Bin\S-1-5-21\$I1A2B3C.t"
            .encode_utf16()
            .collect();
        want.push(0xD800);
        assert_eq!(info_for(&recycled), Some(want));
    }
}
