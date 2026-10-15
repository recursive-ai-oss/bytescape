//! An XFS file system held in memory: the inode table the bulk read hands
//! over and the directories the walk lists, with hooks for everything that
//! can go wrong in a live one.
#![allow(dead_code)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::cast_possible_truncation,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use bs_scan::Placement;
use bs_scan_xfs::{BulkStat, EntryStat, Inodes, Names, OpenedDir, RawName, XfsError};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io;
use std::sync::Mutex;

pub const S_IFREG: u32 = 0o100_000;
pub const S_IFDIR: u32 = 0o040_000;
pub const S_IFLNK: u32 = 0o120_000;
pub const S_IFIFO: u32 = 0o010_000;
pub const DT_REG: u8 = 8;
pub const DT_DIR: u8 = 4;
pub const DT_LNK: u8 = 10;
pub const DT_FIFO: u8 = 1;
pub const MTIME: i64 = 1_700_000_000;
pub const BLOCK: u32 = 4096;

#[derive(Clone, Debug)]
pub struct MemInode {
    pub size: u64,
    pub blocks: u64,
    pub nlink: u32,
    pub mode: u32,
    pub mtime: i64,
}

#[derive(Clone, Debug)]
pub struct MemEntry {
    pub name: Vec<u8>,
    pub ino: u64,
    pub d_type: u8,
    pub clean: bool,
}

#[derive(Default)]
pub struct MemXfs {
    pub inodes: BTreeMap<u64, MemInode>,
    pub dirs: HashMap<u64, Vec<MemEntry>>,
    /// Mount points: the covered directory (the inode the parent lists) and
    /// the root of what is mounted on it (the inode opening it lands on, as
    /// in the kernel, where the two differ).
    pub mounts: HashMap<u64, u64>,
    /// Directories swapped since they were listed: the inode the listing
    /// names, and the directory that is there now (on the same mount).
    pub swaps: HashMap<u64, u64>,
    /// Directories that are not on the root's mount: `(mount id, device)`.
    /// Directories made inside such a directory inherit it.
    pub regions: HashMap<u64, (u64, u64)>,
    /// Inodes the bulk read leaves out (made after it).
    pub hidden: HashSet<u64>,
    /// Directories that cannot be opened, with the error number.
    pub open_errors: HashMap<u64, i32>,
    /// Directories whose listing fails after this many entries.
    pub list_errors: HashMap<u64, usize>,
    /// The bulk read fails after this many inodes.
    pub bulk_fails_after: Option<usize>,
    next: u64,
    pub opened: Mutex<Vec<u64>>,
}

pub const ROOT: u64 = 128;

impl MemXfs {
    pub fn new() -> MemXfs {
        let mut fs = MemXfs {
            next: ROOT + 1,
            ..MemXfs::default()
        };
        fs.inodes.insert(
            ROOT,
            MemInode {
                size: 6,
                blocks: 0,
                nlink: 2,
                mode: S_IFDIR | 0o755,
                mtime: MTIME,
            },
        );
        fs.dirs.insert(ROOT, Vec::new());
        fs
    }

    fn new_ino(&mut self) -> u64 {
        let ino = self.next;
        self.next += 1;
        ino
    }

    fn entry(&mut self, parent: u64, name: &[u8], ino: u64, d_type: u8) {
        self.dirs.entry(parent).or_default().push(MemEntry {
            name: name.to_vec(),
            ino,
            d_type,
            clean: true,
        });
    }

    pub fn mkdir(&mut self, parent: u64, name: &str) -> u64 {
        let ino = self.new_ino();
        self.inodes.insert(
            ino,
            MemInode {
                size: 6,
                blocks: 0,
                nlink: 2,
                mode: S_IFDIR | 0o755,
                mtime: MTIME + 1,
            },
        );
        self.dirs.insert(ino, Vec::new());
        if let Some(&region) = self.regions.get(&parent) {
            self.regions.insert(ino, region);
        }
        self.entry(parent, name.as_bytes(), ino, DT_DIR);
        ino
    }

    /// A file of `size` bytes holding `blocks` blocks.
    pub fn file(&mut self, parent: u64, name: &[u8], size: u64, blocks: u64) -> u64 {
        let ino = self.new_ino();
        self.inodes.insert(
            ino,
            MemInode {
                size,
                blocks,
                nlink: 1,
                mode: S_IFREG | 0o644,
                mtime: MTIME + 2,
            },
        );
        self.entry(parent, name, ino, DT_REG);
        ino
    }

    pub fn symlink(&mut self, parent: u64, name: &str, target_len: u64) -> u64 {
        let ino = self.new_ino();
        self.inodes.insert(
            ino,
            MemInode {
                size: target_len,
                blocks: 0,
                nlink: 1,
                mode: S_IFLNK | 0o777,
                mtime: MTIME + 3,
            },
        );
        self.entry(parent, name.as_bytes(), ino, DT_LNK);
        ino
    }

    pub fn fifo(&mut self, parent: u64, name: &str) -> u64 {
        let ino = self.new_ino();
        self.inodes.insert(
            ino,
            MemInode {
                size: 0,
                blocks: 0,
                nlink: 1,
                mode: S_IFIFO | 0o600,
                mtime: MTIME + 4,
            },
        );
        self.entry(parent, name.as_bytes(), ino, DT_FIFO);
        ino
    }

    /// Another name for `ino`.
    pub fn link(&mut self, parent: u64, name: &str, ino: u64) {
        let node = self.inodes.get_mut(&ino).unwrap();
        node.nlink += 1;
        let d_type = if node.mode & 0o170_000 == S_IFREG {
            DT_REG
        } else {
            DT_LNK
        };
        self.entry(parent, name.as_bytes(), ino, d_type);
    }

    /// An entry for the directory `dir` that already has one (a loop).
    pub fn link_dir(&mut self, parent: u64, name: &str, dir: u64) {
        self.entry(parent, name.as_bytes(), dir, DT_DIR);
    }

    /// A mount point with another mount of this same file system on it (say
    /// a bind mount of a directory the scan does not otherwise reach). The
    /// covered directory is returned; what is mounted has its own inode,
    /// which holds `inside-the-mount`.
    pub fn mount_point(&mut self, parent: u64, name: &str, mount_id: u64) -> u64 {
        let covered = self.mkdir(parent, name);
        let root = self.new_ino();
        self.inodes.insert(
            root,
            MemInode {
                size: 6,
                blocks: 1,
                nlink: 2,
                mode: S_IFDIR | 0o755,
                mtime: MTIME + 10,
            },
        );
        self.dirs.insert(root, Vec::new());
        self.regions.insert(root, (mount_id, 1));
        self.mounts.insert(covered, root);
        self.file(root, b"inside-the-mount", 4096, 1);
        covered
    }

    /// A mount point with another file system on it. Its inodes are not in
    /// this file system's table; opening the mount point lands on a root
    /// whose inode number is unrelated to the covered directory's.
    pub fn foreign_mount_point(&mut self, parent: u64, name: &str, mount_id: u64) -> u64 {
        let covered = self.mkdir(parent, name);
        let root = self.new_ino();
        self.inodes.insert(
            root,
            MemInode {
                size: 40,
                blocks: 0,
                nlink: 2,
                mode: S_IFDIR | 0o755,
                mtime: MTIME + 20,
            },
        );
        self.dirs.insert(root, Vec::new());
        self.hidden.insert(root);
        self.regions.insert(root, (mount_id, 2));
        self.mounts.insert(covered, root);
        let inner = self.file(root, b"other-fs-file", 999, 99);
        self.hidden.insert(inner);
        covered
    }

    fn stat(&self, ino: u64) -> Option<EntryStat> {
        let n = self.inodes.get(&ino)?;
        Some(EntryStat {
            ino,
            size: n.size,
            allocated: n.blocks * u64::from(BLOCK),
            mtime: n.mtime,
            nlink: n.nlink,
            mode: n.mode,
        })
    }

    fn opened(&self, ino: u64) -> io::Result<OpenedDir<u64>> {
        let stat = self
            .stat(ino)
            .ok_or_else(|| io::Error::from_raw_os_error(libc_enoent()))?;
        let (mount_id, device) = self.regions.get(&ino).copied().unwrap_or((1, 1));
        Ok(OpenedDir {
            dir: ino,
            placement: Placement {
                mount_id: Some(mount_id),
                device,
            },
            stat,
            device,
        })
    }
}

fn libc_enoent() -> i32 {
    2
}

impl Inodes for MemXfs {
    fn each(&self, each: &mut dyn FnMut(BulkStat) -> bool) -> Result<(), XfsError> {
        let mut n = 0usize;
        for (&ino, node) in &self.inodes {
            if self.hidden.contains(&ino) {
                continue;
            }
            if self.bulk_fails_after.is_some_and(|k| n >= k) {
                return Err(XfsError::Io(io::Error::other("injected failure")));
            }
            n += 1;
            let rec = BulkStat {
                ino,
                size: node.size,
                blocks: node.blocks,
                block_size: BLOCK,
                mtime: node.mtime,
                nlink: node.nlink,
                mode: node.mode as u16,
            };
            if !each(rec) {
                break;
            }
        }
        Ok(())
    }
}

impl Names for MemXfs {
    type Dir = u64;

    fn root(&self) -> io::Result<OpenedDir<u64>> {
        self.opened(ROOT)
    }

    fn list(&self, dir: &u64, each: &mut dyn FnMut(RawName<'_>)) -> io::Result<()> {
        let entries = self.dirs.get(dir).map_or(&[][..], |v| &v[..]);
        let fail_at = self.list_errors.get(dir).copied();
        for (i, e) in entries.iter().enumerate() {
            if fail_at == Some(i) {
                return Err(io::Error::from(io::ErrorKind::InvalidData));
            }
            each(RawName {
                name: &e.name,
                ino: e.ino,
                d_type: e.d_type,
                clean: e.clean,
            });
        }
        if fail_at.is_some_and(|k| k >= entries.len()) {
            return Err(io::Error::from(io::ErrorKind::InvalidData));
        }
        Ok(())
    }

    fn open_child(&self, parent: &u64, name: &[u8]) -> io::Result<OpenedDir<u64>> {
        let entry = self
            .dirs
            .get(parent)
            .and_then(|v| v.iter().find(|e| e.name == name))
            .ok_or_else(|| io::Error::from_raw_os_error(2))?;
        if let Some(&errno) = self.open_errors.get(&entry.ino) {
            return Err(io::Error::from_raw_os_error(errno));
        }
        // A mount point opens onto the root of what is mounted on it.
        let landed = self
            .mounts
            .get(&entry.ino)
            .or_else(|| self.swaps.get(&entry.ino))
            .copied()
            .unwrap_or(entry.ino);
        self.opened_log(landed);
        let node = self
            .inodes
            .get(&landed)
            .ok_or_else(|| io::Error::from_raw_os_error(2))?;
        match node.mode & 0o170_000 {
            S_IFDIR => self.opened(landed),
            S_IFLNK => Err(io::Error::from_raw_os_error(40)),
            _ => Err(io::Error::from_raw_os_error(20)),
        }
    }

    fn stat_entry(&self, dir: &u64, name: &[u8]) -> io::Result<EntryStat> {
        let entry = self
            .dirs
            .get(dir)
            .and_then(|v| v.iter().find(|e| e.name == name))
            .ok_or_else(|| io::Error::from_raw_os_error(2))?;
        // statx follows a mount point onto what is mounted there.
        let landed = self.mounts.get(&entry.ino).copied().unwrap_or(entry.ino);
        self.stat(landed)
            .ok_or_else(|| io::Error::from_raw_os_error(2))
    }
}

impl MemXfs {
    fn opened_log(&self, ino: u64) {
        self.opened.lock().unwrap().push(ino);
    }
}
