//! A btrfs file system held in memory as tree items, for tests: a builder
//! that writes the items the kernel would, and a [`TreeSource`] that serves
//! them. The items have the layout of `btrfs_tree.h`, so the reader parses
//! them as it parses the kernel's.
#![allow(dead_code)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::cast_possible_truncation,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use bs_scan_btrfs::consts::*;
use bs_scan_btrfs::{BtrfsError, Flow, Item, Key, MountPoint, TreeSource};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io;
use std::sync::Mutex;

pub const S_IFREG: u32 = 0o100_000;
pub const S_IFDIR: u32 = 0o040_000;
pub const S_IFLNK: u32 = 0o120_000;
pub const S_IFIFO: u32 = 0o010_000;

pub fn inode_item(size: u64, nlink: u32, mode: u32, mtime: i64) -> Vec<u8> {
    let mut b = vec![0u8; 160];
    b[16..24].copy_from_slice(&size.to_le_bytes());
    b[40..44].copy_from_slice(&nlink.to_le_bytes());
    b[52..56].copy_from_slice(&mode.to_le_bytes());
    b[136..144].copy_from_slice(&(mtime as u64).to_le_bytes());
    b
}

pub fn dir_item(target: u64, key_type: u8, ft: u8, name: &[u8]) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&target.to_le_bytes());
    b.push(key_type);
    b.extend_from_slice(&0u64.to_le_bytes());
    b.extend_from_slice(&1u64.to_le_bytes());
    b.extend_from_slice(&0u16.to_le_bytes());
    b.extend_from_slice(&(name.len() as u16).to_le_bytes());
    b.push(ft);
    b.extend_from_slice(name);
    b
}

/// What a file's data is made of, in file order.
#[derive(Clone, Copy, Debug)]
pub enum Ext {
    /// Data inside the tree: no extent.
    Inline,
    /// A hole.
    Hole,
    /// A range of disk blocks: address, size on disk, compressed or not.
    Disk(u64, u64, bool),
    /// The same, preallocated.
    Prealloc(u64, u64),
}

fn extent_item(e: Ext, num_bytes: u64) -> Vec<u8> {
    match e {
        Ext::Inline => {
            let mut b = vec![0u8; 21 + num_bytes as usize];
            b[8..16].copy_from_slice(&num_bytes.to_le_bytes());
            b[20] = 0;
            b
        }
        Ext::Hole => disk_extent(0, 0, false, false, num_bytes),
        Ext::Disk(addr, bytes, compressed) => {
            disk_extent(addr, bytes, compressed, false, num_bytes)
        }
        Ext::Prealloc(addr, bytes) => disk_extent(addr, bytes, false, true, num_bytes),
    }
}

fn disk_extent(addr: u64, bytes: u64, compressed: bool, prealloc: bool, num_bytes: u64) -> Vec<u8> {
    let mut b = vec![0u8; 53];
    b[8..16].copy_from_slice(&num_bytes.to_le_bytes());
    b[16] = u8::from(compressed) * 3;
    b[20] = if prealloc { 2 } else { 1 };
    b[21..29].copy_from_slice(&addr.to_le_bytes());
    b[29..37].copy_from_slice(&bytes.to_le_bytes());
    b[45..53].copy_from_slice(&num_bytes.to_le_bytes());
    b
}

/// The file system: items by tree and key.
#[derive(Default)]
pub struct MemFs {
    pub items: BTreeMap<(u64, Key), Vec<u8>>,
    next_ino: HashMap<u64, u64>,
    next_index: HashMap<(u64, u64), u64>,
    next_extent: u64,
}

pub const MTIME: i64 = 1_700_000_000;

impl MemFs {
    /// A file system with the default subvolume (tree 5) and its root.
    pub fn new() -> MemFs {
        let mut fs = MemFs {
            next_extent: 1 << 20,
            ..MemFs::default()
        };
        fs.make_tree(FS_TREE_ID);
        fs
    }

    fn put(&mut self, tree: u64, objectid: u64, ty: u8, offset: u64, data: Vec<u8>) {
        self.items.insert(
            (
                tree,
                Key {
                    objectid,
                    ty,
                    offset,
                },
            ),
            data,
        );
    }

    /// A subvolume tree with its root directory (inode 256), registered in
    /// the tree of trees.
    pub fn make_tree(&mut self, tree: u64) {
        self.put(ROOT_TREE_ID, tree, ROOT_ITEM, 0, vec![0u8; 439]);
        self.put(
            tree,
            FIRST_FREE,
            INODE_ITEM,
            0,
            inode_item(0, 1, S_IFDIR | 0o755, MTIME),
        );
        self.next_ino.insert(tree, FIRST_FREE + 1);
    }

    fn new_ino(&mut self, tree: u64) -> u64 {
        let n = self.next_ino.entry(tree).or_insert(FIRST_FREE + 1);
        let ino = *n;
        *n += 1;
        ino
    }

    fn entry(&mut self, tree: u64, dir: u64, name: &[u8], target: u64, key_type: u8, ft: u8) {
        let idx = self.next_index.entry((tree, dir)).or_insert(2);
        let at = *idx;
        *idx += 1;
        self.put(
            tree,
            dir,
            DIR_INDEX,
            at,
            dir_item(target, key_type, ft, name),
        );
    }

    pub fn mkdir(&mut self, tree: u64, parent: u64, name: &str) -> u64 {
        let ino = self.new_ino(tree);
        self.put(
            tree,
            ino,
            INODE_ITEM,
            0,
            inode_item(0, 1, S_IFDIR | 0o755, MTIME + 1),
        );
        self.entry(tree, parent, name.as_bytes(), ino, INODE_ITEM, 2);
        ino
    }

    /// A regular file of `size` bytes made of `extents`, each covering an
    /// equal share of the size.
    pub fn file(&mut self, tree: u64, parent: u64, name: &[u8], size: u64, extents: &[Ext]) -> u64 {
        let ino = self.new_ino(tree);
        self.put(
            tree,
            ino,
            INODE_ITEM,
            0,
            inode_item(size, 1, S_IFREG | 0o644, MTIME + 2),
        );
        let share = size / extents.len().max(1) as u64;
        for (i, e) in extents.iter().enumerate() {
            self.put(
                tree,
                ino,
                EXTENT_DATA,
                share * i as u64,
                extent_item(*e, share),
            );
        }
        self.entry(tree, parent, name, ino, INODE_ITEM, 1);
        ino
    }

    /// A file with `n` bytes of fresh, unshared extents on disk.
    pub fn plain(&mut self, tree: u64, parent: u64, name: &str, bytes: u64) -> u64 {
        let addr = self.next_extent;
        self.next_extent += bytes.max(4096);
        self.file(
            tree,
            parent,
            name.as_bytes(),
            bytes,
            &[Ext::Disk(addr, bytes, false)],
        )
    }

    /// The extent a later file can refer to as well.
    pub fn fresh_extent(&mut self, bytes: u64) -> u64 {
        let addr = self.next_extent;
        self.next_extent += bytes.max(4096);
        addr
    }

    pub fn symlink(&mut self, tree: u64, parent: u64, name: &str, target: &str) -> u64 {
        let ino = self.new_ino(tree);
        self.put(
            tree,
            ino,
            INODE_ITEM,
            0,
            inode_item(target.len() as u64, 1, S_IFLNK | 0o777, MTIME + 3),
        );
        self.put(
            tree,
            ino,
            EXTENT_DATA,
            0,
            extent_item(Ext::Inline, target.len() as u64),
        );
        self.entry(tree, parent, name.as_bytes(), ino, INODE_ITEM, 7);
        ino
    }

    pub fn fifo(&mut self, tree: u64, parent: u64, name: &str) -> u64 {
        let ino = self.new_ino(tree);
        self.put(
            tree,
            ino,
            INODE_ITEM,
            0,
            inode_item(0, 1, S_IFIFO | 0o600, MTIME + 4),
        );
        self.entry(tree, parent, name.as_bytes(), ino, INODE_ITEM, 5);
        ino
    }

    /// Another name for `ino` (and the link count to match).
    pub fn link(&mut self, tree: u64, parent: u64, name: &str, ino: u64) {
        let key = Key {
            objectid: ino,
            ty: INODE_ITEM,
            offset: 0,
        };
        let item = self.items.get_mut(&(tree, key)).unwrap();
        let nlink = u32::from_le_bytes(item[40..44].try_into().unwrap()) + 1;
        item[40..44].copy_from_slice(&nlink.to_le_bytes());
        self.entry(tree, parent, name.as_bytes(), ino, INODE_ITEM, 1);
    }

    /// Another entry for the directory `dir`, which btrfs does not allow.
    pub fn link_dir(&mut self, tree: u64, parent: u64, name: &str, dir: u64) {
        self.entry(tree, parent, name.as_bytes(), dir, INODE_ITEM, 2);
    }

    /// A subvolume `child` nested at `parent/name` of `tree`.
    pub fn subvolume(&mut self, tree: u64, parent: u64, name: &str, child: u64) {
        self.make_tree(child);
        self.entry(tree, parent, name.as_bytes(), child, ROOT_ITEM, 2);
    }

    pub fn source(self) -> MemSource {
        MemSource {
            fs: self,
            fail_trees: HashSet::new(),
            fail_after_items: Mutex::new(None),
            searches: Mutex::new(Vec::new()),
            mounts: Vec::new(),
            mounts_fail: false,
        }
    }
}

/// Serves the items of a [`MemFs`], optionally failing.
pub struct MemSource {
    pub fs: MemFs,
    /// Searches of these trees fail at once.
    pub fail_trees: HashSet<u64>,
    /// Fail after this many items have been delivered (over all searches).
    pub fail_after_items: Mutex<Option<u64>>,
    /// The trees searched, in order.
    pub searches: Mutex<Vec<u64>>,
    /// The mounts below the start directory.
    pub mounts: Vec<MountPoint>,
    /// The mount table cannot be read.
    pub mounts_fail: bool,
}

impl TreeSource for MemSource {
    fn mounts(&self) -> Result<Vec<MountPoint>, BtrfsError> {
        if self.mounts_fail {
            return Err(BtrfsError::Io(io::Error::from(
                io::ErrorKind::PermissionDenied,
            )));
        }
        Ok(self.mounts.clone())
    }

    fn search(
        &self,
        tree: u64,
        from: Key,
        to: Key,
        each: &mut dyn FnMut(Item<'_>) -> Flow,
    ) -> Result<(), BtrfsError> {
        self.searches.lock().unwrap().push(tree);
        if self.fail_trees.contains(&tree) {
            return Err(BtrfsError::Io(io::Error::from(io::ErrorKind::NotFound)));
        }
        for ((t, key), data) in self.fs.items.range((tree, from)..=(tree, to)) {
            debug_assert_eq!(*t, tree);
            {
                let mut left = self.fail_after_items.lock().unwrap();
                if let Some(n) = left.as_mut() {
                    if *n == 0 {
                        return Err(BtrfsError::Io(io::Error::other("injected failure")));
                    }
                    *n -= 1;
                }
            }
            if each(Item { key: *key, data }) == Flow::Stop {
                return Ok(());
            }
        }
        Ok(())
    }
}
