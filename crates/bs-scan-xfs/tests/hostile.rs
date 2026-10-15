//! Seeded corruption: the reader is run over file systems whose inode records
//! and directory listings are damaged on their way out of the source (fields
//! rewritten, records dropped, repeated or reordered, names and types and
//! inode numbers scrambled, listings cut short), thousands of ways. Whatever
//! the damage: no panic, an end within a time bound, and either a typed error
//! or a tree that is internally consistent, with every loss said.
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

use bs_scan_xfs::{BulkStat, EntryStat, Inodes, Names, OpenedDir, RawName, XfsError, XfsOptions};
use common::mem::{MemXfs, ROOT};
use common::try_read;
use std::io;
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

struct Damaged<'a> {
    fs: &'a MemXfs,
    seed: u64,
    rate: usize,
    calls: Mutex<u64>,
}

impl Damaged<'_> {
    fn rng(&self, salt: u64) -> Rng {
        let mut n = self.calls.lock().unwrap();
        *n += 1;
        Rng(self.seed ^ salt.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ (*n << 20) | 1)
    }
}

fn damage_stat(rng: &mut Rng, s: &mut EntryStat) {
    match rng.below(6) {
        0 => s.size = u64::MAX,
        1 => s.allocated = [0, u64::MAX, 1 << 62][rng.below(3)],
        2 => s.mode = rng.next() as u32,
        3 => s.nlink = rng.next() as u32,
        4 => s.mtime = rng.next() as i64,
        _ => s.ino = rng.next(),
    }
}

impl Inodes for Damaged<'_> {
    fn each(&self, each: &mut dyn FnMut(BulkStat) -> bool) -> Result<(), XfsError> {
        let mut rng = self.rng(1);
        let mut held: Option<BulkStat> = None;
        self.fs.each(&mut |mut r: BulkStat| {
            if rng.below(self.rate) == 0 {
                match rng.below(8) {
                    0 => r.size = u64::MAX,
                    1 => r.blocks = u64::MAX,
                    2 => r.block_size = [0, 1, u32::MAX][rng.below(3)],
                    3 => r.mode = rng.next() as u16,
                    4 => r.nlink = rng.next() as u32,
                    5 => r.mtime = rng.next() as i64,
                    // Leave it out.
                    6 => return true,
                    // Keep it for later (out of order).
                    _ => {
                        if let Some(old) = held.replace(r) {
                            return each(old);
                        }
                        return true;
                    }
                }
            }
            let go = each(r);
            if go && rng.below(40) == 0 {
                // Repeat it.
                return each(r);
            }
            go
        })?;
        if let Some(r) = held {
            each(r);
        }
        Ok(())
    }
}

impl Names for Damaged<'_> {
    type Dir = u64;

    fn root(&self) -> io::Result<OpenedDir<u64>> {
        self.fs.root()
    }

    fn list(&self, dir: &u64, each: &mut dyn FnMut(RawName<'_>)) -> io::Result<()> {
        let mut rng = self.rng(*dir);
        let mut error = None;
        self.fs.list(dir, &mut |raw: RawName<'_>| {
            if error.is_some() {
                return;
            }
            if rng.below(self.rate) != 0 {
                each(raw);
                return;
            }
            let mut name = raw.name.to_vec();
            let (mut ino, mut d_type, mut clean) = (raw.ino, raw.d_type, raw.clean);
            match rng.below(9) {
                0 => {
                    if !name.is_empty() {
                        let at = rng.below(name.len());
                        name[at] = [b'/', 0, b'x', 0xff][rng.below(4)];
                    }
                }
                1 => name.clear(),
                2 => d_type = rng.next() as u8,
                3 => ino = rng.next(),
                4 => ino = ROOT,
                5 => clean = false,
                // Leave it out.
                6 => return,
                // List it twice.
                7 => each(RawName {
                    name: &name,
                    ino,
                    d_type,
                    clean,
                }),
                _ => error = Some(io::Error::from(io::ErrorKind::InvalidData)),
            }
            if error.is_none() {
                each(RawName {
                    name: &name,
                    ino,
                    d_type,
                    clean,
                });
            }
        })?;
        match error {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    fn open_child(&self, parent: &u64, name: &[u8]) -> io::Result<OpenedDir<u64>> {
        let mut rng = self.rng(parent.wrapping_add(7));
        if rng.below(self.rate.saturating_mul(4)) == 0 {
            return Err(io::Error::from_raw_os_error(
                [13, 2, 20, 40, 5][rng.below(5)],
            ));
        }
        let mut opened = self.fs.open_child(parent, name)?;
        if rng.below(self.rate.saturating_mul(4)) == 0 {
            opened.stat.ino = rng.next();
        }
        Ok(opened)
    }

    fn stat_entry(&self, dir: &u64, name: &[u8]) -> io::Result<EntryStat> {
        let mut rng = self.rng(dir.wrapping_add(13));
        let mut s = self.fs.stat_entry(dir, name)?;
        if rng.below(self.rate) == 0 {
            damage_stat(&mut rng, &mut s);
        }
        Ok(s)
    }
}

fn fixture() -> MemXfs {
    let mut fs = MemXfs::new();
    let a = fs.mkdir(ROOT, "a");
    let b = fs.mkdir(a, "b");
    for i in 0..12 {
        fs.file(b, format!("plain{i}").as_bytes(), 4096 * (i + 1), i + 1);
    }
    let f = fs.file(a, b"linked", 20_000, 5);
    fs.link(b, "linked2", f);
    fs.symlink(ROOT, "link", 12);
    fs.fifo(ROOT, "fifo");
    fs.file(a, b"empty", 0, 0);
    fs.file(a, b"holey", 1 << 30, 2);
    let m = fs.mount_point(ROOT, "mnt", 9);
    let _ = m;
    fs.foreign_mount_point(a, "foreign", 10);
    let late = fs.file(a, b"late", 8192, 2);
    fs.hidden.insert(late);
    fs
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

fn corrupt_run(base: &MemXfs, seed: u64, rate: usize, options: &XfsOptions, t: &mut Tally) {
    let damaged = Damaged {
        fs: base,
        seed,
        rate,
        calls: Mutex::new(0),
    };
    let begun = Instant::now();
    let result = catch_unwind(AssertUnwindSafe(|| try_read(&damaged, &damaged, options)));
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
            let _ = e.to_string();
            t.refused += 1;
        }
        Ok(Ok(run)) => {
            // `try_read` has already checked the tree's invariants.
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

fn seeds(default: u64) -> std::ops::Range<u64> {
    let base: u64 = std::env::var("HOSTILE_SEED_BASE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let count: u64 = std::env::var("HOSTILE_SEEDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default);
    base..base + count
}

#[test]
fn damaged_records_and_listings_never_panic_the_reader_or_corrupt_the_tree() {
    let base = fixture();
    let mut t = Tally::default();
    for seed in seeds(1500) {
        for (rate, threads) in [(3usize, 1usize), (10, 3), (50, 4)] {
            let options = XfsOptions {
                threads: Some(threads),
                ..XfsOptions::default()
            };
            corrupt_run(&base, seed, rate, &options, &mut t);
        }
    }
    println!("{t:?}");
    // The corruption must have teeth: damage is seen and marked.
    assert!(t.marked > t.ok / 4, "{t:?}");
    assert!(t.ok > 0);
}

#[test]
fn damage_with_a_small_memory_limit_ends_in_a_typed_refusal_or_a_tree() {
    let base = fixture();
    let mut t = Tally::default();
    for seed in seeds(300) {
        for cap in [500u64, 5_000, 20_000] {
            let options = XfsOptions {
                max_memory_bytes: cap,
                threads: Some(2),
            };
            corrupt_run(&base, seed, 20, &options, &mut t);
        }
    }
    assert!(t.refused > 0, "{t:?}");
}
