//! Mapping a directory's logical blocks to disk blocks: extent trees and the
//! old direct/indirect pointers. Both are hostile input. Every pointer is
//! checked against the file system, trees are walked with an explicit stack,
//! each node block is visited at most once per inode, and every block that a
//! map causes to be read is charged to a budget shared by the whole read.

use crate::error::ProblemKind;
use crate::le::{bytes_at, join64, to_usize, u16_at, u32_at};
use crate::source::ByteSource;
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};

const EXTENT_MAGIC: u16 = 0xF30A;
/// Extent trees are at most this deep, counting the root.
const MAX_EXTENT_DEPTH: u16 = 5;
/// An extent longer than this is an unwritten one: it reads as zeros.
const MAX_INIT_EXTENT_LEN: u16 = 32768;

/// A stretch of a file: `len` blocks from logical block `lblk`, stored from
/// disk block `pblk` on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Run {
    pub lblk: u64,
    pub pblk: u64,
    pub len: u64,
}

/// Why a map could not be completed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MapFailure {
    pub kind: ProblemKind,
    pub detail: u64,
    /// Whether the cause was a failed read rather than bad data.
    pub io: bool,
}

impl MapFailure {
    fn corrupt(kind: ProblemKind, detail: u64) -> MapFailure {
        MapFailure {
            kind,
            detail,
            io: false,
        }
    }
}

/// Reads whole blocks, within the file system and within a budget.
pub(crate) struct BlockIo<'a, S: ByteSource + ?Sized> {
    src: &'a S,
    block_size: usize,
    block_size_wide: u64,
    blocks_count: u64,
    /// Blocks that may still be read, shared by every reader of the file
    /// system. A real file system has each directory block in one directory,
    /// so the directories together need no more blocks than the file system
    /// has; only blocks shared between directories can use more, and those
    /// are not followed.
    budget: &'a AtomicU64,
    pub blocks_read: u64,
}

impl<'a, S: ByteSource + ?Sized> BlockIo<'a, S> {
    pub fn new(
        src: &'a S,
        block_size: u32,
        blocks_count: u64,
        budget: &'a AtomicU64,
    ) -> Option<Self> {
        Some(BlockIo {
            src,
            block_size: to_usize(u64::from(block_size))?,
            block_size_wide: u64::from(block_size),
            blocks_count,
            budget,
            blocks_read: 0,
        })
    }

    pub fn block_size(&self) -> usize {
        self.block_size
    }

    pub fn blocks_count(&self) -> u64 {
        self.blocks_count
    }

    #[cfg(test)]
    pub fn budget_left(&self) -> u64 {
        self.budget.load(Ordering::Relaxed)
    }

    /// Takes `blocks` out of the budget, or fails without taking anything.
    pub fn charge(&mut self, blocks: u64) -> Result<(), MapFailure> {
        self.budget
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |left| {
                left.checked_sub(blocks)
            })
            .map(|_| ())
            .map_err(|_| MapFailure::corrupt(ProblemKind::ReadBudgetExceeded, blocks))
    }

    /// Reads `count` blocks from `first` into `out`, which is resized. The
    /// budget is charged by whoever planned the read.
    pub fn read(&mut self, first: u64, count: u64, out: &mut Vec<u8>) -> Result<(), MapFailure> {
        first
            .checked_add(count)
            .filter(|&e| e <= self.blocks_count)
            .ok_or(MapFailure::corrupt(ProblemKind::BlockOutOfRange, first))?;
        let bytes = count
            .checked_mul(self.block_size_wide)
            .and_then(to_usize)
            .ok_or(MapFailure::corrupt(ProblemKind::BlockOutOfRange, first))?;
        let offset = first
            .checked_mul(self.block_size_wide)
            .ok_or(MapFailure::corrupt(ProblemKind::BlockOutOfRange, first))?;
        out.clear();
        out.resize(bytes, 0);
        self.blocks_read = self.blocks_read.saturating_add(count);
        self.src.read_at(offset, out).map_err(|_| MapFailure {
            kind: ProblemKind::BlockUnreadable,
            detail: first,
            io: true,
        })
    }

    /// Reads one block that the map itself needs (an index or indirect
    /// block), charging the budget.
    fn read_map_block(&mut self, block: u64, out: &mut Vec<u8>) -> Result<(), MapFailure> {
        self.charge(1)?;
        self.read(block, 1, out)
    }
}

/// Collects runs, joining stretches that continue each other, and charging
/// the budget for the blocks they will cost to read.
struct RunList {
    runs: Vec<Run>,
    /// Logical blocks at and past this are not wanted.
    limit: u64,
}

impl RunList {
    fn add<S: ByteSource + ?Sized>(
        &mut self,
        io: &mut BlockIo<'_, S>,
        lblk: u64,
        pblk: u64,
        len: u64,
    ) -> Result<(), MapFailure> {
        if lblk >= self.limit || len == 0 {
            return Ok(());
        }
        let len = len.min(self.limit.saturating_sub(lblk));
        pblk.checked_add(len)
            .filter(|&e| e <= io.blocks_count())
            .ok_or(MapFailure::corrupt(ProblemKind::BlockOutOfRange, pblk))?;
        io.charge(len)?;
        if let Some(last) = self.runs.last_mut()
            && last.lblk.checked_add(last.len) == Some(lblk)
            && last.pblk.checked_add(last.len) == Some(pblk)
        {
            last.len = last.len.saturating_add(len);
            return Ok(());
        }
        self.runs.push(Run { lblk, pblk, len });
        Ok(())
    }
}

/// The first physical block an inode's extent root points at, to order
/// directories for reading. Zero effort: only looks at the root.
pub(crate) fn first_block_hint(root: &[u8]) -> u64 {
    if u16_at(root, 0) == Some(EXTENT_MAGIC)
        && u16_at(root, 6) == Some(0)
        && u16_at(root, 2).is_some_and(|n| n > 0)
        && let (Some(hi), Some(lo)) = (u16_at(root, 12 + 6), u32_at(root, 12 + 8))
    {
        return join64(lo, u32::from(hi));
    }
    u64::MAX
}

struct Node {
    data: Vec<u8>,
    entries: usize,
    depth: u16,
    next: usize,
}

fn read_header(data: &[u8], expect_depth: Option<u16>) -> Result<(usize, u16), MapFailure> {
    let bad = |d| MapFailure::corrupt(ProblemKind::ExtentTreeCorrupt, d);
    let (Some(magic), Some(entries), Some(max), Some(depth)) = (
        u16_at(data, 0),
        u16_at(data, 2),
        u16_at(data, 4),
        u16_at(data, 6),
    ) else {
        return Err(bad(0));
    };
    if magic != EXTENT_MAGIC {
        return Err(bad(1));
    }
    if depth >= MAX_EXTENT_DEPTH || expect_depth.is_some_and(|d| d != depth) {
        return Err(bad(2));
    }
    // The node holds 12 bytes of header and 12 bytes per entry.
    let capacity = data.len().saturating_sub(12) / 12;
    if entries > max || usize::from(entries) > capacity {
        return Err(bad(3));
    }
    Ok((usize::from(entries), depth))
}

/// Maps an extent tree whose root is `root` (the 60 bytes of `i_block`).
/// Returns the runs found, even when a later part of the tree fails.
pub(crate) fn extent_runs<S: ByteSource + ?Sized>(
    io: &mut BlockIo<'_, S>,
    root: &[u8],
    limit_blocks: u64,
) -> (Vec<Run>, Option<MapFailure>) {
    let mut list = RunList {
        runs: Vec::new(),
        limit: limit_blocks,
    };
    let failure = walk_extents(io, root, &mut list).err();
    (list.runs, failure)
}

fn walk_extents<S: ByteSource + ?Sized>(
    io: &mut BlockIo<'_, S>,
    root: &[u8],
    list: &mut RunList,
) -> Result<(), MapFailure> {
    let (entries, depth) = read_header(root, None)?;
    let mut stack = vec![Node {
        data: root.to_vec(),
        entries,
        depth,
        next: 0,
    }];
    // Node blocks seen for this inode: a tree that reaches one twice is a
    // loop or a shared node, and is not followed again.
    let mut seen: HashSet<u64> = HashSet::new();
    let mut scratch = Vec::new();
    while let Some(top) = stack.last_mut() {
        if top.next >= top.entries {
            stack.pop();
            continue;
        }
        let at = top.next.saturating_mul(12).saturating_add(12);
        top.next = top.next.saturating_add(1);
        let corrupt = MapFailure::corrupt(ProblemKind::ExtentTreeCorrupt, 4);
        let entry = bytes_at(&top.data, at, 12).ok_or(corrupt)?;
        if top.depth == 0 {
            let lblk = u64::from(u32_at(entry, 0).ok_or(corrupt)?);
            let raw_len = u16_at(entry, 4).ok_or(corrupt)?;
            let hi = u32::from(u16_at(entry, 6).ok_or(corrupt)?);
            let lo = u32_at(entry, 8).ok_or(corrupt)?;
            if raw_len == 0 {
                return Err(corrupt);
            }
            // An unwritten extent reads as zeros: nothing to parse in it.
            if raw_len > MAX_INIT_EXTENT_LEN {
                continue;
            }
            list.add(io, lblk, join64(lo, hi), u64::from(raw_len))?;
        } else {
            let lo = u32_at(entry, 4).ok_or(corrupt)?;
            let hi = u32::from(u16_at(entry, 8).ok_or(corrupt)?);
            let child = join64(lo, hi);
            let child_depth = top.depth.saturating_sub(1);
            if child >= io.blocks_count() {
                return Err(MapFailure::corrupt(ProblemKind::BlockOutOfRange, child));
            }
            if !seen.insert(child) {
                return Err(MapFailure::corrupt(ProblemKind::ExtentTreeLoop, child));
            }
            io.read_map_block(child, &mut scratch)?;
            let (entries, depth) = read_header(&scratch, Some(child_depth))?;
            stack.push(Node {
                data: std::mem::take(&mut scratch),
                entries,
                depth,
                next: 0,
            });
        }
    }
    Ok(())
}

/// Maps the classic block pointers: twelve direct, then one single, one
/// double and one triple indirect block.
pub(crate) fn legacy_runs<S: ByteSource + ?Sized>(
    io: &mut BlockIo<'_, S>,
    i_block: &[u8],
    limit_blocks: u64,
) -> (Vec<Run>, Option<MapFailure>) {
    let mut list = RunList {
        runs: Vec::new(),
        limit: limit_blocks,
    };
    let failure = walk_legacy(io, i_block, &mut list).err();
    (list.runs, failure)
}

struct Level {
    data: Vec<u8>,
    /// Pointer level: 1 holds data blocks, 2 holds level-1 blocks, ...
    level: u8,
    next: u64,
    /// Logical block of the first entry.
    base: u64,
}

fn walk_legacy<S: ByteSource + ?Sized>(
    io: &mut BlockIo<'_, S>,
    i_block: &[u8],
    list: &mut RunList,
) -> Result<(), MapFailure> {
    let short = MapFailure::corrupt(ProblemKind::BlockOutOfRange, 0);
    for i in 0..12u64 {
        let at = to_usize(i.saturating_mul(4)).ok_or(short)?;
        let p = u64::from(u32_at(i_block, at).ok_or(short)?);
        if p != 0 {
            list.add(io, i, p, 1)?;
        }
    }
    let per_block = u64::try_from(io.block_size() / 4).unwrap_or(0);
    let mut base = 12u64;
    let mut seen: HashSet<u64> = HashSet::new();
    let mut scratch = Vec::new();
    for (slot, level) in [(12usize, 1u8), (13, 2), (14, 3)] {
        // How many logical blocks one pointer at this level covers.
        let span_all = per_block.checked_pow(u32::from(level));
        let Some(span_all) = span_all else {
            return Ok(());
        };
        let root = u64::from(u32_at(i_block, slot.saturating_mul(4)).ok_or(short)?);
        let here = base;
        base = base.saturating_add(span_all);
        if root == 0 || here >= list.limit {
            continue;
        }
        if root >= io.blocks_count() {
            return Err(MapFailure::corrupt(ProblemKind::BlockOutOfRange, root));
        }
        if !seen.insert(root) {
            return Err(MapFailure::corrupt(ProblemKind::ExtentTreeLoop, root));
        }
        io.read_map_block(root, &mut scratch)?;
        let mut stack = vec![Level {
            data: std::mem::take(&mut scratch),
            level,
            next: 0,
            base: here,
        }];
        while let Some(top) = stack.last_mut() {
            if top.next >= per_block {
                stack.pop();
                continue;
            }
            let i = top.next;
            top.next = top.next.saturating_add(1);
            let span = per_block
                .checked_pow(u32::from(top.level.saturating_sub(1)))
                .unwrap_or(u64::MAX);
            let child_base = top.base.saturating_add(i.saturating_mul(span));
            if child_base >= list.limit {
                stack.pop();
                continue;
            }
            let at = to_usize(i.saturating_mul(4)).ok_or(short)?;
            let p = u64::from(u32_at(&top.data, at).ok_or(short)?);
            if p == 0 {
                continue;
            }
            if top.level == 1 {
                list.add(io, child_base, p, 1)?;
                continue;
            }
            if p >= io.blocks_count() {
                return Err(MapFailure::corrupt(ProblemKind::BlockOutOfRange, p));
            }
            if !seen.insert(p) {
                return Err(MapFailure::corrupt(ProblemKind::ExtentTreeLoop, p));
            }
            let child_level = top.level.saturating_sub(1);
            io.read_map_block(p, &mut scratch)?;
            stack.push(Level {
                data: std::mem::take(&mut scratch),
                level: child_level,
                next: 0,
                base: child_base,
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::cast_possible_truncation,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects
    )]
    use super::*;

    fn leaf_root(extents: &[(u32, u16, u64)]) -> Vec<u8> {
        let mut r = vec![0u8; 60];
        r[0..2].copy_from_slice(&EXTENT_MAGIC.to_le_bytes());
        r[2..4].copy_from_slice(&(extents.len() as u16).to_le_bytes());
        r[4..6].copy_from_slice(&4u16.to_le_bytes());
        for (i, (l, len, p)) in extents.iter().enumerate() {
            let at = 12 + i * 12;
            r[at..at + 4].copy_from_slice(&l.to_le_bytes());
            r[at + 4..at + 6].copy_from_slice(&len.to_le_bytes());
            r[at + 6..at + 8].copy_from_slice(&((p >> 32) as u16).to_le_bytes());
            r[at + 8..at + 12].copy_from_slice(&(*p as u32).to_le_bytes());
        }
        r
    }

    fn make_io<'a>(dev: &'a Vec<u8>, budget: &'a AtomicU64) -> BlockIo<'a, Vec<u8>> {
        BlockIo::new(dev, 1024, (dev.len() / 1024) as u64, budget).unwrap()
    }

    #[test]
    fn leaf_extents_become_runs_and_cost_budget() {
        let dev = vec![0u8; 1024 * 100];
        let budget = AtomicU64::new(100);
        let mut io = make_io(&dev, &budget);
        let (runs, fail) = extent_runs(
            &mut io,
            &leaf_root(&[(0, 2, 10), (2, 3, 12), (9, 1, 50)]),
            100,
        );
        assert!(fail.is_none());
        // The first two continue each other.
        assert_eq!(
            runs,
            [
                Run {
                    lblk: 0,
                    pblk: 10,
                    len: 5
                },
                Run {
                    lblk: 9,
                    pblk: 50,
                    len: 1
                }
            ]
        );
        assert_eq!(io.budget_left(), 100 - 6);
    }

    #[test]
    fn bad_extents_fail_with_typed_reasons() {
        let dev = vec![0u8; 1024 * 100];
        for (root, kind) in [
            (leaf_root(&[(0, 0, 10)]), ProblemKind::ExtentTreeCorrupt),
            (leaf_root(&[(0, 5, 98)]), ProblemKind::BlockOutOfRange),
            (
                leaf_root(&[(0, 5, u64::MAX >> 20)]),
                ProblemKind::BlockOutOfRange,
            ),
            (vec![0u8; 60], ProblemKind::ExtentTreeCorrupt),
            (vec![1u8; 5], ProblemKind::ExtentTreeCorrupt),
        ] {
            let budget = AtomicU64::new(100);
            let mut io = make_io(&dev, &budget);
            let (_, fail) = extent_runs(&mut io, &root, 100);
            assert_eq!(fail.map(|f| f.kind), Some(kind));
        }
        // More entries than the node holds.
        let mut r = leaf_root(&[(0, 1, 10)]);
        r[2..4].copy_from_slice(&5u16.to_le_bytes());
        r[4..6].copy_from_slice(&5u16.to_le_bytes());
        let budget = AtomicU64::new(100);
        let mut io2 = make_io(&dev, &budget);
        assert!(extent_runs(&mut io2, &r, 100).1.is_some());
    }

    #[test]
    fn unwritten_extents_and_the_size_limit() {
        let dev = vec![0u8; 1024 * 100];
        let budget = AtomicU64::new(100);
        let mut io = make_io(&dev, &budget);
        let (runs, fail) = extent_runs(&mut io, &leaf_root(&[(0, 32769, 10), (1, 50, 20)]), 4);
        assert!(fail.is_none());
        assert_eq!(
            runs,
            [Run {
                lblk: 1,
                pblk: 20,
                len: 3
            }]
        );
    }

    #[test]
    fn an_index_that_points_at_itself_is_a_loop_not_a_hang() {
        let mut dev = vec![0u8; 1024 * 100];
        // Block 7 is an index node (depth 1) with one entry pointing at 7.
        let at = 7 * 1024;
        dev[at..at + 2].copy_from_slice(&EXTENT_MAGIC.to_le_bytes());
        dev[at + 2..at + 4].copy_from_slice(&1u16.to_le_bytes());
        dev[at + 4..at + 6].copy_from_slice(&80u16.to_le_bytes());
        dev[at + 6..at + 8].copy_from_slice(&1u16.to_le_bytes());
        dev[at + 12 + 4..at + 12 + 8].copy_from_slice(&7u32.to_le_bytes());
        // The root (depth 2) points at block 7.
        let mut root = vec![0u8; 60];
        root[0..2].copy_from_slice(&EXTENT_MAGIC.to_le_bytes());
        root[2..4].copy_from_slice(&1u16.to_le_bytes());
        root[4..6].copy_from_slice(&4u16.to_le_bytes());
        root[6..8].copy_from_slice(&2u16.to_le_bytes());
        root[12 + 4..12 + 8].copy_from_slice(&7u32.to_le_bytes());
        let budget = AtomicU64::new(100);
        let mut io = make_io(&dev, &budget);
        let (_, fail) = extent_runs(&mut io, &root, 100);
        assert_eq!(fail.map(|f| f.kind), Some(ProblemKind::ExtentTreeLoop));
    }

    #[test]
    fn legacy_pointers_direct_and_indirect() {
        let mut dev = vec![0u8; 1024 * 100];
        let mut iblock = vec![0u8; 60];
        iblock[0..4].copy_from_slice(&30u32.to_le_bytes());
        iblock[4..8].copy_from_slice(&31u32.to_le_bytes());
        // Indirect block 40 maps logical 12 and 14.
        iblock[48..52].copy_from_slice(&40u32.to_le_bytes());
        dev[40 * 1024..40 * 1024 + 4].copy_from_slice(&60u32.to_le_bytes());
        dev[40 * 1024 + 8..40 * 1024 + 12].copy_from_slice(&62u32.to_le_bytes());
        let budget = AtomicU64::new(100);
        let mut io = make_io(&dev, &budget);
        let (runs, fail) = legacy_runs(&mut io, &iblock, 1000);
        assert!(fail.is_none());
        assert_eq!(
            runs,
            [
                Run {
                    lblk: 0,
                    pblk: 30,
                    len: 2
                },
                Run {
                    lblk: 12,
                    pblk: 60,
                    len: 1
                },
                Run {
                    lblk: 14,
                    pblk: 62,
                    len: 1
                }
            ]
        );
        // A pointer outside the file system.
        iblock[8..12].copy_from_slice(&5000u32.to_le_bytes());
        let budget = AtomicU64::new(100);
        let mut io = make_io(&dev, &budget);
        let (_, fail) = legacy_runs(&mut io, &iblock, 1000);
        assert_eq!(fail.map(|f| f.kind), Some(ProblemKind::BlockOutOfRange));
    }

    #[test]
    fn the_budget_stops_blocks_shared_between_directories() {
        let dev = vec![0u8; 1024 * 100];
        let budget = AtomicU64::new(100);
        let mut io = make_io(&dev, &budget);
        let root = leaf_root(&[(0, 60, 10)]);
        assert!(extent_runs(&mut io, &root, 1000).1.is_none());
        // Another directory claims the same 60 blocks: 40 left is not enough.
        let (_, fail) = extent_runs(&mut io, &root, 1000);
        assert_eq!(fail.map(|f| f.kind), Some(ProblemKind::ReadBudgetExceeded));
    }
}
