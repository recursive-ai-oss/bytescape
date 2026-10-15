//! Run lists ("mapping pairs"): how a non-resident attribute says which
//! clusters hold its data. Decoding is checked at every step: a length that
//! overflows, a run that leaves the volume, a list that ends mid-pair, all
//! become errors, and the number of runs is bounded by the bytes of the list.

/// One run of a stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Run {
    /// First virtual cluster of the run.
    pub vcn: u64,
    /// Length in clusters.
    pub len: u64,
    /// First logical cluster on the volume, or `None` for a hole (sparse).
    pub lcn: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RunlistError {
    /// The list ends inside a pair.
    Truncated,
    /// A length or offset field has an impossible size.
    BadSizes,
    /// A run has no length.
    EmptyRun,
    /// A cluster number below zero or beyond the volume.
    OutOfVolume,
    /// Counting overflowed.
    Overflow,
}

/// Sums and shape of a decoded list.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Totals {
    /// Clusters in runs that are backed by the volume.
    pub allocated: u64,
    /// Clusters in holes.
    pub holes: u64,
    /// Number of runs.
    pub runs: u64,
}

impl Totals {
    /// Clusters the list covers, holes included.
    pub fn covered(&self) -> Option<u64> {
        self.allocated.checked_add(self.holes)
    }
}

fn read_unsigned(bytes: &[u8]) -> u64 {
    let mut b = [0u8; 8];
    for (d, s) in b.iter_mut().zip(bytes) {
        *d = *s;
    }
    u64::from_le_bytes(b)
}

/// A little-endian two's complement number of up to eight bytes.
fn read_signed(bytes: &[u8]) -> Option<i64> {
    let last = *bytes.last()?;
    let fill = if last & 0x80 != 0 { 0xFF } else { 0 };
    let mut b = [fill; 8];
    for (d, s) in b.iter_mut().zip(bytes) {
        *d = *s;
    }
    Some(i64::from_le_bytes(b))
}

/// Decodes `runs` (the mapping pairs, up to their zero terminator or the end
/// of the slice), starting at virtual cluster `first_vcn`, and calls `each`
/// for every run. `volume_clusters` bounds where a run may lie.
///
/// A list may end without a terminator when it fills the attribute.
pub(crate) fn decode(
    runs: &[u8],
    first_vcn: u64,
    volume_clusters: u64,
    mut each: impl FnMut(Run),
) -> Result<Totals, RunlistError> {
    let mut totals = Totals::default();
    let mut pos = 0usize;
    let mut vcn = first_vcn;
    let mut lcn: i64 = 0;
    loop {
        let Some(&header) = runs.get(pos) else {
            return Ok(totals);
        };
        if header == 0 {
            return Ok(totals);
        }
        pos = pos.checked_add(1).ok_or(RunlistError::Overflow)?;
        let len_size = usize::from(header & 0x0F);
        let off_size = usize::from(header >> 4);
        if len_size == 0 || len_size > 8 || off_size > 8 {
            return Err(RunlistError::BadSizes);
        }
        let len_end = pos.checked_add(len_size).ok_or(RunlistError::Overflow)?;
        let len_bytes = runs.get(pos..len_end).ok_or(RunlistError::Truncated)?;
        let len = read_unsigned(len_bytes);
        // A length whose top bit is set would be a negative number of
        // clusters; no volume has that many.
        if len == 0 {
            return Err(RunlistError::EmptyRun);
        }
        if len > u64::MAX >> 1 {
            return Err(RunlistError::OutOfVolume);
        }
        pos = len_end;
        let this_lcn = if off_size == 0 {
            None
        } else {
            let off_end = pos.checked_add(off_size).ok_or(RunlistError::Overflow)?;
            let off_bytes = runs.get(pos..off_end).ok_or(RunlistError::Truncated)?;
            let delta = read_signed(off_bytes).ok_or(RunlistError::BadSizes)?;
            pos = off_end;
            lcn = lcn.checked_add(delta).ok_or(RunlistError::OutOfVolume)?;
            let start = u64::try_from(lcn).map_err(|_| RunlistError::OutOfVolume)?;
            let end = start.checked_add(len).ok_or(RunlistError::OutOfVolume)?;
            if end > volume_clusters {
                return Err(RunlistError::OutOfVolume);
            }
            Some(start)
        };
        each(Run {
            vcn,
            len,
            lcn: this_lcn,
        });
        vcn = vcn.checked_add(len).ok_or(RunlistError::Overflow)?;
        totals.runs = totals.runs.saturating_add(1);
        let slot = if this_lcn.is_some() {
            &mut totals.allocated
        } else {
            &mut totals.holes
        };
        *slot = slot.checked_add(len).ok_or(RunlistError::Overflow)?;
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
    use crate::testkit::encode_runs;

    fn all(runs: &[u8], vol: u64) -> Result<(Vec<Run>, Totals), RunlistError> {
        let mut out = Vec::new();
        let t = decode(runs, 0, vol, |r| out.push(r))?;
        Ok((out, t))
    }

    #[test]
    fn simple_list_with_a_hole_and_backward_jump() {
        // 8 clusters at 100, a 4-cluster hole, 2 clusters at 40 (a negative
        // delta from 100).
        let enc = encode_runs(&[(Some(100), 8), (None, 4), (Some(40), 2)]);
        let (runs, t) = all(&enc, 1000).unwrap();
        assert_eq!(
            runs,
            vec![
                Run {
                    vcn: 0,
                    len: 8,
                    lcn: Some(100)
                },
                Run {
                    vcn: 8,
                    len: 4,
                    lcn: None
                },
                Run {
                    vcn: 12,
                    len: 2,
                    lcn: Some(40)
                },
            ]
        );
        assert_eq!(
            (t.allocated, t.holes, t.runs, t.covered()),
            (10, 4, 3, Some(14))
        );
    }

    #[test]
    fn wide_fields_and_sign_extension() {
        let enc = encode_runs(&[(Some(0x0001_2345_6789), 0x1_0000), (Some(5), 1)]);
        let (runs, _) = all(&enc, 1 << 40).unwrap();
        assert_eq!(runs[0].lcn, Some(0x0001_2345_6789));
        assert_eq!(runs[1].lcn, Some(5));
        // A one-byte negative delta of -1 from 3 gives 2.
        let (runs, _) = all(&[0x11, 1, 3, 0x11, 1, 0xFF, 0], 100).unwrap();
        assert_eq!(runs[1].lcn, Some(2));
    }

    #[test]
    fn hostile_lists_are_errors_not_panics() {
        assert_eq!(all(&[0x11, 4], 100).unwrap_err(), RunlistError::Truncated);
        assert_eq!(
            all(&[0x21, 4, 9], 100).unwrap_err(),
            RunlistError::Truncated
        );
        assert_eq!(all(&[0x91, 4, 0], 100).unwrap_err(), RunlistError::BadSizes);
        assert_eq!(all(&[0x09, 4], 100).unwrap_err(), RunlistError::BadSizes);
        assert_eq!(all(&[0x10, 4], 100).unwrap_err(), RunlistError::BadSizes);
        assert_eq!(all(&[0x11, 0, 5], 100).unwrap_err(), RunlistError::EmptyRun);
        // Beyond the volume, before its start, and past the end by length.
        assert_eq!(
            all(&[0x11, 4, 99], 100).unwrap_err(),
            RunlistError::OutOfVolume
        );
        assert_eq!(
            all(&[0x11, 4, 0xFF], 100).unwrap_err(),
            RunlistError::OutOfVolume
        );
        assert_eq!(
            all(&[0x11, 4, 98], 100).unwrap_err(),
            RunlistError::OutOfVolume
        );
        // Eight-byte length with the top bit set.
        let mut huge = vec![0x18];
        huge.extend_from_slice(&u64::MAX.to_le_bytes());
        assert_eq!(all(&huge, 100).unwrap_err(), RunlistError::OutOfVolume);
        // Two holes whose lengths overflow when added.
        let big = (u64::MAX >> 1).to_le_bytes();
        let mut two = vec![0x08];
        two.extend_from_slice(&big);
        two.push(0x08);
        two.extend_from_slice(&big);
        two.push(0x08);
        two.extend_from_slice(&big);
        assert!(all(&two, 100).is_err());
        // Empty, terminated at once, and ending without a terminator.
        assert_eq!(all(&[], 100).unwrap().1.runs, 0);
        assert_eq!(all(&[0, 0x11, 1, 1], 100).unwrap().1.runs, 0);
        assert_eq!(all(&[0x11, 1, 1], 100).unwrap().1.runs, 1);
    }
}
