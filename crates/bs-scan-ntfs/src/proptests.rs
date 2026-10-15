//! Property tests over arbitrary and mutated bytes: whatever the disk holds,
//! the parsers return a value or an error, and finish.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::cast_possible_truncation,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use crate::boot;
use crate::parse::{ChunkOut, Parser, list_entries};
use crate::record::{Body, attrs, fix_and_parse, peek, split_ref};
use crate::runlist::{self, Run};
use crate::testkit::{RecordBuilder, encode_runs, stamp};
use proptest::prelude::*;

fn geometry() -> boot::Geometry {
    let sector = {
        let mut s = vec![0u8; 512];
        s[3..11].copy_from_slice(b"NTFS    ");
        s[0x0B..0x0D].copy_from_slice(&512u16.to_le_bytes());
        s[0x0D] = 8;
        s[0x28..0x30].copy_from_slice(&(1u64 << 20).to_le_bytes());
        s[0x30..0x38].copy_from_slice(&4u64.to_le_bytes());
        s[0x40] = 0xF6;
        s
    };
    boot::parse(&sector, 1 << 40).unwrap()
}

/// A valid record with a bit of everything, to mutate.
fn rich_record() -> Vec<u8> {
    let runs = encode_runs(&[(Some(100), 8), (None, 4), (Some(40), 2)]);
    RecordBuilder::new(1024, 3)
        .standard_info(0x20, 1_600_000_000)
        .file_name(5, 5, 1, "mutated.txt")
        .file_name(5, 5, 2, "MUTATE~1.TXT")
        .file_name(9, 1, 0, "second")
        .data_resident("ads", &[1, 2, 3])
        .nonres(&crate::testkit::NonRes {
            ty: 0x80,
            name: String::new(),
            flags: 0x8000,
            lowest_vcn: 0,
            highest_vcn: 13,
            unit: 4,
            allocated: 14 * 4096,
            size: 50_000,
            initialized: 50_000,
            compressed: Some(10 * 4096),
            runs,
        })
        .resident(0xC0, "", &[0x0C, 0, 0, 0xA0, 0, 0, 0, 0])
        .resident(0xE0, "", &[0u8; 16])
        .build()
}

fn parse_one(mut rec: Vec<u8>, no: u64) -> ChunkOut {
    let geo = geometry();
    let charge = crate::budget::Charge::new(u64::MAX, 0);
    let mut parser = Parser::new(&geo, &charge);
    let mut out = ChunkOut::default();
    // A record that was not numbered takes the number of its slot.
    if rec[0x2C..0x30] == crate::testkit::NUMBER_UNSET.to_le_bytes() {
        rec[0x2C..0x30].copy_from_slice(&u32::try_from(no).unwrap_or(0).to_le_bytes());
    }
    parser.record(&mut rec, no, &mut out);
    out
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(4000))]

    #[test]
    fn any_run_list_decodes_or_errors(bytes in proptest::collection::vec(any::<u8>(), 0..200), first in any::<u64>(), clusters in any::<u64>()) {
        let mut runs: Vec<Run> = Vec::new();
        let result = runlist::decode(&bytes, first, clusters, |r| runs.push(r));
        // Never more runs than the bytes can hold, and each is inside the
        // volume.
        prop_assert!(runs.len() <= bytes.len() / 2 + 1);
        for r in &runs {
            prop_assert!(r.len > 0);
            if let Some(lcn) = r.lcn {
                prop_assert!(lcn.checked_add(r.len).is_some_and(|e| e <= clusters));
            }
        }
        if let Ok(t) = result {
            prop_assert_eq!(t.runs as usize, runs.len());
        }
    }

    #[test]
    fn any_attribute_list_is_walked_or_refused(bytes in proptest::collection::vec(any::<u8>(), 0..400)) {
        let mut n = 0usize;
        let _ = list_entries(&bytes, |_| n += 1);
        // An entry takes at least 0x1A bytes.
        prop_assert!(n <= bytes.len() / 0x1A + 1);
    }

    #[test]
    fn any_boot_sector_parses_or_errors(bytes in proptest::collection::vec(any::<u8>(), 0..600), len in any::<u64>()) {
        let _ = boot::parse(&bytes, len);
        let _ = boot::size_hint(&bytes);
    }

    #[test]
    fn any_bytes_in_a_record_slot_are_handled(mut bytes in proptest::collection::vec(any::<u8>(), 1024..=1024), magic in any::<bool>(), flags in any::<u16>()) {
        if magic {
            bytes[..4].copy_from_slice(b"FILE");
            bytes[0x16..0x18].copy_from_slice(&(flags | 1).to_le_bytes());
        }
        let _ = peek(&bytes);
        let mut copy = bytes.clone();
        if let Ok(h) = fix_and_parse(&mut copy) {
            let mut n = 0;
            for a in attrs(&copy, &h) {
                n += 1;
                prop_assert!(n <= 1024 / 16 + 1);
                if let Ok(a) = a && let Body::NonResident(nr) = a.body {
                    let _ = runlist::decode(nr.runs, nr.lowest_vcn, 1 << 20, |_| {});
                }
            }
        }
        let out = parse_one(bytes, 40);
        prop_assert!(out.bases.len() + out.exts.len() <= 1);
    }

    #[test]
    fn a_valid_record_with_bytes_changed_is_handled(edits in proptest::collection::vec((0usize..1024, any::<u8>()), 1..24), restamp in any::<bool>()) {
        let mut rec = rich_record();
        for (at, v) in edits {
            rec[at] = v;
        }
        if restamp {
            // Keep the update sequence valid so that the attributes are
            // reached.
            if u16::from_le_bytes([rec[4], rec[5]]) == 0x30 && u16::from_le_bytes([rec[6], rec[7]]) == 3 {
                stamp(&mut rec);
            }
        }
        let out = parse_one(rec, 77);
        prop_assert!(out.bases.len() + out.exts.len() <= 1);
        for n in &out.names {
            prop_assert!(usize::from(n.len) <= 255 * 3);
            prop_assert!(n.off as usize + usize::from(n.len) <= out.pool.len());
        }
    }

    #[test]
    fn the_file_reference_splits_without_loss(r in any::<u64>()) {
        let (rec, seq) = split_ref(r);
        prop_assert_eq!(rec | (u64::from(seq) << 48), r);
    }
}

#[test]
fn the_rich_record_parses_cleanly() {
    let out = parse_one(rich_record(), 77);
    assert_eq!(out.bases.len(), 1);
    assert_eq!(out.diag.total(), 0, "{:?}", out.diag);
    let f = &out.bases[0].file;
    // 10 clusters back the stream; 4 are a hole.
    assert_eq!(f.c.alloc, 10 * 4096);
    assert_eq!(f.c.logical, 50_000);
    assert_eq!(f.c.ads_logical, 3);
    // The DOS alias is dropped, the second parent's name kept.
    assert_eq!(out.names.len(), 2);
    assert_eq!(f.reparse_tag, 0xA000_000C);
}
