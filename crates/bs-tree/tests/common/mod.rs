//! Shared helpers for the integration tests: a declarative tree spec, the
//! batches a scanner would produce for it, and the totals an independent
//! oracle expects.
#![allow(dead_code)]

use bs_tree::{Batch, DirKey, Meta, NodeKind};

#[derive(Clone, Debug)]
pub struct SpecNode {
    pub parent: usize,
    pub is_dir: bool,
    pub logical: u64,
    pub allocated: u64,
}

/// Node 0 is the root directory; every other node's parent is an earlier
/// directory.
#[derive(Clone, Debug)]
pub struct Spec {
    pub nodes: Vec<SpecNode>,
}

/// Builds a spec from raw numbers: `raw[i]` describes node `i + 1`.
pub fn spec_from_raw(raw: &[(u32, bool, u16, u16)]) -> Spec {
    let mut nodes = vec![SpecNode {
        parent: 0,
        is_dir: true,
        logical: 0,
        allocated: 4096,
    }];
    let mut dirs = vec![0usize];
    for &(parent_pick, is_dir, logical, allocated) in raw {
        let parent = dirs[parent_pick as usize % dirs.len()];
        let index = nodes.len();
        nodes.push(SpecNode {
            parent,
            is_dir,
            logical: u64::from(logical),
            allocated: u64::from(allocated) * 512,
        });
        if is_dir {
            dirs.push(index);
        }
    }
    Spec { nodes }
}

pub fn name_of(index: usize) -> Vec<u8> {
    if index == 0 {
        b"/root".to_vec()
    } else {
        format!("n{index}").into_bytes()
    }
}

pub fn meta_of(node: &SpecNode) -> Meta {
    Meta {
        logical: node.logical,
        allocated: node.allocated,
        mtime: 1_700_000_000 + node.logical as i64,
        ..Meta::default()
    }
}

/// One batch per directory: its own entry first when it is the root, then its
/// children, then the end-of-listing record. Returned in directory order.
pub fn batches_for(spec: &Spec) -> Vec<Batch> {
    let mut batches = Vec::new();
    for (dir, node) in spec.nodes.iter().enumerate() {
        if !node.is_dir {
            continue;
        }
        let mut batch = Batch::new();
        if dir == 0 {
            batch
                .dir(DirKey(0), DirKey::NONE, &name_of(0), &meta_of(node))
                .unwrap();
        }
        for (i, child) in spec.nodes.iter().enumerate().skip(1) {
            if child.parent != dir {
                continue;
            }
            if child.is_dir {
                batch
                    .dir(
                        DirKey(i as u32),
                        DirKey(dir as u32),
                        &name_of(i),
                        &meta_of(child),
                    )
                    .unwrap();
            } else {
                batch
                    .file(
                        NodeKind::File,
                        DirKey(dir as u32),
                        &name_of(i),
                        &meta_of(child),
                    )
                    .unwrap();
            }
        }
        batch.listing_done(DirKey(dir as u32));
        batches.push(batch);
    }
    batches
}

/// `(logical, allocated, files)` of each node's subtree, own entry included.
pub fn expected_totals(spec: &Spec) -> Vec<(u64, u64, u64)> {
    let mut totals: Vec<(u64, u64, u64)> = spec
        .nodes
        .iter()
        .map(|n| (n.logical, n.allocated, u64::from(!n.is_dir)))
        .collect();
    for i in (1..spec.nodes.len()).rev() {
        let (l, a, f) = totals[i];
        let p = spec.nodes[i].parent;
        totals[p].0 += l;
        totals[p].1 += a;
        totals[p].2 += f;
    }
    totals
}

/// Deterministic xorshift, for shuffles and corruption in tests.
pub struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    pub fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    pub fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            let j = self.below(i + 1);
            items.swap(i, j);
        }
    }
}
