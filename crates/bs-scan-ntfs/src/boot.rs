//! The NTFS boot sector: where the master file table is, and the sizes
//! everything else is measured in.

use crate::error::NtfsError;
use crate::le::{bytes_at, u8_at, u16_at, u64_at};

/// The sector size and sector count a boot sector claims, if it looks like an
/// NTFS boot sector at all. Used to size a raw volume before anything else is
/// known about it.
pub(crate) fn size_hint(sector: &[u8]) -> Option<(u32, u64)> {
    if bytes_at(sector, 3, 8)? != b"NTFS    " {
        return None;
    }
    let bytes_per_sector = u32::from(u16_at(sector, 0x0B)?);
    if !matches!(bytes_per_sector, 512 | 1024 | 2048 | 4096) {
        return None;
    }
    Some((bytes_per_sector, u64_at(sector, 0x28)?))
}

/// Largest cluster NTFS supports.
const MAX_CLUSTER_BYTES: u64 = 2 << 20;
/// Largest and smallest file record sizes accepted. Real volumes use 1024 or
/// 4096.
const MIN_RECORD_BYTES: u64 = 512;
const MAX_RECORD_BYTES: u64 = 64 * 1024;

/// What the boot sector says about the volume, checked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Geometry {
    pub bytes_per_sector: u32,
    pub cluster_bytes: u32,
    pub record_bytes: u32,
    pub total_sectors: u64,
    /// Clusters on the volume, from the sector count.
    pub clusters: u64,
    pub mft_lcn: u64,
    pub serial: u64,
}

/// A size field that is a cluster count when positive and a power of two
/// exponent when negative (the encoding of the record size).
fn sized(raw: u8, cluster_bytes: u64) -> Option<u64> {
    let signed = i8::from_le_bytes([raw]);
    if signed > 0 {
        cluster_bytes.checked_mul(u64::from(raw))
    } else {
        // Negative: 2 to the power of its absolute value.
        let exp = u32::from(signed.unsigned_abs());
        1u64.checked_shl(exp).filter(|_| exp < 32)
    }
}

pub(crate) fn parse(sector: &[u8], device_len: u64) -> Result<Geometry, NtfsError> {
    let too_short = || NtfsError::BadBootSector("boot sector too short");
    if bytes_at(sector, 3, 8).ok_or_else(too_short)? != b"NTFS    " {
        return Err(NtfsError::NotNtfs);
    }
    let bytes_per_sector = u32::from(u16_at(sector, 0x0B).ok_or_else(too_short)?);
    if !matches!(bytes_per_sector, 512 | 1024 | 2048 | 4096) {
        return Err(NtfsError::BadBootSector("sector size is not 512 to 4096"));
    }
    let spc = u8_at(sector, 0x0D).ok_or_else(too_short)?;
    // Up to 128 sectors per cluster as a plain count; larger clusters are
    // written as a negative power of two.
    let sectors_per_cluster: u64 = if spc <= 0x80 {
        if spc == 0 || !spc.is_power_of_two() {
            return Err(NtfsError::BadBootSector("sectors per cluster"));
        }
        u64::from(spc)
    } else {
        let exp = 256u32.saturating_sub(u32::from(spc));
        if exp > 12 {
            return Err(NtfsError::BadBootSector("sectors per cluster"));
        }
        1u64 << exp
    };
    let cluster_bytes = sectors_per_cluster
        .checked_mul(u64::from(bytes_per_sector))
        .filter(|&c| c <= MAX_CLUSTER_BYTES)
        .ok_or(NtfsError::BadBootSector("cluster size"))?;
    let total_sectors = u64_at(sector, 0x28).ok_or_else(too_short)?;
    if total_sectors == 0 {
        return Err(NtfsError::BadBootSector("no sectors"));
    }
    let volume_bytes = total_sectors
        .checked_mul(u64::from(bytes_per_sector))
        .ok_or(NtfsError::BadBootSector("volume size overflows"))?;
    if volume_bytes > device_len {
        return Err(NtfsError::BadBootSector(
            "the volume is larger than the device holding it",
        ));
    }
    let clusters = volume_bytes
        .checked_div(cluster_bytes)
        .ok_or(NtfsError::BadBootSector("cluster size"))?;
    let mft_lcn = u64_at(sector, 0x30).ok_or_else(too_short)?;
    let record_bytes = sized(u8_at(sector, 0x40).ok_or_else(too_short)?, cluster_bytes)
        .filter(|r| r.is_power_of_two() && (MIN_RECORD_BYTES..=MAX_RECORD_BYTES).contains(r))
        .ok_or(NtfsError::BadBootSector("file record size"))?;
    let serial = u64_at(sector, 0x48).ok_or_else(too_short)?;
    // The table must start inside the volume.
    let mft_at = mft_lcn
        .checked_mul(cluster_bytes)
        .and_then(|b| b.checked_add(record_bytes))
        .filter(|&end| end <= volume_bytes)
        .ok_or(NtfsError::BadBootSector(
            "the master file table starts outside the volume",
        ))?;
    let _ = mft_at;
    Ok(Geometry {
        bytes_per_sector,
        cluster_bytes: u32::try_from(cluster_bytes)
            .map_err(|_| NtfsError::BadBootSector("cluster size"))?,
        record_bytes: u32::try_from(record_bytes)
            .map_err(|_| NtfsError::BadBootSector("file record size"))?,
        total_sectors,
        clusters,
        mft_lcn,
        serial,
    })
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

    fn sample_sector(bytes_per_sector: u16, spc: u8, total: u64, mft_lcn: u64) -> Vec<u8> {
        let mut s = vec![0u8; 512];
        s[3..11].copy_from_slice(b"NTFS    ");
        s[0x0B..0x0D].copy_from_slice(&bytes_per_sector.to_le_bytes());
        s[0x0D] = spc;
        s[0x28..0x30].copy_from_slice(&total.to_le_bytes());
        s[0x30..0x38].copy_from_slice(&mft_lcn.to_le_bytes());
        s[0x40] = 0xF6; // 2^10 = 1024-byte records
        s[0x48..0x50].copy_from_slice(&0x1122_3344_5566_7788u64.to_le_bytes());
        s[510] = 0x55;
        s[511] = 0xAA;
        s
    }

    #[test]
    fn a_plain_boot_sector_parses() {
        let s = sample_sector(512, 8, 100_000, 4);
        let g = parse(&s, 100_000 * 512 + 512).unwrap();
        assert_eq!(g.cluster_bytes, 4096);
        assert_eq!(g.record_bytes, 1024);
        assert_eq!(g.clusters, 100_000 / 8);
        assert_eq!(g.mft_lcn, 4);
        assert_eq!(g.serial, 0x1122_3344_5566_7788);
        assert_eq!(size_hint(&s), Some((512, 100_000)));
    }

    #[test]
    fn four_k_records_by_cluster_count_and_by_exponent() {
        let mut s = sample_sector(4096, 1, 1000, 4);
        s[0x40] = 1; // one cluster of 4096
        assert_eq!(parse(&s, 1 << 30).unwrap().record_bytes, 4096);
        s[0x40] = 0xF4; // 2^12
        assert_eq!(parse(&s, 1 << 30).unwrap().record_bytes, 4096);
    }

    #[test]
    fn large_clusters_use_the_negative_encoding() {
        let mut s = sample_sector(512, 0xF4, 1 << 20, 4); // 2^12 sectors = 2 MiB
        assert_eq!(parse(&s, 1 << 40).unwrap().cluster_bytes, 2 << 20);
        s[0x0D] = 0xF3; // 4 MiB: beyond what NTFS supports
        assert!(parse(&s, 1 << 40).is_err());
    }

    #[test]
    fn nonsense_is_refused_not_trusted() {
        let good = sample_sector(512, 8, 100_000, 4);
        let dev = 100_000 * 512 + 512;
        let mut bad = good.clone();
        bad[3] = b'X';
        assert!(matches!(parse(&bad, dev), Err(NtfsError::NotNtfs)));
        assert!(size_hint(&bad).is_none());
        for (at, value) in [
            (0x0Cusize, 0u8),
            (0x0D, 0),
            (0x0D, 3),
            (0x40, 0),
            (0x40, 0xE0),
            (0x40, 0xFF),
        ] {
            let mut b = good.clone();
            b[at] = value;
            assert!(parse(&b, dev).is_err(), "{at:#x}={value}");
        }
        // More sectors than the device holds.
        assert!(parse(&good, dev - 512 * 100_000).is_err());
        // The table outside the volume.
        let mut b = good.clone();
        b[0x30..0x38].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(parse(&b, dev).is_err());
        // A sector count that overflows when multiplied.
        let mut b = good;
        b[0x28..0x30].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(parse(&b, u64::MAX).is_err());
        assert!(parse(&b[..100], dev).is_err());
        assert!(size_hint(&[]).is_none());
    }
}
