//! Reading and writing the primitive values of a frame body. Every read is
//! checked against the data that is really there, and nothing is allocated on
//! the strength of a length field alone.

use crate::DecodeError;

/// A reader over one frame body.
pub(crate) struct Reader<'a> {
    data: &'a [u8],
}

impl<'a> Reader<'a> {
    pub(crate) fn new(data: &'a [u8]) -> Self {
        Self { data }
    }

    pub(crate) fn remaining(&self) -> usize {
        self.data.len()
    }

    pub(crate) fn take(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        if n > self.data.len() {
            return Err(DecodeError::Truncated);
        }
        let (head, tail) = self.data.split_at(n);
        self.data = tail;
        Ok(head)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], DecodeError> {
        self.take(N)?.try_into().map_err(|_| DecodeError::Truncated)
    }

    pub(crate) fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(u8::from_le_bytes(self.array()?))
    }

    pub(crate) fn u16(&mut self) -> Result<u16, DecodeError> {
        Ok(u16::from_le_bytes(self.array()?))
    }

    pub(crate) fn u32(&mut self) -> Result<u32, DecodeError> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    pub(crate) fn i32(&mut self) -> Result<i32, DecodeError> {
        Ok(i32::from_le_bytes(self.array()?))
    }

    pub(crate) fn u64(&mut self) -> Result<u64, DecodeError> {
        Ok(u64::from_le_bytes(self.array()?))
    }

    pub(crate) fn u128(&mut self) -> Result<u128, DecodeError> {
        Ok(u128::from_le_bytes(self.array()?))
    }

    pub(crate) fn bool(&mut self) -> Result<bool, DecodeError> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(DecodeError::BadValue),
        }
    }

    pub(crate) fn nonce(&mut self) -> Result<[u8; crate::NONCE_LEN], DecodeError> {
        self.array()
    }

    /// A byte string with a 32-bit length, at most `max` long.
    pub(crate) fn bytes(&mut self, max: usize) -> Result<&'a [u8], DecodeError> {
        let len = usize::try_from(self.u32()?).map_err(|_| DecodeError::TooLong)?;
        if len > max {
            return Err(DecodeError::TooLong);
        }
        self.take(len)
    }

    /// A count of items that each take at least `min_item` bytes. The count
    /// is checked against the data left, so a short frame cannot claim a
    /// million items, and against `max`.
    pub(crate) fn count(&mut self, min_item: usize, max: usize) -> Result<usize, DecodeError> {
        let count = usize::try_from(self.u32()?).map_err(|_| DecodeError::TooLong)?;
        if count > max {
            return Err(DecodeError::TooLong);
        }
        let needed = count.checked_mul(min_item).ok_or(DecodeError::TooLong)?;
        if needed > self.data.len() {
            return Err(DecodeError::Truncated);
        }
        Ok(count)
    }

    pub(crate) fn finish(self) -> Result<(), DecodeError> {
        if self.data.is_empty() {
            Ok(())
        } else {
            Err(DecodeError::TrailingBytes)
        }
    }
}

/// A growing frame body.
pub(crate) struct Writer<'a> {
    out: &'a mut Vec<u8>,
}

impl<'a> Writer<'a> {
    pub(crate) fn new(out: &'a mut Vec<u8>) -> Self {
        Self { out }
    }

    pub(crate) fn vec(&mut self) -> &mut Vec<u8> {
        self.out
    }

    pub(crate) fn u8(&mut self, v: u8) {
        self.out.push(v);
    }

    pub(crate) fn u16(&mut self, v: u16) {
        self.out.extend_from_slice(&v.to_le_bytes());
    }

    pub(crate) fn u32(&mut self, v: u32) {
        self.out.extend_from_slice(&v.to_le_bytes());
    }

    pub(crate) fn i32(&mut self, v: i32) {
        self.out.extend_from_slice(&v.to_le_bytes());
    }

    pub(crate) fn u64(&mut self, v: u64) {
        self.out.extend_from_slice(&v.to_le_bytes());
    }

    pub(crate) fn u128(&mut self, v: u128) {
        self.out.extend_from_slice(&v.to_le_bytes());
    }

    pub(crate) fn bool(&mut self, v: bool) {
        self.out.push(u8::from(v));
    }

    pub(crate) fn raw(&mut self, bytes: &[u8]) {
        self.out.extend_from_slice(bytes);
    }

    /// A byte string with a 32-bit length. The caller has bounded it.
    pub(crate) fn bytes(&mut self, bytes: &[u8]) -> Result<(), crate::EncodeError> {
        let len = u32::try_from(bytes.len()).map_err(|_| crate::EncodeError::TooLarge)?;
        self.u32(len);
        self.raw(bytes);
        Ok(())
    }

    pub(crate) fn count(&mut self, n: usize) -> Result<(), crate::EncodeError> {
        self.u32(u32::try_from(n).map_err(|_| crate::EncodeError::TooLarge)?);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn a_count_that_the_data_cannot_hold_is_refused_before_anything_is_reserved() {
        let mut data = 8192u32.to_le_bytes().to_vec();
        data.extend_from_slice(&[0; 40]);
        assert_eq!(
            Reader::new(&data).count(6, 10_000),
            Err(DecodeError::Truncated)
        );
        // The same count with room for every item is accepted.
        let mut full = 3u32.to_le_bytes().to_vec();
        full.extend_from_slice(&[0; 18]);
        assert_eq!(Reader::new(&full).count(6, 10_000), Ok(3));
        // And a count over the limit is refused whatever the data.
        assert_eq!(Reader::new(&full).count(6, 2), Err(DecodeError::TooLong));
        // An item size that would overflow is refused, not wrapped.
        let huge = u32::MAX.to_le_bytes();
        assert!(Reader::new(&huge).count(usize::MAX, usize::MAX).is_err());
    }

    #[test]
    fn strings_longer_than_their_limit_are_refused_without_being_read() {
        let mut data = 100u32.to_le_bytes().to_vec();
        data.extend_from_slice(&[b'a'; 100]);
        assert_eq!(Reader::new(&data).bytes(99), Err(DecodeError::TooLong));
        assert_eq!(Reader::new(&data).bytes(100).unwrap().len(), 100);
        let short = 100u32.to_le_bytes();
        assert_eq!(Reader::new(&short).bytes(200), Err(DecodeError::Truncated));
    }
}
