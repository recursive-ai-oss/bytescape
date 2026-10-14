//! The Windows Recycle Bin rules that need no Windows call, so they are tested
//! everywhere.

use super::BinPolicy;
use std::path::{Path, PathBuf};

/// The `{guid}` part of a drive identifier such as
/// `volume:{4d1c2e00-0000-0000-0000-100000000000}`. `None` for any other form
/// (a network share has no volume GUID).
pub(crate) fn volume_guid(drive_id: &str) -> Option<&str> {
    let guid = drive_id.strip_prefix("volume:")?;
    let inner = guid.strip_prefix('{')?.strip_suffix('}')?;
    let valid = !inner.is_empty() && inner.chars().all(|c| c.is_ascii_hexdigit() || c == '-');
    valid.then_some(guid)
}

/// Where the user's setting for one volume's bin is stored, under
/// `HKEY_CURRENT_USER`.
pub(crate) fn bit_bucket_key(guid: &str) -> String {
    format!(r"Software\Microsoft\Windows\CurrentVersion\Explorer\BitBucket\Volume\{guid}")
}

/// The stored maximum size is in megabytes.
pub(crate) fn bytes_from_megabytes(megabytes: u32) -> u64 {
    u64::from(megabytes) * 1024 * 1024
}

/// Turns the two stored values into a policy. Values that are not stored stay
/// unknown rather than being guessed.
pub(crate) fn policy_from(max_megabytes: Option<u32>, nuke_on_delete: Option<u32>) -> BinPolicy {
    BinPolicy {
        max_bytes: max_megabytes.map(bytes_from_megabytes),
        delete_permanently: nuke_on_delete.map(|value| value != 0),
    }
}

/// Whether text is a security identifier in its string form, such as
/// `S-1-5-21-1004336348-1177238915-682003330-512`. Only letters, digits and
/// hyphens are allowed, so the text can never add a path separator.
pub(crate) fn is_sid_text(text: &str) -> bool {
    text.starts_with("S-1-")
        && text.len() <= 184
        && text.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// `X:\$Recycle.Bin\<SID>` for a drive root. Only the given user's folder is
/// ever named; the folders of other users are not.
pub(crate) fn recycle_folder(root: &Path, sid: &str) -> Option<PathBuf> {
    is_sid_text(sid).then(|| root.join("$Recycle.Bin").join(sid))
}

/// The bytes and item count of a query answer. A negative number is not a
/// size.
pub(crate) fn query_figures(size: i64, items: i64) -> (u64, u64) {
    (
        u64::try_from(size).unwrap_or(0),
        u64::try_from(items).unwrap_or(0),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guids_come_out_of_drive_ids() {
        assert_eq!(
            volume_guid("volume:{4d1c2e00-0000-0000-0000-100000000000}"),
            Some("{4d1c2e00-0000-0000-0000-100000000000}")
        );
        for bad in [
            "",
            "volume:",
            "volume:{}",
            "volume:{zz}",
            "volume:4d1c",
            "path:C:\\",
            r"unc:\\server\share\",
            "volume:{abc",
            r"volume:{ab\cd}",
        ] {
            assert_eq!(volume_guid(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn the_registry_key_is_under_the_bit_bucket() {
        assert_eq!(
            bit_bucket_key("{abc-1}"),
            r"Software\Microsoft\Windows\CurrentVersion\Explorer\BitBucket\Volume\{abc-1}"
        );
    }

    #[test]
    fn stored_values_become_a_policy() {
        assert_eq!(policy_from(None, None), BinPolicy::default());
        let policy = policy_from(Some(2048), Some(0));
        assert_eq!(policy.max_bytes, Some(2048 * 1024 * 1024));
        assert_eq!(policy.delete_permanently, Some(false));
        assert_eq!(policy_from(None, Some(1)).delete_permanently, Some(true));
        assert_eq!(policy_from(None, Some(7)).delete_permanently, Some(true));
        // The largest stored value does not overflow.
        assert_eq!(
            policy_from(Some(u32::MAX), None).max_bytes,
            Some(u64::from(u32::MAX) << 20)
        );
    }

    #[test]
    fn only_well_formed_sids_name_a_folder() {
        let sid = "S-1-5-21-1004336348-1177238915-682003330-1001";
        assert!(is_sid_text(sid));
        assert_eq!(
            recycle_folder(Path::new(r"C:\"), sid),
            Some(Path::new(r"C:\").join("$Recycle.Bin").join(sid))
        );
        for bad in [
            "",
            "S-1-5-21\\..\\x",
            "S-1-5-21/x",
            "S-1-5-21 x",
            "S-1-5-21:x",
            "s-1-5-21",
            "S-2-5-21",
            "..",
            &format!("S-1-{}", "5".repeat(300)),
        ] {
            assert!(!is_sid_text(bad), "{bad:?}");
            assert_eq!(recycle_folder(Path::new(r"C:\"), bad), None, "{bad:?}");
        }
    }

    #[test]
    fn negative_figures_are_not_sizes() {
        assert_eq!(query_figures(1234, 5), (1234, 5));
        assert_eq!(query_figures(-1, -1), (0, 0));
        assert_eq!(query_figures(0, 0), (0, 0));
        assert_eq!(
            query_figures(i64::MAX, i64::MAX),
            (i64::MAX as u64, i64::MAX as u64)
        );
    }
}
