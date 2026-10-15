//! Storage: chunked struct-of-arrays columns and the name pool.
//!
//! Nodes live in fixed-size chunks of parallel arrays, each chunk behind an
//! `Arc`. Publishing a snapshot clones the list of chunk pointers, not the
//! data. The owner copies a chunk only when it modifies one that a snapshot
//! still holds, so the cost of a publication is proportional to what changed
//! since the last one. The name pool works the same way.

use crate::error::ApplyError;
use crate::hash::FastMap;
use crate::types::{
    ByteCount, DeleteState, DeleteTotals, NodeFlags, NodeId, NodeKind, ReadFailure,
};
use std::sync::Arc;

pub(crate) const NONE: u32 = u32::MAX;

pub(crate) const CHUNK_BITS: u32 = 10;
pub(crate) const CHUNK_LEN: usize = 1 << CHUNK_BITS;
const CHUNK_MASK: u32 = (1 << CHUNK_BITS) - 1;

const POOL_BITS: u32 = 16;
pub(crate) const POOL_CHUNK_LEN: usize = 1 << POOL_BITS;
const POOL_MASK: u32 = (1 << POOL_BITS) - 1;
/// Pool offsets are `u32`: 65536 chunks of 64 KiB.
const POOL_MAX_CHUNKS: usize = 1 << (32 - POOL_BITS);

// Layout of the per-node `bits` column.
const FLAG_BITS: u16 = NodeFlags::STORED;
const DELETE_SHIFT: u16 = 12;
const KIND_SHIFT: u16 = 14;

/// One chunk of node columns. 47 bytes per node.
#[derive(Clone)]
pub(crate) struct NodeChunk {
    parent: [u32; CHUNK_LEN],
    first_child: [u32; CHUNK_LEN],
    next_sibling: [u32; CHUNK_LEN],
    logical: [u64; CHUNK_LEN],
    allocated: [u64; CHUNK_LEN],
    files: [u32; CHUNK_LEN],
    mtime: [i64; CHUNK_LEN],
    name: [u32; CHUNK_LEN],
    bits: [u16; CHUNK_LEN],
    category: [u8; CHUNK_LEN],
}

impl NodeChunk {
    fn new() -> NodeChunk {
        NodeChunk {
            parent: [NONE; CHUNK_LEN],
            first_child: [NONE; CHUNK_LEN],
            next_sibling: [NONE; CHUNK_LEN],
            logical: [0; CHUNK_LEN],
            allocated: [0; CHUNK_LEN],
            files: [0; CHUNK_LEN],
            mtime: [0; CHUNK_LEN],
            name: [0; CHUNK_LEN],
            bits: [0; CHUNK_LEN],
            category: [0; CHUNK_LEN],
        }
    }
}

/// One 64 KiB block of name bytes. A name never straddles two blocks.
#[derive(Clone)]
struct PoolChunk {
    data: Box<[u8; POOL_CHUNK_LEN]>,
    used: usize,
}

impl PoolChunk {
    fn new() -> PoolChunk {
        PoolChunk {
            data: Box::new([0; POOL_CHUNK_LEN]),
            used: 0,
        }
    }
}

/// Append-only byte pool. Each name is stored as a length (one byte below
/// 128, otherwise two) followed by its bytes.
#[derive(Clone, Default)]
pub(crate) struct Pool {
    chunks: Vec<Arc<PoolChunk>>,
}

impl Pool {
    /// Stores `name` (at most `MAX_NAME_LEN` bytes) and returns its offset.
    fn intern(&mut self, name: &[u8]) -> Result<u32, ApplyError> {
        let len = name.len();
        if len > 0x7FFF {
            return Err(ApplyError::NamePoolFull);
        }
        let header = if len < 0x80 { 1 } else { 2 };
        let need = header + len;
        let fits = self
            .chunks
            .last()
            .is_some_and(|c| c.used + need <= POOL_CHUNK_LEN);
        if !fits {
            if self.chunks.len() >= POOL_MAX_CHUNKS {
                return Err(ApplyError::NamePoolFull);
            }
            self.chunks.push(Arc::new(PoolChunk::new()));
        }
        let index = self.chunks.len() - 1;
        let chunk = Arc::make_mut(self.chunks.get_mut(index).ok_or(ApplyError::NamePoolFull)?);
        let start = chunk.used;
        let end = start + need;
        let dest = chunk
            .data
            .get_mut(start..end)
            .ok_or(ApplyError::NamePoolFull)?;
        let (head, body) = dest.split_at_mut(header);
        if header == 1 {
            head[0] = len as u8;
        } else {
            head[0] = 0x80 | (len >> 8) as u8;
            head[1] = (len & 0xFF) as u8;
        }
        body.copy_from_slice(name);
        chunk.used = end;
        Ok(((index as u32) << POOL_BITS) | start as u32)
    }

    fn get(&self, offset: u32) -> &[u8] {
        let Some(chunk) = self.chunks.get((offset >> POOL_BITS) as usize) else {
            return &[];
        };
        let start = (offset & POOL_MASK) as usize;
        let Some(&first) = chunk.data.get(start) else {
            return &[];
        };
        let (header, len) = if first < 0x80 {
            (1, usize::from(first))
        } else {
            let second = chunk.data.get(start + 1).copied().unwrap_or(0);
            (2, (usize::from(first & 0x7F) << 8) | usize::from(second))
        };
        chunk
            .data
            .get(start + header..start + header + len)
            .unwrap_or(&[])
    }

    pub(crate) fn used_bytes(&self) -> usize {
        self.chunks.iter().map(|c| c.used).sum()
    }
}

/// A recorded delete mark: the bytes it moved into its state.
#[derive(Clone, Copy)]
pub(crate) struct Mark {
    pub state: DeleteState,
    pub moved: ByteCount,
    /// The marked subtree's totals were a lower bound (now, or when marked).
    pub lower_bound: bool,
    /// The marked subtree's allocated size includes an estimate.
    pub estimated: bool,
}

/// Facts about the scan as a whole, published with every snapshot.
#[derive(Clone, Copy, Default)]
pub(crate) struct Meta {
    pub stopped: bool,
    pub os_used: Option<u64>,
    pub rejected: u64,
    pub parked: u64,
}

/// Everything a reader needs. Cheap to clone: pointers to chunks and maps.
#[derive(Clone, Default)]
pub(crate) struct Store {
    chunks: Vec<Arc<NodeChunk>>,
    len: u32,
    pool: Pool,
    pub(crate) marks: Arc<FastMap<u32, Mark>>,
    pub(crate) delete_totals: Arc<FastMap<u32, DeleteTotals>>,
    pub(crate) failures: Arc<FastMap<u32, ReadFailure>>,
    pub(crate) link_origin: Arc<FastMap<u32, u32>>,
    /// Nodes the producer made up (see [`NodeFlags::SYNTHETIC`]).
    pub(crate) synthetic: Arc<FastMap<u32, ()>>,
    /// Hard-linked files that carry their bytes (the first name seen), so the
    /// free estimate can find them without looking at every node.
    pub(crate) linked: Arc<FastMap<u32, ()>>,
    /// For shared entries: the allocated bytes that are theirs alone.
    pub(crate) exclusive: Arc<FastMap<u32, u64>>,
    pub(crate) meta: Meta,
}

/// Values for a new node.
pub(crate) struct NewNode<'a> {
    pub parent: u32,
    pub kind: NodeKind,
    pub flags: NodeFlags,
    pub category: u8,
    pub logical: u64,
    pub allocated: u64,
    pub files: u32,
    pub mtime: i64,
    pub name: &'a [u8],
}

impl Store {
    pub(crate) fn len(&self) -> u32 {
        self.len
    }

    pub(crate) fn pool_used_bytes(&self) -> usize {
        self.pool.used_bytes()
    }

    fn slot(&self, id: u32) -> Option<(&NodeChunk, usize)> {
        if id >= self.len {
            return None;
        }
        let chunk = self.chunks.get((id >> CHUNK_BITS) as usize)?;
        Some((chunk, (id & CHUNK_MASK) as usize))
    }

    fn slot_mut(&mut self, id: u32) -> Option<(&mut NodeChunk, usize)> {
        if id >= self.len {
            return None;
        }
        let chunk = self.chunks.get_mut((id >> CHUNK_BITS) as usize)?;
        Some((Arc::make_mut(chunk), (id & CHUNK_MASK) as usize))
    }

    pub(crate) fn contains(&self, id: u32) -> bool {
        id < self.len
    }

    pub(crate) fn parent(&self, id: u32) -> Option<u32> {
        let (c, i) = self.slot(id)?;
        Some(c.parent[i & CHUNK_MASK as usize])
    }

    pub(crate) fn first_child(&self, id: u32) -> Option<u32> {
        let (c, i) = self.slot(id)?;
        Some(c.first_child[i & CHUNK_MASK as usize])
    }

    pub(crate) fn next_sibling(&self, id: u32) -> Option<u32> {
        let (c, i) = self.slot(id)?;
        Some(c.next_sibling[i & CHUNK_MASK as usize])
    }

    pub(crate) fn logical(&self, id: u32) -> u64 {
        self.slot(id)
            .map_or(0, |(c, i)| c.logical[i & CHUNK_MASK as usize])
    }

    pub(crate) fn allocated(&self, id: u32) -> u64 {
        self.slot(id)
            .map_or(0, |(c, i)| c.allocated[i & CHUNK_MASK as usize])
    }

    pub(crate) fn files(&self, id: u32) -> u32 {
        self.slot(id)
            .map_or(0, |(c, i)| c.files[i & CHUNK_MASK as usize])
    }

    pub(crate) fn mtime(&self, id: u32) -> i64 {
        self.slot(id)
            .map_or(0, |(c, i)| c.mtime[i & CHUNK_MASK as usize])
    }

    pub(crate) fn category(&self, id: u32) -> u8 {
        self.slot(id)
            .map_or(0, |(c, i)| c.category[i & CHUNK_MASK as usize])
    }

    pub(crate) fn name(&self, id: u32) -> &[u8] {
        match self.slot(id) {
            Some((c, i)) => self.pool.get(c.name[i & CHUNK_MASK as usize]),
            None => &[],
        }
    }

    fn bits(&self, id: u32) -> u16 {
        self.slot(id)
            .map_or(0, |(c, i)| c.bits[i & CHUNK_MASK as usize])
    }

    pub(crate) fn flags(&self, id: u32) -> NodeFlags {
        NodeFlags::from_bits_truncate(self.bits(id) & FLAG_BITS)
    }

    pub(crate) fn kind(&self, id: u32) -> NodeKind {
        NodeKind::from_bits((self.bits(id) >> KIND_SHIFT) as u8)
    }

    pub(crate) fn delete_state(&self, id: u32) -> DeleteState {
        DeleteState::from_bits(self.bits(id) >> DELETE_SHIFT)
    }

    pub(crate) fn set_flag(&mut self, id: u32, flag: NodeFlags) {
        if let Some((c, i)) = self.slot_mut(id) {
            c.bits[i & CHUNK_MASK as usize] |= flag.bits() & FLAG_BITS;
        }
    }

    pub(crate) fn set_delete_state(&mut self, id: u32, state: DeleteState) {
        if let Some((c, i)) = self.slot_mut(id) {
            let bits = &mut c.bits[i & CHUNK_MASK as usize];
            *bits = (*bits & !(3 << DELETE_SHIFT)) | ((state as u16) << DELETE_SHIFT);
        }
    }

    /// Appends a node and links it as the first child of its parent.
    pub(crate) fn push(
        &mut self,
        node: &NewNode<'_>,
        max_nodes: u32,
    ) -> Result<NodeId, ApplyError> {
        let id = self.len;
        if id >= max_nodes || id == NONE {
            return Err(ApplyError::NodeIdsExhausted);
        }
        let name = self.pool.intern(node.name)?;
        let chunk_index = (id >> CHUNK_BITS) as usize;
        if self.chunks.len() <= chunk_index {
            self.chunks.push(Arc::new(NodeChunk::new()));
        }
        self.len = id + 1;
        let next_sibling = match node.parent {
            NONE => NONE,
            parent => self.first_child(parent).unwrap_or(NONE),
        };
        if let Some((c, i)) = self.slot_mut(id) {
            let i = i & CHUNK_MASK as usize;
            c.parent[i] = node.parent;
            c.first_child[i] = NONE;
            c.next_sibling[i] = next_sibling;
            c.logical[i] = node.logical;
            c.allocated[i] = node.allocated;
            c.files[i] = node.files;
            c.mtime[i] = node.mtime;
            c.name[i] = name;
            c.bits[i] = (node.flags.bits() & FLAG_BITS) | ((node.kind as u16) << KIND_SHIFT);
            c.category[i] = node.category;
        }
        if node.parent != NONE
            && let Some((c, i)) = self.slot_mut(node.parent)
        {
            c.first_child[i & CHUNK_MASK as usize] = id;
        }
        Ok(NodeId(id))
    }

    /// Adds `delta` to the totals of `id` only.
    pub(crate) fn add_to(&mut self, id: u32, delta: ByteCount) {
        if let Some((c, i)) = self.slot_mut(id) {
            let i = i & CHUNK_MASK as usize;
            c.logical[i] = c.logical[i].saturating_add(delta.logical);
            c.allocated[i] = c.allocated[i].saturating_add(delta.allocated);
            c.files[i] = c.files[i].saturating_add(u32::try_from(delta.files).unwrap_or(u32::MAX));
        }
    }

    pub(crate) fn totals(&self, id: u32) -> ByteCount {
        ByteCount {
            logical: self.logical(id),
            allocated: self.allocated(id),
            files: u64::from(self.files(id)),
        }
    }

    pub(crate) fn delete_totals_of(&self, id: u32) -> DeleteTotals {
        self.delete_totals.get(&id).copied().unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pool_round_trips_short_and_long_names() {
        let mut pool = Pool::default();
        let long = vec![b'x'; 1000];
        let a = pool.intern(b"").unwrap();
        let b = pool.intern(b"hello").unwrap();
        let c = pool.intern(&long).unwrap();
        assert_eq!(pool.get(a), b"");
        assert_eq!(pool.get(b), b"hello");
        assert_eq!(pool.get(c), &long[..]);
        // Garbage offsets give empty names, never a panic.
        assert_eq!(pool.get(u32::MAX), b"");
        assert_eq!(pool.get(1 << POOL_BITS), b"");
    }

    #[test]
    fn pool_starts_new_chunk_instead_of_splitting_a_name() {
        let mut pool = Pool::default();
        let name = vec![b'n'; 900];
        let mut offsets = Vec::new();
        for _ in 0..200 {
            offsets.push(pool.intern(&name).unwrap());
        }
        assert!(pool.chunks.len() > 1);
        for off in offsets {
            assert_eq!(pool.get(off), &name[..]);
        }
    }

    #[test]
    fn node_column_bytes_are_47_per_node() {
        // Guards the memory budget: the columns of one chunk, per node.
        let per_node = std::mem::size_of::<NodeChunk>() / CHUNK_LEN;
        assert_eq!(per_node, 47);
    }
}
