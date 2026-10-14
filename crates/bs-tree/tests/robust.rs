//! Random and hostile input: the applier and the decoder must never panic,
//! and whatever they accept must leave a consistent tree.

mod common;

use bs_tree::{
    Aggregator, ApplierLimits, Batch, Category, DirKey, FileKey, Meta, NodeFlags, NodeId, NodeKind,
    ReadErrorKind, ReadFailure,
};
use common::*;
use proptest::prelude::*;

#[derive(Clone, Debug)]
enum Op {
    Entry {
        kind: u8,
        key: u8,
        parent: u8,
        name_len: u16,
        logical: u64,
        allocated: u64,
        link: Option<(u8, u8)>,
        failure: Option<u8>,
        flags: u16,
    },
    Done(u8),
    Unreadable(u8, u8),
}

fn key(raw: u8) -> DirKey {
    // A small key space makes collisions, cycles and forward references
    // likely; 255 stands for "no parent".
    if raw == 255 {
        DirKey::NONE
    } else {
        DirKey(u32::from(raw % 12))
    }
}

fn big_or_small() -> impl Strategy<Value = u64> {
    prop_oneof![
        8 => 0u64..100_000,
        1 => Just(u64::MAX),
        1 => (u64::MAX - 10_000)..=u64::MAX,
    ]
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        10 => (
            0u8..4,
            any::<u8>(),
            any::<u8>(),
            prop_oneof![9 => 0u16..40, 1 => 1000u16..1100],
            big_or_small(),
            big_or_small(),
            proptest::option::of((0u8..3, 0u8..3)),
            proptest::option::of(1u8..8),
            any::<u16>(),
        )
            .prop_map(|(kind, key, parent, name_len, logical, allocated, link, failure, flags)| {
                Op::Entry { kind, key, parent, name_len, logical, allocated, link, failure, flags }
            }),
        3 => any::<u8>().prop_map(Op::Done),
        1 => (any::<u8>(), 1u8..8).prop_map(|(k, e)| Op::Unreadable(k, e)),
    ]
}

fn failure_of(code: u8) -> ReadFailure {
    ReadFailure {
        kind: ReadErrorKind::from_code(code),
        code: i32::from(code),
    }
}

trait FromCode {
    fn from_code(code: u8) -> ReadErrorKind;
}

impl FromCode for ReadErrorKind {
    fn from_code(code: u8) -> ReadErrorKind {
        match code % 6 {
            0 => ReadErrorKind::PermissionDenied,
            1 => ReadErrorKind::NotFound,
            2 => ReadErrorKind::Io,
            3 => ReadErrorKind::Corrupt,
            4 => ReadErrorKind::TooDeep,
            _ => ReadErrorKind::Other,
        }
    }
}

fn build(ops: &[Op], with_root: bool) -> Batch {
    let mut batch = Batch::new();
    if with_root {
        batch
            .dir(
                DirKey(0),
                DirKey::NONE,
                b"root",
                &Meta {
                    allocated: 4096,
                    ..Meta::default()
                },
            )
            .unwrap();
    }
    for op in ops {
        match op {
            Op::Entry {
                kind,
                key: k,
                parent,
                name_len,
                logical,
                allocated,
                link,
                failure,
                flags,
            } => {
                let meta = Meta {
                    logical: *logical,
                    allocated: *allocated,
                    mtime: *logical as i64,
                    flags: NodeFlags::from_bits_truncate(*flags),
                    category: Category((*flags >> 8) as u8),
                    link: link.map(|(v, i)| FileKey {
                        volume: u64::from(v),
                        id: u128::from(i),
                    }),
                    failure: failure.map(failure_of),
                };
                let name = vec![b'a' + (*k % 26); usize::from(*name_len)];
                let kind = NodeKind::from_u8_for_test(*kind);
                if kind == NodeKind::Dir {
                    batch.dir(key(*k), key(*parent), &name, &meta).unwrap();
                } else {
                    batch.file(kind, key(*parent), &name, &meta).unwrap();
                }
            }
            Op::Done(k) => batch.listing_done(key(*k)),
            Op::Unreadable(k, e) => batch.unreadable(key(*k), failure_of(*e)),
        }
    }
    batch
}

trait KindFromU8 {
    fn from_u8_for_test(v: u8) -> NodeKind;
}

impl KindFromU8 for NodeKind {
    fn from_u8_for_test(v: u8) -> NodeKind {
        match v % 4 {
            0 => NodeKind::File,
            1 => NodeKind::Dir,
            2 => NodeKind::Symlink,
            _ => NodeKind::Other,
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(400))]

    #[test]
    fn random_batches_never_panic_and_never_break_the_tree(
        batches in proptest::collection::vec(proptest::collection::vec(op(), 0..40), 1..8),
        with_root in any::<bool>(),
        small_limits in any::<bool>(),
    ) {
        let limits = if small_limits {
            ApplierLimits { max_nodes: 30, max_parked_records: 10, max_parked_name_bytes: 200 }
        } else {
            ApplierLimits::default()
        };
        let mut agg = Aggregator::with_limits(limits);
        for (i, ops) in batches.iter().enumerate() {
            let batch = build(ops, with_root && i == 0);

            // The wire form must survive a round trip into the same effect.
            let mut bytes = Vec::new();
            batch.encode(&mut bytes).unwrap();
            let decoded = Batch::decode(&bytes).expect("own output decodes");

            let _ = agg.apply(&decoded);
            let snap = agg.publish();
            if let Err(v) = snap.verify() {
                prop_assert!(false, "invariant broken: {v}");
            }
            if let Some(root) = snap.root() {
                // A directory is never smaller than its children together.
                let mut sum = 0u64;
                for child in root.children() {
                    sum += child.allocated();
                }
                prop_assert!(root.allocated() >= sum);
            }
        }
        let _ = agg.finish();
        let snap = agg.publish();
        prop_assert!(snap.verify().is_ok());
        // If anything was refused or lost the tree must not call itself done.
        if snap.lost_records() > 0 {
            prop_assert!(!snap.scan_complete());
        }
        // Walking every node must not panic or loop.
        for i in 0..snap.len() {
            let n = snap.node(NodeId::from_index(i)).unwrap();
            let _ = (n.name().len(), n.children().count(), snap.path(n.id(), b'/'));
        }
    }

    #[test]
    fn valid_trees_in_any_arrival_order_match_the_oracle(
        raw in proptest::collection::vec((any::<u32>(), any::<bool>(), any::<u16>(), 0u16..64), 0..120),
        seed in any::<u64>(),
    ) {
        let spec = spec_from_raw(&raw);
        let expected = expected_totals(&spec);
        let mut batches = batches_for(&spec);
        Rng(seed | 1).shuffle(&mut batches);
        let mut agg = Aggregator::new();
        for b in &batches {
            let r = agg.apply(b);
            prop_assert_eq!(r.rejected, 0);
            prop_assert!(agg.publish().verify().is_ok());
        }
        prop_assert_eq!(agg.finish().unresolved, 0);
        let snap = agg.publish();
        prop_assert!(snap.scan_complete());
        prop_assert_eq!(snap.len() as usize, spec.nodes.len());
        for i in 0..snap.len() {
            let n = snap.node(NodeId::from_index(i)).unwrap();
            let name = n.name();
            let idx: usize = if name == b"/root" {
                0
            } else {
                std::str::from_utf8(&name[1..]).unwrap().parse().unwrap()
            };
            prop_assert_eq!(
                (n.logical(), n.allocated(), u64::from(n.file_count())),
                expected[idx]
            );
        }
    }

    #[test]
    fn decoding_arbitrary_bytes_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..300)) {
        let _ = Batch::decode(&bytes);
    }

    #[test]
    fn decoding_damaged_valid_batches_never_panics_and_applies_safely(
        ops in proptest::collection::vec(op(), 1..30),
        flips in proptest::collection::vec((any::<u16>(), any::<u8>()), 1..6),
    ) {
        let batch = build(&ops, true);
        let mut bytes = Vec::new();
        batch.encode(&mut bytes).unwrap();
        for (at, value) in flips {
            let at = usize::from(at) % bytes.len();
            bytes[at] = value;
        }
        if let Ok(decoded) = Batch::decode(&bytes) {
            let mut agg = Aggregator::new();
            let _ = agg.apply(&decoded);
            prop_assert!(agg.publish().verify().is_ok());
        }
    }
}

/// The random generators above are only worth something if they reach the
/// refusal paths. Run a fixed, seeded stream of hostile batches and require
/// that each kind of refusal, the waiting path and unreadable marks all
/// actually occurred.
#[test]
fn seeded_hostile_stream_reaches_every_refusal_path() {
    use bs_tree::ApplyError as E;
    use std::collections::BTreeSet;
    let mut rng = Rng(0xDEAD_BEEF_1234_5678);
    let mut seen: BTreeSet<&'static str> = BTreeSet::new();
    let mut parked = 0u64;
    let mut unreadable = 0u64;
    for round in 0..3000 {
        let small = round % 3 == 0;
        let mut agg = Aggregator::with_limits(if small {
            ApplierLimits {
                max_nodes: 8,
                max_parked_records: 6,
                max_parked_name_bytes: 100,
            }
        } else {
            ApplierLimits::default()
        });
        for _ in 0..1 + rng.below(4) {
            let ops: Vec<Op> = (0..rng.below(30))
                .map(|_| match rng.below(14) {
                    0..=9 => Op::Entry {
                        kind: rng.below(4) as u8,
                        key: rng.below(256) as u8,
                        parent: rng.below(256) as u8,
                        name_len: [0, 3, 40, 1050][rng.below(4)],
                        logical: [0, 10, 10, 10, 77, 0, 3, u64::MAX][rng.below(8)],
                        allocated: [0, 4096, 4096, 8192, 0, 4096, 512, u64::MAX][rng.below(8)],
                        link: (rng.below(3) == 0).then(|| (rng.below(2) as u8, rng.below(2) as u8)),
                        failure: (rng.below(8) == 0).then(|| 1 + rng.below(7) as u8),
                        flags: rng.next() as u16,
                    },
                    10..=12 => Op::Done(rng.below(256) as u8),
                    _ => Op::Unreadable(rng.below(256) as u8, 1 + rng.below(7) as u8),
                })
                .collect();
            let report = agg.apply(&build(&ops, rng.below(2) == 0));
            parked += u64::from(report.parked);
            for (_, e) in &report.first_errors {
                seen.insert(match e {
                    E::RootMustBeDirectory => "RootMustBeDirectory",
                    E::RootAlreadyPresent => "RootAlreadyPresent",
                    E::ReservedKey => "ReservedKey",
                    E::SelfParent => "SelfParent",
                    E::Cycle => "Cycle",
                    E::DuplicateKey(_) => "DuplicateKey",
                    E::ParentComplete => "ParentComplete",
                    E::ListingAlreadyDone => "ListingAlreadyDone",
                    E::ScanFinished => "ScanFinished",
                    E::ParkLimit => "ParkLimit",
                    E::BadNameSpan => "BadNameSpan",
                    E::NodeIdsExhausted => "NodeIdsExhausted",
                    E::NamePoolFull => "NamePoolFull",
                    E::SizeOverflow => "SizeOverflow",
                });
            }
            let snap = agg.publish();
            snap.verify()
                .unwrap_or_else(|v| panic!("round {round}: {v}"));
            unreadable += (0..snap.len())
                .filter(|&i| snap.node(NodeId::from_index(i)).unwrap().is_unreadable())
                .count() as u64;
        }
    }
    for want in [
        "RootMustBeDirectory",
        "RootAlreadyPresent",
        "ReservedKey",
        "SelfParent",
        "Cycle",
        "DuplicateKey",
        "ParentComplete",
        "ListingAlreadyDone",
        "ParkLimit",
        "NodeIdsExhausted",
        "SizeOverflow",
    ] {
        assert!(seen.contains(want), "never reached {want}; saw {seen:?}");
    }
    assert!(parked > 0 && unreadable > 0);
}
