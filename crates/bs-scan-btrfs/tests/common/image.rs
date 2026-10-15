//! A btrfs image file read without mounting, as a [`TreeSource`]: the
//! superblock, the chunk map, the tree of trees and the B-trees themselves.
//!
//! It is test machinery: it lets the reader run over a real file system that
//! `mkfs.btrfs --rootdir` built, with no privileges. It assumes what `mkfs`
//! makes (one device, no checksum verification) and panics on anything else.
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
use bs_scan_btrfs::{BtrfsError, Flow, Item, Key, TreeSource};
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::Path;

const SUPER_OFFSET: u64 = 65536;
const HEADER: usize = 101;

fn u16le(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(b[at..at + 2].try_into().unwrap())
}
fn u32le(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}
fn u64le(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

fn key_at(b: &[u8], at: usize) -> Key {
    Key {
        objectid: u64le(b, at),
        ty: b[at + 8],
        offset: u64le(b, at + 9),
    }
}

struct Chunk {
    logical: u64,
    len: u64,
    physical: u64,
}

pub struct ImageSource {
    file: File,
    nodesize: usize,
    chunks: Vec<Chunk>,
    root_tree: (u64, u8),
}

impl ImageSource {
    pub fn open(path: &Path) -> ImageSource {
        let file = File::open(path).unwrap();
        let mut sb = vec![0u8; 4096];
        file.read_exact_at(&mut sb, SUPER_OFFSET).unwrap();
        assert_eq!(&sb[0x40..0x48], b"_BHRfS_M", "not a btrfs image");
        let nodesize = u32le(&sb, 0x94) as usize;
        let root = u64le(&sb, 0x50);
        let chunk_root = u64le(&sb, 0x58);
        let sys_size = u32le(&sb, 0xa0) as usize;
        let root_level = sb[0xc6];
        let chunk_level = sb[0xc7];
        let mut me = ImageSource {
            file,
            nodesize,
            chunks: Vec::new(),
            root_tree: (root, root_level),
        };
        // The system chunks, from the superblock, say where the chunk tree is.
        let array = &sb[0x32b..0x32b + sys_size];
        let mut pos = 0;
        while pos < array.len() {
            let key = key_at(array, pos);
            assert_eq!(key.ty, 228, "a chunk key");
            pos += 17;
            pos += me.take_chunk(key.offset, &array[pos..]);
        }
        // Then every chunk, from the chunk tree.
        let mut found = Vec::new();
        me.walk(
            chunk_root,
            chunk_level,
            Key::MIN,
            Key::MAX,
            &mut |item: Item<'_>| {
                if item.key.ty == 228 {
                    found.push((item.key.offset, item.data.to_vec()));
                }
                Flow::Continue
            },
        );
        for (logical, data) in found {
            if !me.chunks.iter().any(|c| c.logical == logical) {
                me.take_chunk(logical, &data);
            }
        }
        me
    }

    /// Reads a chunk item; returns its length in bytes.
    fn take_chunk(&mut self, logical: u64, item: &[u8]) -> usize {
        let len = u64le(item, 0);
        let stripes = u16le(item, 44) as usize;
        let physical = u64le(item, 48 + 8);
        self.chunks.push(Chunk {
            logical,
            len,
            physical,
        });
        48 + stripes * 32
    }

    fn phys(&self, logical: u64) -> u64 {
        let c = self
            .chunks
            .iter()
            .find(|c| logical >= c.logical && logical < c.logical + c.len)
            .unwrap_or_else(|| panic!("logical {logical} is in no chunk"));
        c.physical + (logical - c.logical)
    }

    pub fn read_logical(&self, logical: u64, len: usize) -> Vec<u8> {
        let mut b = vec![0u8; len];
        self.file.read_exact_at(&mut b, self.phys(logical)).unwrap();
        b
    }

    fn node(&self, logical: u64) -> Vec<u8> {
        self.read_logical(logical, self.nodesize)
    }

    /// Calls `each` for the items of the tree rooted at `bytenr` in
    /// `from..=to`, in order. Returns `false` once `each` stops it.
    fn walk(
        &self,
        bytenr: u64,
        level: u8,
        from: Key,
        to: Key,
        each: &mut dyn FnMut(Item<'_>) -> Flow,
    ) -> bool {
        let node = self.node(bytenr);
        assert_eq!(u64le(&node, 48), bytenr, "node at its own address");
        let nritems = u32le(&node, 96) as usize;
        assert_eq!(node[100], level, "level");
        if level == 0 {
            for i in 0..nritems {
                let at = HEADER + i * 25;
                let key = key_at(&node, at);
                if key < from {
                    continue;
                }
                if key > to {
                    return false;
                }
                let off = HEADER + u32le(&node, at + 17) as usize;
                let size = u32le(&node, at + 21) as usize;
                if each(Item {
                    key,
                    data: &node[off..off + size],
                }) == Flow::Stop
                {
                    return false;
                }
            }
            return true;
        }
        for i in 0..nritems {
            let at = HEADER + i * 33;
            let key = key_at(&node, at);
            if key > to {
                return false;
            }
            if i + 1 < nritems && key_at(&node, at + 33) <= from {
                continue;
            }
            let child = u64le(&node, at + 17);
            if !self.walk(child, level - 1, from, to, each) {
                return false;
            }
        }
        true
    }

    fn root_of(&self, tree: u64) -> Option<(u64, u8)> {
        if tree == ROOT_TREE_ID {
            return Some(self.root_tree);
        }
        let mut found = None;
        let key = Key {
            objectid: tree,
            ty: ROOT_ITEM,
            offset: 0,
        };
        self.walk(self.root_tree.0, self.root_tree.1, key, key, &mut |item| {
            found = Some((u64le(item.data, 176), item.data[238]));
            Flow::Stop
        });
        found
    }
}

impl TreeSource for ImageSource {
    fn search(
        &self,
        tree: u64,
        from: Key,
        to: Key,
        each: &mut dyn FnMut(Item<'_>) -> Flow,
    ) -> Result<(), BtrfsError> {
        let (bytenr, level) = self
            .root_of(tree)
            .ok_or_else(|| BtrfsError::Io(std::io::Error::from(std::io::ErrorKind::NotFound)))?;
        self.walk(bytenr, level, from, to, each);
        Ok(())
    }
}
