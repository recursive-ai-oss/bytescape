//! Tree search: the key, the item, the trait that anything able to search a
//! tree implements (the kernel through an ioctl, or a test fixture), and the
//! checked reading of a result buffer.
//!
//! A search result is a list of `btrfs_ioctl_search_header` records
//! (`transid`, `objectid`, `offset`, `type`, `len`), each followed by `len`
//! bytes of item. The kernel writes it, but the reader treats it as hostile
//! all the same: a header that does not fit, an item longer than the buffer,
//! a count the buffer cannot hold, or keys that do not strictly increase end
//! the search with an error instead of a panic, an overread or a search that
//! never ends.

use crate::BtrfsError;
use crate::le::{to_usize, u32_at, u64_at};
use crate::mounts::MountPoint;

/// Bytes of one search header.
pub(crate) const HEADER_LEN: usize = 32;

/// A key in a btrfs tree. The derived order is the tree's order: object id,
/// then type, then offset.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Key {
    /// The object the item belongs to (an inode number, a tree id, ...).
    pub objectid: u64,
    /// The item type.
    pub ty: u8,
    /// A type-specific number (a file offset, a directory index, ...).
    pub offset: u64,
}

impl Key {
    /// The smallest key.
    pub const MIN: Key = Key {
        objectid: 0,
        ty: 0,
        offset: 0,
    };
    /// The largest key.
    pub const MAX: Key = Key {
        objectid: u64::MAX,
        ty: u8::MAX,
        offset: u64::MAX,
    };

    /// The key right after this one, or `None` for the largest.
    pub fn successor(self) -> Option<Key> {
        if let Some(offset) = self.offset.checked_add(1) {
            return Some(Key { offset, ..self });
        }
        if let Some(ty) = self.ty.checked_add(1) {
            return Some(Key {
                ty,
                offset: 0,
                ..self
            });
        }
        Some(Key {
            objectid: self.objectid.checked_add(1)?,
            ty: 0,
            offset: 0,
        })
    }
}

/// One item of a tree: its key and its bytes, borrowed from the search
/// result.
#[derive(Clone, Copy, Debug)]
pub struct Item<'a> {
    /// The item's key.
    pub key: Key,
    /// The item's bytes, as stored.
    pub data: &'a [u8],
}

/// What a search callback tells the search to do next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flow {
    /// Go on with the next item.
    Continue,
    /// Stop; the search returns `Ok`.
    Stop,
}

/// Anything that can search the trees of one btrfs file system.
pub trait TreeSource: Send + Sync {
    /// Calls `each` for every item of `tree` whose key lies in `from..=to`,
    /// in ascending key order, each key once.
    ///
    /// The items borrow the source's buffers and are valid only during the
    /// call. A source that cannot deliver the whole range returns an error;
    /// the callback may already have seen part of it.
    fn search(
        &self,
        tree: u64,
        from: Key,
        to: Key,
        each: &mut dyn FnMut(Item<'_>) -> Flow,
    ) -> Result<(), BtrfsError>;

    /// The mounts below the start directory, in any order. A mount hides the
    /// folder it covers, and the files the walker sees there are the
    /// mount's, so the reader needs to know where they are. A source that has
    /// no mounts to tell of (a tree held in memory) has none.
    ///
    /// An error here means the mounts are not known, which the reader treats
    /// as no tree: a guess of "none" would show covered folders as the
    /// mounts' contents.
    fn mounts(&self) -> Result<Vec<MountPoint>, BtrfsError> {
        Ok(Vec::new())
    }
}

/// How reading one result buffer ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PageEnd {
    /// The buffer held no item: the range is exhausted.
    Empty,
    /// The key of the last item read; the search goes on after it.
    Last(Key),
    /// The callback asked to stop.
    Stopped,
}

/// Reads the `nr_items` items of a result buffer. Every key must lie in
/// `floor..=ceil` and be larger than the one before it.
pub(crate) fn read_page(
    buf: &[u8],
    nr_items: u32,
    floor: Key,
    ceil: Key,
    each: &mut dyn FnMut(Item<'_>) -> Flow,
) -> Result<PageEnd, BtrfsError> {
    let count = to_usize(u64::from(nr_items)).ok_or(BtrfsError::Hostile("item count"))?;
    // Every item takes at least its header, so a count the buffer cannot hold
    // is a lie. Checked before anything is done on its strength.
    if count > buf.len() / HEADER_LEN {
        return Err(BtrfsError::Hostile("more items than the buffer can hold"));
    }
    let mut pos = 0usize;
    let mut last: Option<Key> = None;
    for _ in 0..count {
        let header = buf
            .get(pos..)
            .filter(|rest| rest.len() >= HEADER_LEN)
            .ok_or(BtrfsError::Hostile("a header runs past the buffer"))?;
        let objectid = u64_at(header, 8).ok_or(BtrfsError::Hostile("header"))?;
        let offset = u64_at(header, 16).ok_or(BtrfsError::Hostile("header"))?;
        let ty = u32_at(header, 24).ok_or(BtrfsError::Hostile("header"))?;
        let len = u32_at(header, 28).ok_or(BtrfsError::Hostile("header"))?;
        let ty = u8::try_from(ty).map_err(|_| BtrfsError::Hostile("item type"))?;
        let len = to_usize(u64::from(len)).ok_or(BtrfsError::Hostile("item length"))?;
        let start = pos
            .checked_add(HEADER_LEN)
            .ok_or(BtrfsError::Hostile("position"))?;
        let end = start
            .checked_add(len)
            .ok_or(BtrfsError::Hostile("item length"))?;
        let data = buf
            .get(start..end)
            .ok_or(BtrfsError::Hostile("an item runs past the buffer"))?;
        let key = Key {
            objectid,
            ty,
            offset,
        };
        if key < floor || key > ceil {
            return Err(BtrfsError::Hostile("an item outside the range asked for"));
        }
        if last.is_some_and(|l| key <= l) {
            return Err(BtrfsError::Hostile("keys do not increase"));
        }
        last = Some(key);
        if each(Item { key, data }) == Flow::Stop {
            return Ok(PageEnd::Stopped);
        }
        pos = end;
    }
    Ok(last.map_or(PageEnd::Empty, PageEnd::Last))
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

    /// A result buffer holding `items`.
    pub(crate) fn buffer(items: &[(Key, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        for (key, data) in items {
            out.extend_from_slice(&7u64.to_le_bytes());
            out.extend_from_slice(&key.objectid.to_le_bytes());
            out.extend_from_slice(&key.offset.to_le_bytes());
            out.extend_from_slice(&u32::from(key.ty).to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(data);
        }
        out
    }

    fn k(objectid: u64, ty: u8, offset: u64) -> Key {
        Key {
            objectid,
            ty,
            offset,
        }
    }

    fn collect(
        buf: &[u8],
        n: u32,
        floor: Key,
        ceil: Key,
    ) -> Result<(PageEnd, Vec<Key>), BtrfsError> {
        let mut keys = Vec::new();
        let end = read_page(buf, n, floor, ceil, &mut |item| {
            keys.push(item.key);
            Flow::Continue
        })?;
        Ok((end, keys))
    }

    #[test]
    fn the_key_after_a_key_carries_into_the_next_field() {
        assert_eq!(k(1, 2, 3).successor(), Some(k(1, 2, 4)));
        assert_eq!(k(1, 2, u64::MAX).successor(), Some(k(1, 3, 0)));
        assert_eq!(k(1, u8::MAX, u64::MAX).successor(), Some(k(2, 0, 0)));
        assert_eq!(Key::MAX.successor(), None);
        assert!(k(1, 255, 0) < k(2, 0, 0));
        assert!(k(1, 2, 9) < k(1, 3, 0));
    }

    #[test]
    fn a_good_page_is_read_in_order() {
        let items = [
            (k(256, 1, 0), &[1u8, 2, 3][..]),
            (k(256, 96, 2), &[][..]),
            (k(257, 1, 0), &[9u8; 100][..]),
        ];
        let buf = buffer(&items);
        let (end, keys) = collect(&buf, 3, Key::MIN, Key::MAX).unwrap();
        assert_eq!(end, PageEnd::Last(k(257, 1, 0)));
        assert_eq!(keys, vec![k(256, 1, 0), k(256, 96, 2), k(257, 1, 0)]);
        // Nothing in the buffer, nothing asked for.
        assert_eq!(
            collect(&[], 0, Key::MIN, Key::MAX).unwrap().0,
            PageEnd::Empty
        );
    }

    #[test]
    fn the_callback_can_stop_the_search() {
        let buf = buffer(&[(k(1, 1, 0), &[][..]), (k(2, 1, 0), &[][..])]);
        let mut seen = 0;
        let end = read_page(&buf, 2, Key::MIN, Key::MAX, &mut |_| {
            seen += 1;
            Flow::Stop
        })
        .unwrap();
        assert_eq!((end, seen), (PageEnd::Stopped, 1));
    }

    #[test]
    fn every_kind_of_nonsense_is_an_error() {
        let good = buffer(&[(k(5, 1, 0), &[1u8; 10][..]), (k(6, 1, 0), &[2u8; 10][..])]);
        // A count larger than the buffer can hold, however it is made up.
        assert!(collect(&good, 3, Key::MIN, Key::MAX).is_err());
        assert!(collect(&good, u32::MAX, Key::MIN, Key::MAX).is_err());
        // Every cut of the buffer: either the items before the cut come back
        // and the count is then wrong, or it is an error. Never a panic.
        for cut in 0..good.len() {
            assert!(
                collect(&good[..cut], 2, Key::MIN, Key::MAX).is_err(),
                "cut {cut}"
            );
        }
        // An item longer than what remains.
        let mut long = good.clone();
        long[28..32].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(collect(&long, 2, Key::MIN, Key::MAX).is_err());
        // A type that is not a byte.
        let mut wide = good.clone();
        wide[24..28].copy_from_slice(&256u32.to_le_bytes());
        assert!(collect(&wide, 2, Key::MIN, Key::MAX).is_err());
        // Keys that repeat or go backwards.
        let twice = buffer(&[(k(5, 1, 0), &[][..]), (k(5, 1, 0), &[][..])]);
        assert!(collect(&twice, 2, Key::MIN, Key::MAX).is_err());
        let back = buffer(&[(k(6, 1, 0), &[][..]), (k(5, 1, 0), &[][..])]);
        assert!(collect(&back, 2, Key::MIN, Key::MAX).is_err());
        // Keys outside the range asked for (this is what stops a search
        // from going round in circles on a page that does not advance).
        assert!(collect(&good, 2, k(5, 1, 1), Key::MAX).is_err());
        assert!(collect(&good, 2, Key::MIN, k(5, 1, 0)).is_err());
    }
}
