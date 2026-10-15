//! Where the names come from: a directory-only walk, with the primitives the
//! directory walker is made of. The trait lets the reader's logic run over
//! trees held in memory in the tests; the real implementation opens each
//! directory relative to its parent without following links, reads it with
//! `getdents64`, and asks nothing of the files in it.

use bs_scan::Placement;
use std::io;

/// One name in a directory, as the listing gives it.
#[derive(Clone, Copy, Debug)]
pub struct RawName<'a> {
    /// The name's bytes.
    pub name: &'a [u8],
    /// The inode number the listing reports.
    pub ino: u64,
    /// The `DT_*` type the listing reports.
    pub d_type: u8,
    /// Whether the name is the whole of what the listing holds for it. A
    /// `false` says the name contained a NUL, so what is held is a prefix.
    pub clean: bool,
}

/// What `statx` says about one entry, for the entries the bulk read missed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EntryStat {
    /// The inode number.
    pub ino: u64,
    /// Size in bytes.
    pub size: u64,
    /// Size on disk in bytes.
    pub allocated: u64,
    /// Modification time, whole seconds.
    pub mtime: i64,
    /// Link count.
    pub nlink: u32,
    /// Type and permission bits.
    pub mode: u32,
}

/// An opened directory and what is known about it.
#[derive(Debug)]
pub struct OpenedDir<D> {
    /// The handle later calls take.
    pub dir: D,
    /// Where the directory lives, for the boundary check.
    pub placement: Placement,
    /// The directory's own numbers.
    pub stat: EntryStat,
    /// The device number of the file system, the volume of hard-link keys.
    pub device: u64,
}

/// Something that can be walked for names.
pub trait Names: Sync {
    /// A handle on a directory.
    type Dir: Send + Sync;

    /// Opens the directory where the scan starts.
    fn root(&self) -> io::Result<OpenedDir<Self::Dir>>;

    /// Calls `each` for every entry of `dir` except `.` and `..`. An error
    /// partway keeps the entries already given.
    fn list(&self, dir: &Self::Dir, each: &mut dyn FnMut(RawName<'_>)) -> io::Result<()>;

    /// Opens the directory `name` inside `parent`. It must fail, and never
    /// follow, when `name` is a link, and fail with `ENOTDIR` when it is not
    /// a directory.
    fn open_child(&self, parent: &Self::Dir, name: &[u8]) -> io::Result<OpenedDir<Self::Dir>>;

    /// What `statx` says about `name` inside `dir`, links not followed.
    fn stat_entry(&self, dir: &Self::Dir, name: &[u8]) -> io::Result<EntryStat>;
}

/// Reads directories of a mounted file system.
#[derive(Debug)]
pub struct FsNames {
    root: std::path::PathBuf,
}

impl FsNames {
    /// A walk of the directory `root`, which must be canonical.
    pub fn new(root: impl Into<std::path::PathBuf>) -> FsNames {
        FsNames { root: root.into() }
    }
}

mod real {
    use super::{EntryStat, FsNames, Names, OpenedDir, RawName};
    use bs_scan::Placement;
    use bs_scan::rawdir::{self, Stat};
    use std::ffi::CString;
    use std::io;
    use std::os::fd::{AsFd, OwnedFd};

    fn entry_stat(st: &Stat) -> EntryStat {
        EntryStat {
            ino: st.ino,
            size: st.size,
            allocated: st.blocks.saturating_mul(512),
            mtime: st.mtime,
            nlink: st.nlink,
            mode: st.mode,
        }
    }

    fn opened(fd: OwnedFd) -> io::Result<OpenedDir<OwnedFd>> {
        let st = rawdir::stat_fd(fd.as_fd())?;
        Ok(OpenedDir {
            placement: Placement {
                mount_id: st.mnt_id,
                device: st.dev,
            },
            stat: entry_stat(&st),
            device: st.dev,
            dir: fd,
        })
    }

    fn cname(name: &[u8]) -> io::Result<CString> {
        CString::new(name).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
    }

    impl Names for FsNames {
        type Dir = OwnedFd;

        fn root(&self) -> io::Result<OpenedDir<OwnedFd>> {
            rawdir::raise_open_file_limit();
            opened(rawdir::open_root(&self.root)?)
        }

        fn list(&self, dir: &OwnedFd, each: &mut dyn FnMut(RawName<'_>)) -> io::Result<()> {
            // The kernel leaves the padding after each name as it found it,
            // so the buffer is zero before every read and a name that holds
            // a NUL shows as dirty padding.
            let mut buf = vec![0u8; 128 * 1024];
            loop {
                let filled = rawdir::getdents(dir.as_fd(), &mut buf)?;
                if filled == 0 {
                    return Ok(());
                }
                let result: io::Result<()> = (|| {
                    for item in rawdir::entries(buf.get(..filled).unwrap_or(&[])) {
                        let entry = item.map_err(|_| {
                            io::Error::new(io::ErrorKind::InvalidData, "damaged directory listing")
                        })?;
                        if entry.is_dot_or_dotdot() {
                            continue;
                        }
                        each(RawName {
                            name: entry.name(),
                            ino: entry.ino,
                            d_type: entry.d_type,
                            clean: entry.padding_is_clean,
                        });
                    }
                    Ok(())
                })();
                if let Some(dirty) = buf.get_mut(..filled) {
                    dirty.fill(0);
                }
                result?;
            }
        }

        fn open_child(&self, parent: &OwnedFd, name: &[u8]) -> io::Result<OpenedDir<OwnedFd>> {
            let c = cname(name)?;
            opened(rawdir::open_dir_at(parent.as_fd(), &c)?)
        }

        fn stat_entry(&self, dir: &OwnedFd, name: &[u8]) -> io::Result<EntryStat> {
            let c = cname(name)?;
            Ok(entry_stat(&rawdir::stat_at(dir.as_fd(), &c)?))
        }
    }
}
