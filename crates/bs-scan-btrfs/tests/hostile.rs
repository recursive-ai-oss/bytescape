//! Seeded corruption: the reader is run over file systems whose items are
//! damaged on their way out of the source (bits flipped, lengths rewritten,
//! items cut short, dropped, repeated, keys scrambled), thousands of ways.
//! Whatever the damage: no panic, an end within a time bound, and either a
//! typed error or a tree that is internally consistent and was accepted
//! without a single refused record.
#![cfg(target_os = "linux")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::cast_possible_truncation,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

mod common;

use bs_scan_btrfs::{BtrfsError, BtrfsOptions, Flow, Item, Key, TreeSource};
use common::memsrc::{Ext, MemFs, MemSource};
use common::{START, try_read};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Mutex;
use std::time::{Duration, Instant};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

/// Damages the items of another source as they pass.
struct Corrupting<'a> {
    inner: &'a dyn TreeSource,
    seed: u64,
    /// How often an item is damaged: one in this many.
    rate: usize,
    searches: Mutex<u64>,
}

impl TreeSource for Corrupting<'_> {
    fn search(
        &self,
        tree: u64,
        from: Key,
        to: Key,
        each: &mut dyn FnMut(Item<'_>) -> Flow,
    ) -> Result<(), BtrfsError> {
        let n = {
            let mut s = self.searches.lock().unwrap();
            *s += 1;
            *s
        };
        let mut rng = Rng(self.seed ^ tree.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ (n << 32) | 1);
        let mut stopped = false;
        let mut held: Option<(Key, Vec<u8>)> = None;
        self.inner.search(tree, from, to, &mut |item: Item<'_>| {
            if stopped {
                return Flow::Stop;
            }
            let mut key = item.key;
            let mut data = item.data.to_vec();
            if rng.below(self.rate) == 0 {
                match rng.below(9) {
                    // Flip a few bits.
                    0 => {
                        for _ in 0..=rng.below(3) {
                            if !data.is_empty() {
                                let at = rng.below(data.len());
                                data[at] ^= 1 << rng.below(8);
                            }
                        }
                    }
                    // Cut the item short.
                    1 => {
                        let to = rng.below(data.len() + 1);
                        data.truncate(to);
                    }
                    // Rewrite a length or count field with something big.
                    2 => {
                        let spots = [16usize, 25, 27, 29, 40, 52, 21, 29, 37, 45];
                        let at = spots[rng.below(spots.len())];
                        if at + 8 <= data.len() {
                            let v =
                                [0u64, 1, u64::MAX, 1 << 40, 0x7FFF_FFFF, rng.next()][rng.below(6)];
                            let len = [2usize, 4, 8][rng.below(3)];
                            data[at..at + len].copy_from_slice(&v.to_le_bytes()[..len]);
                        }
                    }
                    // Scramble the key.
                    3 => match rng.below(3) {
                        0 => key.objectid = rng.next(),
                        1 => key.offset = rng.next(),
                        _ => key.ty = rng.next() as u8,
                    },
                    // Leave the item out.
                    4 => return Flow::Continue,
                    // Deliver it twice.
                    5 => {
                        if each(Item { key, data: &data }) == Flow::Stop {
                            stopped = true;
                            return Flow::Stop;
                        }
                    }
                    // Swap it with the next one.
                    6 => {
                        if let Some((k, d)) = held.take() {
                            let first = each(Item { key: k, data: &d });
                            if first == Flow::Stop {
                                stopped = true;
                                return Flow::Stop;
                            }
                        }
                        held = Some((key, data));
                        return Flow::Continue;
                    }
                    // Fill it with noise.
                    7 => {
                        for b in &mut data {
                            *b = rng.next() as u8;
                        }
                    }
                    // Make it enormous.
                    _ => data.resize(data.len() + 100_000, 0xAB),
                }
            }
            if let Some((k, d)) = held.take() {
                let first = each(Item { key, data: &data });
                if first == Flow::Stop {
                    stopped = true;
                    return Flow::Stop;
                }
                return each(Item { key: k, data: &d });
            }
            each(Item { key, data: &data })
        })?;
        if let Some((k, d)) = held {
            each(Item { key: k, data: &d });
        }
        Ok(())
    }
}

/// A file system with a bit of everything.
fn fixture() -> MemSource {
    let mut fs = MemFs::new();
    let a = fs.mkdir(5, 256, "a");
    let b = fs.mkdir(5, a, "b");
    let shared = fs.fresh_extent(65536);
    for i in 0..12 {
        fs.plain(5, b, &format!("plain{i}"), 4096 * (i + 1));
    }
    fs.file(5, a, b"s1", 65536, &[Ext::Disk(shared, 65536, false)]);
    fs.file(5, a, b"s2", 65536, &[Ext::Disk(shared, 65536, false)]);
    let packed = fs.fresh_extent(8192);
    fs.file(5, a, b"packed", 1 << 20, &[Ext::Disk(packed, 8192, true)]);
    fs.file(5, a, b"mixed", 400, &[Ext::Inline, Ext::Hole]);
    fs.file(5, a, b"empty", 0, &[]);
    let f = fs.plain(5, 256, "linked", 20_000);
    fs.link(5, a, "linked2", f);
    fs.symlink(5, 256, "link", "a/b/plain0");
    fs.fifo(5, 256, "fifo");
    fs.subvolume(5, a, "sub", 300);
    fs.file(
        300,
        256,
        b"in-sub",
        65536,
        &[Ext::Disk(shared, 65536, false)],
    );
    let d = fs.mkdir(300, 256, "dir");
    fs.plain(300, d, "deep", 8192);
    fs.subvolume(300, d, "sub2", 301);
    fs.plain(301, 256, "leaf", 4096);
    fs.make_tree(400);
    fs.file(
        400,
        256,
        b"elsewhere",
        65536,
        &[Ext::Disk(shared, 65536, false)],
    );
    fs.source()
}

fn limit() -> Duration {
    if cfg!(debug_assertions) {
        Duration::from_secs(20)
    } else {
        Duration::from_secs(5)
    }
}

#[derive(Default, Debug)]
struct Tally {
    ok: u64,
    refused: u64,
    marked: u64,
}

fn corrupt_run(base: &MemSource, seed: u64, rate: usize, options: &BtrfsOptions, t: &mut Tally) {
    let source = Corrupting {
        inner: base,
        seed,
        rate,
        searches: Mutex::new(0),
    };
    let begun = Instant::now();
    let result = catch_unwind(AssertUnwindSafe(|| try_read(&source, START, options)));
    if begun.elapsed() > Duration::from_millis(500) {
        eprintln!("seed {seed}, rate {rate}: slow, {:?}", begun.elapsed());
    }
    assert!(
        begun.elapsed() < limit(),
        "seed {seed}: took {:?}",
        begun.elapsed()
    );
    match result {
        Err(panic) => {
            let msg = panic
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| panic.downcast_ref::<&str>().map(|s| (*s).to_owned()))
                .unwrap_or_default();
            panic!("seed {seed}, rate {rate}: the reader panicked: {msg}");
        }
        Ok(Err(e)) => {
            // A typed refusal is an answer.
            let _ = e.to_string();
            t.refused += 1;
        }
        Ok(Ok(run)) => {
            // `try_read` has already checked the tree's invariants. The reader
            // makes parents before children with unique keys, so the tree
            // refuses a record only for a size it cannot add (a hostile
            // length of 2^64 - 1); it then says so, and says what is lost.
            if run.applied_rejected == 0 {
                assert_eq!(run.parked, 0, "seed {seed}: records parked");
            } else {
                assert!(
                    run.refusals.iter().all(|r| r.contains("SizeOverflow")),
                    "seed {seed}, rate {rate}: records refused: {:?}",
                    run.refusals
                );
                assert!(!run.snapshot.scan_complete(), "seed {seed}");
                assert_eq!(
                    run.snapshot.lost_records(),
                    run.applied_rejected + run.parked,
                    "seed {seed}"
                );
                assert!(run.snapshot.root().unwrap().has_lost_records_below());
            }
            t.ok += 1;
            let root = run.snapshot.root().unwrap();
            if root.has_unreadable_below()
                || root.is_unreadable()
                || run.report.diagnostics.total() > 0
            {
                t.marked += 1;
            }
        }
    }
}

fn seeds(var: &str, default: u64) -> std::ops::Range<u64> {
    let base: u64 = std::env::var("HOSTILE_SEED_BASE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let count: u64 = std::env::var(var)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default);
    base..base + count
}

#[test]
fn damaged_items_never_panic_the_reader_or_corrupt_the_tree() {
    let base = fixture();
    let options = BtrfsOptions::default();
    let mut t = Tally::default();
    for seed in seeds("HOSTILE_SEEDS", 4000) {
        for rate in [3usize, 12, 60] {
            corrupt_run(&base, seed, rate, &options, &mut t);
        }
    }
    println!("{t:?}");
    // The corruption must have teeth: damage is seen and marked.
    assert!(t.marked > t.ok / 4, "{t:?}");
    assert!(t.ok > 0);
}

#[test]
fn damaged_items_with_a_small_memory_limit_end_in_a_typed_refusal_or_a_tree() {
    let base = fixture();
    let mut t = Tally::default();
    for seed in seeds("HOSTILE_SEEDS", 400) {
        for cap in [2_000u64, 20_000, 60_000] {
            let options = BtrfsOptions {
                max_memory_bytes: cap,
                ..BtrfsOptions::default()
            };
            corrupt_run(&base, seed, 20, &options, &mut t);
        }
    }
    assert!(t.refused > 0, "{t:?}");
}

#[test]
fn damaged_items_without_the_census_are_as_harmless() {
    let base = fixture();
    let options = BtrfsOptions {
        census: false,
        ..BtrfsOptions::default()
    };
    let mut t = Tally::default();
    for seed in seeds("HOSTILE_SEEDS", 1500) {
        corrupt_run(&base, seed, 8, &options, &mut t);
    }
}
