//! Property tests for the parsers: whatever the bytes are, they return
//! `None`/`Err`/some value, never panic, and what they return stays inside
//! the buffers they were given.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::cast_possible_truncation,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use crate::items::{self, Extent};
use crate::search::{Flow, HEADER_LEN, Key, PageEnd, read_page};
use proptest::collection::vec;
use proptest::prelude::*;

fn config() -> ProptestConfig {
    ProptestConfig {
        cases: 4000,
        failure_persistence: None,
        ..ProptestConfig::default()
    }
}

/// A result buffer built from items, whose headers are all plausible.
fn plausible_buffer() -> impl Strategy<Value = (Vec<u8>, u32)> {
    let item = (
        any::<u64>(),
        any::<u8>(),
        any::<u64>(),
        vec(any::<u8>(), 0..40),
    );
    vec(item, 0..30).prop_map(|items| {
        let mut out = Vec::new();
        let n = items.len() as u32;
        for (objectid, ty, offset, data) in items {
            out.extend_from_slice(&0u64.to_le_bytes());
            out.extend_from_slice(&objectid.to_le_bytes());
            out.extend_from_slice(&offset.to_le_bytes());
            out.extend_from_slice(&u32::from(ty).to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&data);
        }
        (out, n)
    })
}

fn inside(outer: &[u8], inner: &[u8]) -> bool {
    let o = outer.as_ptr() as usize;
    let i = inner.as_ptr() as usize;
    inner.is_empty() || (i >= o && i + inner.len() <= o + outer.len())
}

proptest! {
    #![proptest_config(config())]

    #[test]
    fn items_of_any_bytes_decode_or_are_refused(raw in vec(any::<u8>(), 0..400)) {
        let _ = items::parse_inode(&raw);
        let _ = items::parse_extent(&raw);
        let mut count = 0usize;
        for e in items::dir_entries(&raw) {
            count += 1;
            // No more entries than the bytes could hold, and no endless item.
            assert!(count <= raw.len() / 30 + 1);
            if let Ok(entry) = e {
                assert!(inside(&raw, entry.name));
            }
        }
        if let Some(Extent::Inline { bytes }) = items::parse_extent(&raw) {
            assert_eq!(bytes as usize, raw.len() - 21);
        }
    }

    #[test]
    fn a_result_buffer_of_any_bytes_stays_inside_itself(
        raw in vec(any::<u8>(), 0..600),
        n in any::<u32>(),
        lo in any::<u64>(),
        hi in any::<u64>(),
    ) {
        let floor = Key { objectid: lo.min(hi), ty: 0, offset: 0 };
        let ceil = Key { objectid: lo.max(hi), ty: u8::MAX, offset: u64::MAX };
        let mut last: Option<Key> = None;
        let result = read_page(&raw, n, floor, ceil, &mut |item| {
            assert!(inside(&raw, item.data));
            assert!(item.key >= floor && item.key <= ceil);
            assert!(last.is_none_or(|l| item.key > l));
            last = Some(item.key);
            Flow::Continue
        });
        if let Ok(end) = result {
            match end {
                PageEnd::Last(k) => assert_eq!(Some(k), last),
                PageEnd::Empty => assert!(last.is_none()),
                PageEnd::Stopped => unreachable!("the callback never stops"),
            }
        }
        // Never more items than the buffer has room for.
        if n as usize > raw.len() / HEADER_LEN {
            assert!(result.is_err());
        }
    }

    #[test]
    fn a_plausible_buffer_either_reads_in_order_or_is_refused(
        (buf, n) in plausible_buffer(),
    ) {
        let mut keys = Vec::new();
        let result = read_page(&buf, n, Key::MIN, Key::MAX, &mut |item| {
            keys.push(item.key);
            Flow::Continue
        });
        match result {
            Ok(_) => {
                assert_eq!(keys.len(), n as usize);
                assert!(keys.windows(2).all(|w| w[0] < w[1]));
            }
            Err(_) => assert!(keys.len() < n as usize || n == 0),
        }
    }

    #[test]
    fn keys_have_a_successor_that_is_the_next_in_order(
        objectid in any::<u64>(), ty in any::<u8>(), offset in any::<u64>(),
    ) {
        let k = Key { objectid, ty, offset };
        match k.successor() {
            Some(next) => assert!(next > k),
            None => assert_eq!(k, Key::MAX),
        }
    }
}
