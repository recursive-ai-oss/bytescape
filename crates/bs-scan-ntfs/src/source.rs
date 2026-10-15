//! Where the bytes come from: a positioned-read byte source, and the thin
//! shim that opens a volume or an image file read-only.
//!
//! The parsers only see [`ByteSource`], so the whole reader builds, runs and
//! is tested on any system against image files; only opening `\\.\C:` is
//! Windows-specific, and that is done with the standard library alone.

use std::fs::File;
use std::io;
use std::path::Path;

/// Positioned reads from a device or an image. Reads are exact: a read that
/// cannot be filled in full is an error. Implementations must never write.
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
        "read past the end of the volume",
    )
}

/// A volume or image file opened for reading only.
///
/// A raw volume handle on Windows only accepts reads whose offset and length
/// are multiples of the sector size, so reads are widened to that unit and
/// the wanted part copied out; an image file needs no widening.
#[derive(Debug)]
pub struct FileSource {
    file: File,
    len: u64,
    /// Reads are widened to a multiple of this (1 = no widening).
    unit: u64,
}

impl FileSource {
    /// Opens the image file `path`.
    pub fn open(path: &Path) -> io::Result<FileSource> {
        let file = File::open(path)?;
        let len = file.metadata()?.len();
        Ok(FileSource { file, len, unit: 1 })
    }

    /// Opens a raw volume such as `\\.\C:`. The length is not available from
    /// the handle, so it is taken from the boot sector.
    pub fn open_volume(path: &Path) -> io::Result<FileSource> {
        let file = File::open(path)?;
        let mut probe = FileSource {
            file,
            len: u64::MAX,
            unit: 4096,
        };
        let mut sector = [0u8; 512];
        // Four-kilobyte-sector devices refuse a 512-byte read at their start;
        // the widening below turns it into a 4096-byte one.
        probe.read_at(0, &mut sector)?;
        let (bytes_per_sector, total_sectors) = crate::boot::size_hint(&sector)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "not an NTFS volume"))?;
        let bytes = total_sectors
            .checked_mul(u64::from(bytes_per_sector))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "volume size overflows"))?;
        // The backup boot sector is the sector after the last counted one.
        probe.len = bytes.saturating_add(u64::from(bytes_per_sector));
        probe.unit = u64::from(bytes_per_sector);
        Ok(probe)
    }

    #[cfg(unix)]
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        use std::os::unix::fs::FileExt;
        self.file.read_exact_at(buf, offset)
    }

    #[cfg(windows)]
    fn read_exact_at(&self, mut buf: &mut [u8], mut offset: u64) -> io::Result<()> {
        use std::os::windows::fs::FileExt;
        while !buf.is_empty() {
            let n = self.file.seek_read(buf, offset)?;
            if n == 0 {
                return Err(out_of_range());
            }
            let rest = buf.get_mut(n..).ok_or_else(out_of_range)?;
            buf = rest;
            offset = offset.checked_add(n as u64).ok_or_else(out_of_range)?;
        }
        Ok(())
    }

    #[cfg(not(any(unix, windows)))]
    fn read_exact_at(&self, _buf: &mut [u8], _offset: u64) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "no positioned reads on this platform",
        ))
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
        if self.unit <= 1 || (offset.is_multiple_of(self.unit) && wanted.is_multiple_of(self.unit))
        {
            return self.read_exact_at(buf, offset);
        }
        let start = offset
            .checked_div(self.unit)
            .and_then(|q| q.checked_mul(self.unit))
            .ok_or_else(out_of_range)?;
        let stop = end
            .checked_next_multiple_of(self.unit)
            .ok_or_else(out_of_range)?;
        let span = stop
            .checked_sub(start)
            .and_then(|n| usize::try_from(n).ok())
            .ok_or_else(out_of_range)?;
        let mut wide = vec![0u8; span];
        self.read_exact_at(&mut wide, start)?;
        let from = offset
            .checked_sub(start)
            .and_then(|n| usize::try_from(n).ok())
            .ok_or_else(out_of_range)?;
        let part = wide
            .get(from..from.checked_add(buf.len()).ok_or_else(out_of_range)?)
            .ok_or_else(out_of_range)?;
        buf.copy_from_slice(part);
        Ok(())
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

    #[test]
    fn widened_reads_return_the_wanted_bytes() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("source-{}.bin", std::process::id()));
        let data: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, &data).unwrap();
        let mut src = FileSource::open(&path).unwrap();
        src.unit = 512;
        for (offset, len) in [
            (0usize, 512usize),
            (3, 7),
            (510, 5),
            (1000, 3000),
            (4090, 6),
        ] {
            let mut buf = vec![0u8; len];
            src.read_at(offset as u64, &mut buf).unwrap();
            assert_eq!(buf, data[offset..offset + len], "{offset}+{len}");
        }
        let mut buf = [0u8; 8];
        assert!(src.read_at(4090, &mut buf).is_err());
        std::fs::remove_file(&path).ok();
    }
}
