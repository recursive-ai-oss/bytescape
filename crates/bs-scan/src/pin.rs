//! Holding a scan to the root that was judged: [`RootPin`], the keys it
//! compares, and the one function that computes them for both sides.

use std::fmt;

/// What identifies the root a scan is held to, as the system reports it.
///
/// Two keys are equal only when they are the same kind of key and every part
/// is equal; a scanner that cannot produce a key for the root it opened
/// refuses a pinned scan.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RootKey {
    /// Linux: the mount the root is on. `unique` says which kind of ID it is:
    /// the never-reused one (Linux 6.8 and later) or the older one, which is
    /// only safe while the judged mount stays referenced.
    Mount {
        /// The mount ID.
        id: u64,
        /// Whether `id` is the unique kind.
        unique: bool,
    },
    /// Windows: the volume serial number and the 128-bit file ID of the root
    /// folder itself.
    Volume {
        /// The volume serial number.
        serial: u64,
        /// The 128-bit file ID.
        file_id: u128,
    },
    /// The device number of the block device a file system lives on.
    Device {
        /// Major number.
        major: u32,
        /// Minor number.
        minor: u32,
    },
}

impl fmt::Display for RootKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RootKey::Mount { id, unique: true } => write!(f, "mount {id} (unique)"),
            RootKey::Mount { id, unique: false } => write!(f, "mount {id}"),
            RootKey::Volume { serial, file_id } => {
                write!(f, "volume {serial:016x} folder {file_id:032x}")
            }
            RootKey::Device { major, minor } => write!(f, "device {major}:{minor}"),
        }
    }
}

#[cfg(target_os = "linux")]
type Held = std::sync::Arc<std::os::fd::OwnedFd>;
#[cfg(windows)]
type Held = std::sync::Arc<std::os::windows::io::OwnedHandle>;
#[cfg(not(any(target_os = "linux", windows)))]
type Held = ();

/// The root a caller judged, held open.
///
/// A caller that decides something about a folder (that it is on a local
/// disk, say) and then hands a scanner the folder's *name* leaves a gap: by
/// the time the scanner opens the name it can lead somewhere else. A pin
/// closes it. The caller opens the root once, judges that very descriptor,
/// and gives the descriptor to the pin; each scanner then checks the root it
/// opened itself against the pin, on its own descriptor, before it reads or
/// sends anything. One function computes the comparison value for both sides:
/// the pin's, from the held descriptor, and the scanner's, from the one it
/// opened.
///
/// # What the pin holds, and why
///
/// The pin keeps the judged descriptor open for as long as the pin (or any
/// clone of it, or any [`ScanOptions`](crate::ScanOptions) made with it)
/// lives. That is not tidiness. On Linux the mount ID that identifies a
/// mount is a small number the kernel hands out again as soon as the mount is
/// gone: a lazily unmounted root followed by a different mount at the same
/// place can carry the very same ID. An open descriptor on the judged mount
/// keeps that mount referenced, so its ID cannot be freed or reused while the
/// pin exists. Where the kernel also reports the unique mount ID (Linux 6.8
/// and later), that is what is compared, and it is never reused at all. The
/// Windows pin compares the volume serial number and the 128-bit file ID of
/// the root folder itself, so another folder of the same volume is refused too;
/// an NTFS file ID carries a sequence number that changes when a record is
/// reused, and the held handle keeps the judged folder open meanwhile.
///
/// # What the caller must do
///
/// * Build the pin from the descriptor (Linux) or handle (Windows) that was
///   judged, not from a second open of the name.
/// * Keep the pin, or the [`ScanOptions`](crate::ScanOptions) that hold it,
///   alive until the scan has ended: scanners check their root more than once
///   (the XFS reader opens it twice), and dropping the pin earlier removes the
///   guarantee on kernels that only have the reusable ID.
/// * On Windows, open the judged handle with at least `FILE_READ_ATTRIBUTES`
///   and with backup semantics, so that it can be asked for its file ID.
///
/// Give it to a scan with [`ScanOptions::pin_root`](crate::ScanOptions::pin_root).
/// A pin is cheap to clone; clones share the one descriptor.
#[derive(Clone, Debug)]
pub struct RootPin {
    key: RootKey,
    /// The device the judged mount's file system is on, as the system reports
    /// it for the held descriptor. Linux only.
    device: Option<(u32, u32)>,
    /// Kept only to keep the judged mount (and its ID) referenced.
    #[allow(dead_code)]
    held: Held,
}

impl RootPin {
    /// The key of the judged root.
    #[must_use]
    pub fn key(&self) -> RootKey {
        self.key
    }

    /// The device number (major, minor) of the file system the judged root is
    /// on, where the system reports one. A reader that opens a block device
    /// instead of a folder (ext4) holds the device to this.
    #[must_use]
    pub fn device(&self) -> Option<(u32, u32)> {
        self.device
    }

    /// The pin for a root judged on `fd`.
    ///
    /// The pin takes a share of `fd` and keeps it open. Fails if the system
    /// gives no mount ID for it (before Linux 5.8), because nothing could then
    /// be checked: an elevated caller refuses the scan.
    #[cfg(target_os = "linux")]
    pub fn from_fd(fd: std::sync::Arc<std::os::fd::OwnedFd>) -> std::io::Result<RootPin> {
        use std::os::fd::AsFd;
        let identity = crate::linux::sys::root_identity(fd.as_fd())?;
        let key = identity.key.ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "the system reports no mount ID, so a scan root cannot be pinned",
            )
        })?;
        Ok(RootPin {
            key,
            device: Some((identity.major, identity.minor)),
            held: fd,
        })
    }

    /// The pin for a root judged on `handle`.
    ///
    /// The pin takes a share of `handle` and keeps it open. Fails if the
    /// volume gives no 128-bit file ID for it, because nothing could then be
    /// checked.
    #[cfg(windows)]
    pub fn from_handle(
        handle: std::sync::Arc<std::os::windows::io::OwnedHandle>,
    ) -> std::io::Result<RootPin> {
        let key = crate::windows::sys::root_key(&handle)?;
        Ok(RootPin {
            key,
            device: None,
            held: handle,
        })
    }
}

/// Compares the key of a root a scanner opened with the pin, if there is one.
///
/// `found` is what the system reported for the descriptor the scanner holds as
/// its root, from the same function that built the pin's key. Without a pin
/// this always passes. With one, a different key, or no key at all, is
/// [`ScanError::RootMoved`](crate::ScanError::RootMoved): a check that cannot
/// be made is a refusal, never a pass.
pub(crate) fn compare(
    pin: Option<&RootPin>,
    found: Option<RootKey>,
) -> Result<(), crate::ScanError> {
    match pin {
        None => Ok(()),
        Some(pin) if found == Some(pin.key) => Ok(()),
        Some(pin) => Err(crate::ScanError::RootMoved {
            pinned: pin.key,
            found,
        }),
    }
}

/// Checks the root a scanner opened (`fd`) against the pin. Call it on the
/// descriptor the scan will use, before reading or sending anything.
///
/// # Errors
///
/// [`ScanError::RootMoved`](crate::ScanError::RootMoved) when the root is not
/// on the pinned mount or cannot be told; `ScanError::Root` when the system
/// refuses to describe the descriptor.
#[cfg(target_os = "linux")]
pub fn check_root_pin(
    pin: Option<&RootPin>,
    fd: std::os::fd::BorrowedFd<'_>,
) -> Result<(), crate::ScanError> {
    if pin.is_none() {
        return Ok(());
    }
    let identity = crate::linux::sys::root_identity(fd).map_err(crate::ScanError::Root)?;
    compare(pin, identity.key)
}

/// Checks the root a scanner opened (`handle`) against the pin. Call it on the
/// handle the scan will use, before reading or sending anything.
///
/// # Errors
///
/// [`ScanError::RootMoved`](crate::ScanError::RootMoved) when the root is not
/// the pinned folder or cannot be told.
#[cfg(windows)]
pub fn check_root_pin(
    pin: Option<&RootPin>,
    handle: std::os::windows::io::BorrowedHandle<'_>,
) -> Result<(), crate::ScanError> {
    if pin.is_none() {
        return Ok(());
    }
    // A volume that gives no file ID is a root that cannot be told.
    let key = crate::windows::sys::root_key_of_borrowed(handle).ok();
    compare(pin, key)
}

/// Checks that the device a reader opened is the one the judged mount's file
/// system is on. `is_block` and `rdev` are what the system says about the
/// opened device; `rdev` is a raw `st_rdev`.
///
/// # Errors
///
/// [`ScanError::RootMoved`](crate::ScanError::RootMoved) when it is not a block
/// device, or its number differs from the pin's, or the pin has no device.
pub fn check_device_pin(
    pin: Option<&RootPin>,
    is_block: bool,
    rdev: u64,
) -> Result<(), crate::ScanError> {
    let Some(pin) = pin else {
        return Ok(());
    };
    let (major, minor) = split_device(rdev);
    let found = is_block.then_some(RootKey::Device { major, minor });
    let wanted = pin
        .device
        .map(|(major, minor)| RootKey::Device { major, minor });
    match (wanted, found) {
        (Some(wanted), Some(found)) if wanted == found => Ok(()),
        (wanted, found) => Err(crate::ScanError::RootMoved {
            pinned: wanted.unwrap_or(pin.key),
            found,
        }),
    }
}

/// The major and minor numbers of a raw Linux device number, in the encoding
/// the kernel gives `st_rdev` and `st_dev`.
#[must_use]
pub fn split_device(dev: u64) -> (u32, u32) {
    let major = ((dev >> 8) & 0xfff) | ((dev >> 32) & !0xfff);
    let minor = (dev & 0xff) | ((dev >> 12) & !0xff);
    (
        u32::try_from(major & 0xffff_ffff).unwrap_or(u32::MAX),
        u32::try_from(minor & 0xffff_ffff).unwrap_or(u32::MAX),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn device_numbers_split_as_the_kernel_encodes_them() {
        for (major, minor) in [(8, 1), (259, 3), (0xfff, 0xff), (0x1234, 0x12345), (0, 0)] {
            assert_eq!(
                split_device(libc::makedev(major, minor)),
                (major, minor),
                "{major}:{minor}"
            );
        }
    }

    #[test]
    fn keys_have_words() {
        assert_eq!(
            RootKey::Mount {
                id: 5,
                unique: false
            }
            .to_string(),
            "mount 5"
        );
        assert!(
            RootKey::Mount {
                id: 5,
                unique: true
            }
            .to_string()
            .contains("unique")
        );
        assert!(
            RootKey::Volume {
                serial: 1,
                file_id: 2
            }
            .to_string()
            .contains("volume")
        );
        assert_eq!(
            RootKey::Device { major: 8, minor: 1 }.to_string(),
            "device 8:1"
        );
    }
}
