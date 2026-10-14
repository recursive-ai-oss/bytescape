//! The hash function behind the applier's maps.
//!
//! The applier looks up a directory key for nearly every record, so on a
//! large tree the maps' hash function is on the critical path of the single
//! thread that builds the tree. The standard library's default (SipHash) is
//! built to resist attack and costs several times what a multiplicative hash
//! does for the small integer keys used here.
//!
//! This one is a folded multiplication (the 128-bit product of the input and
//! a constant, with its two halves xored), keyed with two random 64-bit
//! values. The keys come from the standard library's own randomness
//! (`RandomState`, which the operating system seeds) and are drawn once per
//! process, so the hash of a key differs from run to run. Keys reach the
//! applier from scanners and, in the elevated helper's case, from a stream the
//! tree does not control; without the seed a crafted set of keys could pile up
//! in one bucket and turn lookups into list walks. With a seed the attacker
//! does not know, such keys spread like any others.
//!
//! This is not a cryptographic hash and makes no claim to be one. It resists
//! precomputed collisions, which is the threat for a map that lives for one
//! scan inside one process; it is not meant for keys whose hashes are ever
//! shown to the other side.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::sync::OnceLock;

/// Odd constants with well mixed bits (fractional parts of pi and e).
const MUL_A: u64 = 0x243F_6A88_85A3_08D3;
const MUL_B: u64 = 0x1319_8A2E_0370_7344;
const MUL_C: u64 = 0xA409_3822_299F_31D1;

/// The 128-bit product of `a` and `b`, folded to 64 bits.
#[inline]
fn fold(a: u64, b: u64) -> u64 {
    let product = u128::from(a) * u128::from(b);
    (product as u64) ^ ((product >> 64) as u64)
}

/// The per-process keys, drawn on first use.
fn process_keys() -> (u64, u64) {
    static KEYS: OnceLock<(u64, u64)> = OnceLock::new();
    *KEYS.get_or_init(|| {
        // `RandomState` is seeded by the operating system; hashing two
        // different constants with it yields two independent 64-bit values.
        let state = RandomState::new();
        (
            state.hash_one(0x6273_2d74_7265_6501u64),
            state.hash_one(0x6273_2d74_7265_6502u64),
        )
    })
}

/// Builds [`FastHasher`]s with one pair of keys.
#[derive(Clone, Copy, Debug)]
pub(crate) struct FastState {
    k0: u64,
    k1: u64,
}

impl FastState {
    /// Fixed keys, for tests that need to know them.
    #[cfg(test)]
    pub(crate) const fn with_keys(k0: u64, k1: u64) -> FastState {
        FastState { k0, k1 }
    }
}

impl Default for FastState {
    fn default() -> FastState {
        let (k0, k1) = process_keys();
        FastState { k0, k1 }
    }
}

impl BuildHasher for FastState {
    type Hasher = FastHasher;

    #[inline]
    fn build_hasher(&self) -> FastHasher {
        FastHasher {
            state: self.k0,
            k1: self.k1,
        }
    }
}

/// A keyed folded-multiplication hasher.
pub(crate) struct FastHasher {
    state: u64,
    k1: u64,
}

impl FastHasher {
    #[inline]
    fn add(&mut self, word: u64) {
        self.state = fold(self.state ^ word, MUL_A);
    }
}

impl Hasher for FastHasher {
    #[inline]
    fn finish(&self) -> u64 {
        // The second key and a second multiplication give the low bits (which
        // pick the bucket) and the high bits (which pick the control byte)
        // the same quality.
        fold(self.state ^ self.k1, MUL_B)
    }

    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let (words, rest) = bytes.as_chunks::<8>();
        for word in words {
            self.add(u64::from_le_bytes(*word));
        }
        if !rest.is_empty() {
            let mut word = [0u8; 8];
            if let Some(dst) = word.get_mut(..rest.len()) {
                dst.copy_from_slice(rest);
            }
            // The length keeps "a" and "a\0" apart.
            self.add(u64::from_le_bytes(word) ^ ((rest.len() as u64) << 56));
        }
    }

    #[inline]
    fn write_u8(&mut self, n: u8) {
        self.add(u64::from(n));
    }

    #[inline]
    fn write_u16(&mut self, n: u16) {
        self.add(u64::from(n));
    }

    #[inline]
    fn write_u32(&mut self, n: u32) {
        self.add(u64::from(n));
    }

    #[inline]
    fn write_u64(&mut self, n: u64) {
        self.add(n);
    }

    #[inline]
    fn write_u128(&mut self, n: u128) {
        self.add(n as u64);
        self.add(fold((n >> 64) as u64, MUL_C));
    }

    #[inline]
    fn write_usize(&mut self, n: usize) {
        self.add(n as u64);
    }
}

/// A map from small integers to values, hashed with [`FastState`].
pub(crate) type FastMap<K, V> = std::collections::HashMap<K, V, FastState>;

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::hash::Hash;

    fn hash_of<T: Hash + ?Sized>(state: &FastState, value: &T) -> u64 {
        state.hash_one(value)
    }

    #[test]
    fn the_same_keys_hash_the_same_and_different_keys_differ() {
        let s = FastState::with_keys(1, 2);
        assert_eq!(hash_of(&s, &7u32), hash_of(&s, &7u32));
        assert_ne!(hash_of(&s, &7u32), hash_of(&s, &8u32));
        let a = crate::FileKey { volume: 1, id: 2 };
        let b = crate::FileKey {
            volume: 1,
            id: 2 | (1u128 << 100),
        };
        assert_ne!(hash_of(&s, &a), hash_of(&s, &b));
        // Different seeds hash the same key differently.
        let t = FastState::with_keys(3, 4);
        assert_ne!(hash_of(&s, &7u32), hash_of(&t, &7u32));
        // Byte strings of different length differ even when padded alike.
        assert_ne!(hash_of(&s, &b"a"[..]), hash_of(&s, &b"a\0"[..]));
    }

    #[test]
    fn the_process_keys_are_drawn_once() {
        let a = FastState::default();
        let b = FastState::default();
        assert_eq!((a.k0, a.k1), (b.k0, b.k1));
    }

    #[test]
    fn sequential_keys_spread_over_buckets() {
        // Directory keys are consecutive integers, the friendliest input for
        // a weak hash to get wrong: count how many of 1024 buckets 100k of
        // them reach.
        let s = FastState::with_keys(0x1234_5678_9abc_def0, 0x0fed_cba9_8765_4321);
        let mut seen = vec![false; 1024];
        for k in 0..100_000u32 {
            seen[(hash_of(&s, &k) & 1023) as usize] = true;
        }
        assert!(seen.iter().filter(|&&b| b).count() > 1000);
        // And the top bits, which hashbrown uses for its control bytes.
        let mut top = [false; 128];
        for k in 0..100_000u32 {
            top[(hash_of(&s, &k) >> 57) as usize] = true;
        }
        assert!(top.iter().all(|&b| b));
    }

    /// Finds `want` keys that all land in the same one of 256 buckets under
    /// `state`: what an attacker who knew the keys could send.
    fn crafted_for(state: &FastState, want: usize) -> Vec<u32> {
        let mut out = Vec::with_capacity(want);
        let mut k = 0u32;
        while out.len() < want {
            if hash_of(state, &k) & 0xFF == 0 {
                out.push(k);
            }
            k += 1;
        }
        out
    }

    fn buckets_used(state: &FastState, keys: &[u32]) -> usize {
        let mut seen = vec![false; 256];
        for k in keys {
            seen[(hash_of(state, k) & 0xFF) as usize] = true;
        }
        seen.iter().filter(|&&b| b).count()
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(24))]

        /// Keys crafted to collide under one seed are ordinary keys under
        /// another: they use nearly all buckets, so a map seeded differently
        /// keeps its chains short and stays linear.
        #[test]
        fn keys_crafted_for_one_seed_do_not_collide_under_another(
            a0 in any::<u64>(), a1 in any::<u64>(), b0 in any::<u64>(), b1 in any::<u64>(),
        ) {
            prop_assume!((a0, a1) != (b0, b1) && a0 != b0);
            let attacker = FastState::with_keys(a0, a1);
            let actual = FastState::with_keys(b0, b1);
            let crafted = crafted_for(&attacker, 2048);
            // Under the seed they were crafted for, all collide ...
            prop_assert_eq!(buckets_used(&attacker, &crafted), 1);
            // ... under the real one they spread (2048 keys in 256 buckets
            // leave no bucket empty in expectation; require most to be used
            // and none to hold more than a small multiple of the average).
            prop_assert!(buckets_used(&actual, &crafted) > 200);
            let mut counts = vec![0u32; 256];
            for k in &crafted {
                counts[(hash_of(&actual, k) & 0xFF) as usize] += 1;
            }
            prop_assert!(counts.iter().copied().max().unwrap_or(0) < 40);
        }
    }

    #[test]
    fn a_map_with_crafted_keys_and_a_different_seed_stays_linear() {
        // The fixed-seed form of the property above, through a real map:
        // 200k keys crafted for one seed inserted and looked up under another
        // finish in a time that a quadratic chain walk could not.
        let attacker = FastState::with_keys(0xdead_beef_dead_beef, 0x1111_2222_3333_4444);
        let actual = FastState::with_keys(0x0123_4567_89ab_cdef, 0xfedc_ba98_7654_3210);
        let crafted = crafted_for(&attacker, 50_000);
        let mut map: FastMap<u32, u32> = FastMap::with_hasher(actual);
        let started = std::time::Instant::now();
        for (i, k) in crafted.iter().enumerate() {
            map.insert(*k, i as u32);
        }
        for (i, k) in crafted.iter().enumerate() {
            assert_eq!(map.get(k), Some(&(i as u32)));
        }
        // 50k keys in one chain would be over a billion steps; a healthy map
        // does this in milliseconds. The bound is generous for a loaded host.
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "took {:?}",
            started.elapsed()
        );
    }
}
