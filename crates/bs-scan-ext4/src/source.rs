//! Where the bytes come from: a positioned-read source, so the parser runs
//! the same on a block device, an image file or a buffer in memory.

use std::fs::File;
use std::io::{self, Seek, SeekFrom};
use std::os::unix::fs::FileExt;
use std::path::Path;

/// Something that can be read at byte offsets.
///
/// Reads are exact: a read that cannot be satisfied in full is an error.
/// Implementations must never write.
pub trait ByteSource: Send + Sync {
    /// Size in bytes.
    fn len(&self) -> u64;

    /// Whether the source holds no bytes.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Fills `buf` from `offset`, or fails.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()>;
}

fn out_of_range() -> io::Error {
    io::Error::new(
        io::ErrorKind::UnexpectedEof,
        "read past the end of the device",
    )
}

/// A file or block device opened read-only.
///
/// Reads go through the page cache, never around it. That matters on a
/// mounted file system: its metadata lives in the device's cache and may be
/// newer than what has reached the disk.
#[derive(Debug)]
pub struct FileSource {
    file: File,
    len: u64,
}

impl FileSource {
    /// Opens `path` for reading only. A block device reports no length in its
    /// metadata, so the length is found by seeking to the end.
    pub fn open(path: &Path) -> io::Result<FileSource> {
        let mut file = File::open(path)?;
        let len = file.seek(SeekFrom::End(0))?;
        Ok(FileSource { file, len })
    }

    /// The open file, for asking the system what it is (a block device and
    /// which one) without opening the name a second time.
    #[must_use]
    pub fn file(&self) -> &File {
        &self.file
    }
}

impl ByteSource for FileSource {
    fn len(&self) -> u64 {
        self.len
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let wanted = u64::try_from(buf.len()).map_err(|_| out_of_range())?;
        let end = offset.checked_add(wanted).ok_or_else(out_of_range)?;
        if end > self.len {
            return Err(out_of_range());
        }
        self.file.read_exact_at(buf, offset)
    }
}

impl ByteSource for [u8] {
    fn len(&self) -> u64 {
        u64::try_from(<[u8]>::len(self)).unwrap_or(u64::MAX)
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let start = usize::try_from(offset).map_err(|_| out_of_range())?;
        let end = start.checked_add(buf.len()).ok_or_else(out_of_range)?;
        let src = self.get(start..end).ok_or_else(out_of_range)?;
        buf.copy_from_slice(src);
        Ok(())
    }
}

impl ByteSource for Vec<u8> {
    fn len(&self) -> u64 {
        self.as_slice().len() as u64
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        self.as_slice().read_at(offset, buf)
    }
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
    fn memory_source_reads_are_exact() {
        let data: Vec<u8> = (0u8..=9).collect();
        let mut b = [0u8; 3];
        data.read_at(2, &mut b).unwrap();
        assert_eq!(b, [2, 3, 4]);
        assert!(data.read_at(8, &mut b).is_err());
        assert!(data.read_at(u64::MAX, &mut b).is_err());
        assert!(data.read_at(10, &mut []).is_ok());
        assert!(data.read_at(11, &mut []).is_err());
    }
}
