//! Property tests for the parsers: whatever the bytes are, they return
//! `None`/`Err`/some value, never panic, and what they return stays inside
//! the buffers they were given.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::cast_possible_truncation,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use crate::bulkstat::{RECORD_LEN, ReplyEnd, parse_record, read_reply};
use proptest::collection::vec;
use proptest::prelude::*;

fn config() -> ProptestConfig {
    ProptestConfig {
        cases: 4000,
        failure_persistence: None,
        ..ProptestConfig::default()
    }
}

/// A reply whose records have plausible, mostly increasing inode numbers.
fn plausible_reply() -> impl Strategy<Value = (Vec<u8>, u32, u64)> {
    (
        vec(
            (1u64..1000, vec(any::<u8>(), RECORD_LEN - 8..RECORD_LEN - 7)),
            0..20,
        ),
        0u64..50,
    )
        .prop_map(|(recs, after)| {
            let mut ino = after;
            let mut out = Vec::new();
            let n = recs.len() as u32;
            for (step, rest) in recs {
                ino = ino.saturating_add(step);
                out.extend_from_slice(&ino.to_le_bytes());
                out.extend_from_slice(&rest);
            }
            (out, n, after)
        })
}

proptest! {
    #![proptest_config(config())]

    #[test]
    fn a_record_of_any_bytes_decodes_or_is_refused(raw in vec(any::<u8>(), 0..400)) {
        let r = parse_record(&raw);
        assert_eq!(r.is_some(), raw.len() >= RECORD_LEN);
        if let Some(r) = r {
            // Sizes are the disk's to claim; the product cannot overflow.
            let _ = r.allocated();
        }
    }

    #[test]
    fn a_reply_of_any_bytes_stays_inside_itself(
        raw in vec(any::<u8>(), 0..1200),
        n in any::<u32>(),
        after in any::<u64>(),
    ) {
        let mut inos = Vec::new();
        let result = read_reply(&raw, n, after, &mut |r| inos.push(r.ino));
        if n as usize > raw.len() / RECORD_LEN {
            assert!(result.is_err());
            assert!(inos.is_empty());
        }
        if let Ok(end) = result {
            assert_eq!(inos.len(), n as usize);
            assert!(inos.windows(2).all(|w| w[0] < w[1]));
            assert!(inos.iter().all(|&i| i >= after));
            match end {
                ReplyEnd::Empty => assert!(inos.is_empty()),
                ReplyEnd::Last(l) => assert_eq!(Some(&l), inos.last()),
            }
        }
    }

    #[test]
    fn a_plausible_reply_is_read_in_full((buf, n, after) in plausible_reply()) {
        let mut count = 0u32;
        let result = read_reply(&buf, n, after, &mut |_| count += 1);
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(count, n);
    }
}
