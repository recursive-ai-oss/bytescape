//! The bulk inode ioctl, as a small function over byte buffers. The only
//! place in the crate with `unsafe`. The reader never writes to the file
//! system: `XFS_IOC_BULKSTAT` only reads. It needs administrator rights,
//! which is why the reader runs in the helper.
#![allow(unsafe_code)]
#![allow(
    clippy::cast_possible_truncation,
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing
)]

use crate::bulkstat::{
    BulkStat, HEADER_LEN, RECORD_LEN, REQ_OCOUNT, ReplyEnd, read_reply, request_header,
};
use crate::le::{u32_at, u64_at};
use crate::{Inodes, XfsError};
use std::fs::File;
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

/// `_IOR('X', 127, struct xfs_bulkstat_req)`: read, size 64, type `X`.
const IOC_BULKSTAT: libc::c_ulong =
    (2 << 30) | ((HEADER_LEN as libc::c_ulong) << 16) | ((b'X' as libc::c_ulong) << 8) | 127;

/// Records asked for per call.
const PER_CALL: u32 = 2048;

/// `XFS_SB_MAGIC`.
const XFS_MAGIC: i64 = 0x5846_5342;

/// An XFS file system, read through a directory on it.
#[derive(Debug)]
pub struct IoctlInodes {
    fd: OwnedFd,
}

impl IoctlInodes {
    /// Opens the directory `path` and checks that it is on XFS.
    pub fn open(path: &Path) -> Result<IoctlInodes, XfsError> {
        IoctlInodes::open_pinned(path, None)
    }

    /// Like [`IoctlInodes::open`], and with a pinned root to hold the
    /// directory to: the descriptor that was opened must be on the pin's mount
    /// (compared with the same function that made the pin's key), or the open
    /// fails with [`XfsError::RootMoved`] before anything else is asked of the
    /// file system. The directory is opened without following a final link.
    /// The caller keeps the pin alive until the scan has ended.
    pub fn open_pinned(
        path: &Path,
        pin: Option<&bs_scan::RootPin>,
    ) -> Result<IoctlInodes, XfsError> {
        let file = File::options()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)?;
        let fd: OwnedFd = file.into();
        match bs_scan::check_root_pin(pin, fd.as_fd()) {
            Ok(()) => {}
            Err(bs_scan::ScanError::RootMoved { pinned, found }) => {
                return Err(XfsError::RootMoved { pinned, found });
            }
            Err(bs_scan::ScanError::Root(e)) => return Err(XfsError::Io(e)),
            Err(other) => return Err(XfsError::Io(io::Error::other(other))),
        }
        let mut st = MaybeUninit::<libc::statfs>::zeroed();
        // SAFETY: `fd` is an open descriptor and `st` is a writable buffer of
        // the size `fstatfs` fills; it was zeroed, so it is initialized
        // whether or not the call fills it.
        let rc = unsafe { libc::fstatfs(fd.as_raw_fd(), st.as_mut_ptr()) };
        if rc != 0 {
            return Err(io::Error::last_os_error().into());
        }
        // SAFETY: zero-initialized above, and a plain-data struct.
        let st = unsafe { st.assume_init() };
        // The type of `f_type` differs between targets.
        #[allow(clippy::unnecessary_cast)]
        let magic = st.f_type as i64;
        if magic != XFS_MAGIC {
            return Err(XfsError::NotXfs);
        }
        Ok(IoctlInodes { fd })
    }
}

impl Inodes for IoctlInodes {
    fn each(&self, each: &mut dyn FnMut(BulkStat) -> bool) -> Result<(), XfsError> {
        let mut buf = vec![0u8; HEADER_LEN + PER_CALL as usize * RECORD_LEN];
        let mut after = 0u64;
        loop {
            buf[..HEADER_LEN].copy_from_slice(&request_header(after, PER_CALL));
            // SAFETY: `buf` is writable and holds the 64-byte request header
            // the ioctl number encodes followed by room for `icount` records,
            // which the header promises; it stays alive and unmoved for the
            // call.
            let rc = unsafe {
                libc::ioctl(
                    self.fd.as_raw_fd(),
                    IOC_BULKSTAT as _,
                    buf.as_mut_ptr().cast::<libc::c_void>(),
                )
            };
            if rc != 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e.into());
            }
            let ocount = u32_at(&buf, REQ_OCOUNT).ok_or(XfsError::Hostile("record count"))?;
            let records = buf
                .get(HEADER_LEN..)
                .ok_or(XfsError::Hostile("reply buffer"))?;
            let mut keep_going = true;
            let end = read_reply(records, ocount, after, &mut |rec| {
                if keep_going {
                    keep_going = each(rec);
                }
            })?;
            match end {
                ReplyEnd::Empty => return Ok(()),
                ReplyEnd::Last(ino) => {
                    if !keep_going {
                        return Ok(());
                    }
                    // The kernel leaves where to go on in the header. Trust it
                    // no further than "after the last inode it gave", which
                    // is what makes every request move forward.
                    let beyond = ino
                        .checked_add(1)
                        .ok_or(XfsError::Hostile("inode number"))?;
                    after = u64_at(&buf, 0).unwrap_or(0).max(beyond);
                }
            }
        }
    }
}
