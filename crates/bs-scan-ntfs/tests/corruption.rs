//! The reader against hostile volumes, built in memory: thousands of seeded
//! corruptions of boot sectors, file records, update sequences, attributes,
//! run lists, names, attribute lists, the table's own record, plus damaged
//! and truncated devices. See `common/hostile.rs` for what is checked.
//!
//! `HOSTILE_SEEDS` sets the seeds per family and base (default 120);
//! `HOSTILE_SEED_BASE` is the first seed (default 0);
//! `HOSTILE_ONLY=base:family:seed` runs one case and prints it.

mod common;
#[path = "common/hostile.rs"]
mod hostile;

use hostile::*;

#[global_allocator]
static ALLOCATOR: Counting = Counting;

#[test]
fn the_harness_sees_a_panic_in_the_reader() {
    let out = run_variant(Patched::clean(std::sync::Arc::new(Panicky)), 1, 0);
    // The panic is in the boot-sector read; the scan reports it as an error
    // that is not a panic only if the reader had caught it, which would be
    // wrong: it must reach the harness as a panic or a refusal.
    assert!(out.panicked.is_some() || out.refused, "{out:?}");
}

#[test]
fn corrupted_volumes_never_panic_hang_or_balloon() {
    let bases = vec![
        make_base("plain", synthetic_plain()),
        make_base("extensions", synthetic_extensions()),
        make_base("deep", synthetic_deep()),
    ];
    // A clean read of each is clean.
    for b in &bases {
        let out = run_variant(Patched::clean(b.src.clone()), 2, b.baseline_unreachable);
        assert!(
            !out.refused && !out.damaged && out.panicked.is_none(),
            "{}: {out:?}",
            b.name
        );
    }
    let summary = drive(&bases);
    summary.print();
    summary.assert_every_family_bites();
}
