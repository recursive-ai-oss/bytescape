//! Bounds-checked little-endian field access. Every function returns `None`
//! instead of reading outside the slice, so parsing code cannot be made to
//! panic by a short or hostile buffer.

pub(crate) fn bytes_at(buf: &[u8], offset: usize, len: usize) -> Option<&[u8]> {
    let end = offset.checked_add(len)?;
    buf.get(offset..end)
}

pub(crate) fn u8_at(buf: &[u8], offset: usize) -> Option<u8> {
    buf.get(offset).copied()
}

pub(crate) fn u16_at(buf: &[u8], offset: usize) -> Option<u16> {
    let raw: [u8; 2] = bytes_at(buf, offset, 2)?.try_into().ok()?;
    Some(u16::from_le_bytes(raw))
}

pub(crate) fn u32_at(buf: &[u8], offset: usize) -> Option<u32> {
    let raw: [u8; 4] = bytes_at(buf, offset, 4)?.try_into().ok()?;
    Some(u32::from_le_bytes(raw))
}

pub(crate) fn u64_at(buf: &[u8], offset: usize) -> Option<u64> {
    let raw: [u8; 8] = bytes_at(buf, offset, 8)?.try_into().ok()?;
    Some(u64::from_le_bytes(raw))
}

pub(crate) fn i64_at(buf: &[u8], offset: usize) -> Option<i64> {
    let raw: [u8; 8] = bytes_at(buf, offset, 8)?.try_into().ok()?;
    Some(i64::from_le_bytes(raw))
}

/// `usize` from a disk value. `None` only where `usize` is narrower than the
/// value.
pub(crate) fn to_usize(value: u64) -> Option<usize> {
    usize::try_from(value).ok()
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

    #[test]
    fn short_buffers_give_none() {
        let b = [1u8, 2, 3, 4, 5, 6, 7, 8, 9];
        assert_eq!(u16_at(&b, 0), Some(0x0201));
        assert_eq!(u32_at(&b, 1), Some(0x0504_0302));
        assert_eq!(u64_at(&b, 1), Some(0x0908_0706_0504_0302));
        assert_eq!(u64_at(&b, 2), None);
        assert_eq!(u32_at(&b, 6), None);
        assert_eq!(u16_at(&b, 8), None);
        assert_eq!(u8_at(&b, 9), None);
        assert_eq!(u32_at(&b, usize::MAX), None);
        assert_eq!(bytes_at(&b, usize::MAX, 2), None);
        assert_eq!(bytes_at(&b, 1, usize::MAX), None);
        assert_eq!(i64_at(&b, 0), Some(0x0807_0605_0403_0201));
    }
}
