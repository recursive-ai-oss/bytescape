//! Windows drive rules that need no Windows call, so they are tested everywhere.

use super::DriveKind;

pub(crate) const DRIVE_NO_ROOT_DIR: u32 = 1;
pub(crate) const DRIVE_REMOVABLE: u32 = 2;
pub(crate) const DRIVE_FIXED: u32 = 3;
pub(crate) const DRIVE_REMOTE: u32 = 4;
pub(crate) const DRIVE_CDROM: u32 = 5;

/// Maps what `GetDriveTypeW` and the storage bus say to a kind of drive.
///
/// `usb_bus` and `removable_media` come from the storage device descriptor.
/// Windows reports many external USB disks as fixed drives, so a fixed drive
/// on the USB bus, or flagged as removable media, is removable here.
///
/// `None` means there is nothing to list: the letter has no volume.
pub(crate) fn kind_of(drive_type: u32, usb_bus: bool, removable_media: bool) -> Option<DriveKind> {
    match drive_type {
        DRIVE_NO_ROOT_DIR => None,
        DRIVE_REMOVABLE | DRIVE_CDROM => Some(DriveKind::Removable),
        DRIVE_FIXED if usb_bus || removable_media => Some(DriveKind::Removable),
        DRIVE_FIXED => Some(DriveKind::Fixed),
        DRIVE_REMOTE => Some(DriveKind::Network),
        // Unknown types and RAM disks.
        _ => Some(DriveKind::Other),
    }
}

/// The drive letters set in a `GetLogicalDrives` bit mask, in order.
pub(crate) fn letters(mask: u32) -> Vec<char> {
    (0u8..26)
        .filter(|bit| mask & (1u32 << bit) != 0)
        .map(|bit| char::from(b'A' + bit))
        .collect()
}

/// The stable part of a volume name such as
/// `\\?\Volume{4d1c...}\`, as a drive identifier. `None` when the text is not
/// a volume GUID path.
pub(crate) fn volume_id(volume_name: &str) -> Option<String> {
    let rest = volume_name.strip_prefix(r"\\?\Volume{")?;
    let guid = rest
        .strip_suffix(r"}\")
        .or_else(|| rest.strip_suffix('}'))?;
    let valid = !guid.is_empty() && guid.chars().all(|c| c.is_ascii_hexdigit() || c == '-');
    valid.then(|| format!("volume:{{{}}}", guid.to_ascii_lowercase()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_table() {
        #[rustfmt::skip]
        let cases: [(u32, bool, bool, Option<DriveKind>); 13] = [
            (0, false, false, Some(DriveKind::Other)),
            (DRIVE_NO_ROOT_DIR, false, false, None),
            (DRIVE_REMOVABLE, false, false, Some(DriveKind::Removable)),
            (DRIVE_FIXED, false, false, Some(DriveKind::Fixed)),
            (DRIVE_FIXED, true, false, Some(DriveKind::Removable)),
            (DRIVE_FIXED, false, true, Some(DriveKind::Removable)),
            (DRIVE_FIXED, true, true, Some(DriveKind::Removable)),
            (DRIVE_REMOTE, false, false, Some(DriveKind::Network)),
            // The bus flags do not turn a network drive into a local one.
            (DRIVE_REMOTE, true, true, Some(DriveKind::Network)),
            (DRIVE_CDROM, false, false, Some(DriveKind::Removable)),
            (6, false, false, Some(DriveKind::Other)),
            (7, false, false, Some(DriveKind::Other)),
            (u32::MAX, false, false, Some(DriveKind::Other)),
        ];
        for (drive_type, usb, removable, expected) in cases {
            assert_eq!(
                kind_of(drive_type, usb, removable),
                expected,
                "{drive_type} {usb} {removable}"
            );
        }
    }

    #[test]
    fn letters_from_mask() {
        assert_eq!(letters(0), Vec::<char>::new());
        assert_eq!(letters(0b101), vec!['A', 'C']);
        assert_eq!(letters(1 << 25), vec!['Z']);
        // Bits above Z are not drives.
        assert_eq!(letters(u32::MAX).len(), 26);
        assert_eq!(letters(0xFFFF_FFFF << 26), Vec::<char>::new());
    }

    #[test]
    fn volume_ids() {
        assert_eq!(
            volume_id(r"\\?\Volume{4D1C2E00-0000-0000-0000-100000000000}\").as_deref(),
            Some("volume:{4d1c2e00-0000-0000-0000-100000000000}")
        );
        assert_eq!(
            volume_id(r"\\?\Volume{abcd-ef01}").as_deref(),
            Some("volume:{abcd-ef01}")
        );
        for bad in [
            "",
            r"C:\",
            r"\\?\Volume{}\",
            r"\\?\Volume{zz}\",
            r"\\?\Volume{abcd",
            r"\\server\share\",
        ] {
            assert_eq!(volume_id(bad), None, "{bad}");
        }
    }
}
