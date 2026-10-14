//! Builds snapshots from declarative specs for the tests and the benchmark.
#![allow(dead_code)]

use bs_tree::{Aggregator, Batch, Category, DirKey, Meta, NodeId, NodeKind, Snapshot};

#[derive(Clone, Debug)]
pub struct SpecNode {
    /// Index of the containing directory; ignored for node 0.
    pub parent: usize,
    pub is_dir: bool,
    /// Allocated bytes of the entry itself.
    pub size: u64,
    /// The tree's category number.
    pub category: u8,
    pub name: Vec<u8>,
}

impl SpecNode {
    pub fn file(parent: usize, size: u64) -> SpecNode {
        SpecNode {
            parent,
            is_dir: false,
            size,
            category: 0,
            name: Vec::new(),
        }
    }

    pub fn dir(parent: usize) -> SpecNode {
        SpecNode {
            parent,
            is_dir: true,
            size: 4096,
            category: 0,
            name: Vec::new(),
        }
    }

    pub fn cat(mut self, category: u8) -> SpecNode {
        self.category = category;
        self
    }

    pub fn named(mut self, name: &[u8]) -> SpecNode {
        self.name = name.to_vec();
        self
    }
}

/// A tree whose node 0 is the root directory and whose every other node
/// names an earlier directory as its parent.
#[derive(Clone, Debug)]
pub struct Spec {
    pub nodes: Vec<SpecNode>,
}

impl Spec {
    pub fn new() -> Spec {
        Spec {
            nodes: vec![SpecNode::dir(0).named(b"/root")],
        }
    }

    /// Adds a node and returns its index (which is its node id).
    pub fn add(&mut self, node: SpecNode) -> usize {
        self.nodes.push(node);
        self.nodes.len() - 1
    }

    pub fn dir(&mut self, parent: usize) -> usize {
        self.add(SpecNode::dir(parent))
    }

    pub fn file(&mut self, parent: usize, size: u64) -> usize {
        self.add(SpecNode::file(parent, size))
    }
}

fn name_of(spec: &Spec, i: usize) -> Vec<u8> {
    let n = &spec.nodes[i];
    if n.name.is_empty() {
        format!("n{i}").into_bytes()
    } else {
        n.name.clone()
    }
}

/// How much of the scan has arrived.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Listing {
    /// Every directory reported as listed: the tree is complete.
    Complete,
    /// No directory reported as listed: everything is in progress.
    None,
}

pub fn aggregate(spec: &Spec, listing: Listing) -> Aggregator {
    let mut agg = Aggregator::new();
    let mut batch = Batch::new();
    let flush = |agg: &mut Aggregator, batch: &mut Batch| {
        agg.apply(batch);
        batch.clear();
    };
    for (i, n) in spec.nodes.iter().enumerate() {
        let meta = Meta {
            logical: n.size,
            allocated: n.size,
            category: Category(n.category),
            ..Meta::default()
        };
        let name = name_of(spec, i);
        if n.is_dir {
            let parent = if i == 0 {
                DirKey::NONE
            } else {
                DirKey(n.parent as u32)
            };
            batch.dir(DirKey(i as u32), parent, &name, &meta).unwrap();
        } else {
            batch
                .file(NodeKind::File, DirKey(n.parent as u32), &name, &meta)
                .unwrap();
        }
        if batch.len() >= 50_000 {
            flush(&mut agg, &mut batch);
        }
    }
    if listing == Listing::Complete {
        for (i, n) in spec.nodes.iter().enumerate().rev() {
            if n.is_dir {
                batch.listing_done(DirKey(i as u32));
            }
        }
    }
    flush(&mut agg, &mut batch);
    agg
}

pub fn build(spec: &Spec) -> Snapshot {
    aggregate(spec, Listing::Complete).snapshot()
}

pub fn id(i: usize) -> NodeId {
    NodeId::from_index(i as u32)
}

/// Small deterministic generator.
pub struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    pub fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }

    pub fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// A tree of about `files` files shaped like a real disk: folders hold a
/// log-normal spread of file sizes, a few folders hold most of the bytes, and
/// each folder's entries are contiguous, the way a scanner emits them.
/// Deterministic for a seed.
pub fn synthetic(files: usize, seed: u64) -> Spec {
    let mut rng = Rng(seed);
    let dir_count = (files / 40).max(1);
    // Random recursive tree of directories.
    let mut subdirs: Vec<Vec<usize>> = vec![Vec::new(); dir_count];
    for d in 1..dir_count {
        let parent = rng.below(d);
        subdirs[parent].push(d);
    }
    let mut file_counts = vec![0usize; dir_count];
    for _ in 0..files {
        file_counts[rng.below(dir_count)] += 1;
    }
    let mut spec = Spec::new();
    let mut index_of = vec![0usize; dir_count];
    let mut queue = std::collections::VecDeque::from([0usize]);
    while let Some(d) = queue.pop_front() {
        // Entries of one folder: its subfolders and files, in random order.
        let mut entries: Vec<Option<usize>> = subdirs[d].iter().map(|&c| Some(c)).collect();
        entries.extend(std::iter::repeat_n(None, file_counts[d]));
        for i in (1..entries.len()).rev() {
            let j = rng.below(i + 1);
            entries.swap(i, j);
        }
        for entry in entries {
            match entry {
                Some(c) => {
                    index_of[c] = spec.dir(index_of[d]);
                    queue.push_back(c);
                }
                None => {
                    let gauss = (rng.unit() + rng.unit() + rng.unit() + rng.unit() - 2.0) * 1.7;
                    let size = ((12.0 + 2.6 * gauss).exp() as u64).max(1);
                    let size = size.div_ceil(4096) * 4096;
                    let category = rng.below(9) as u8;
                    spec.add(SpecNode::file(index_of[d], size).cat(category));
                }
            }
        }
    }
    spec
}

// ---------------------------------------------------------------------
// Invariants every layout must satisfy.
// ---------------------------------------------------------------------

use bs_layout::{ItemKind, ItemRef, Layout, content_rect};
use std::collections::HashSet;

/// Checks the structural promises of a layout against the snapshot it was
/// made from. Returns a description of the first broken one.
pub fn check_invariants(snap: &Snapshot, layout: &Layout) -> Result<(), String> {
    let items = layout.items();
    let m = layout.metrics();
    let th = bs_layout::Thresholds::new(m);
    let mut ids = HashSet::new();
    // parents[i]: index of the nearest earlier item one level up.
    let mut parents: Vec<Option<usize>> = Vec::with_capacity(items.len());
    let mut last_at_depth: Vec<usize> = Vec::new();
    for (i, it) in items.iter().enumerate() {
        let r = it.rect;
        for v in [r.x, r.y, r.w, r.h] {
            if !v.is_finite() || v.fract() != 0.0 {
                return Err(format!("item {i}: edge {v} is not a whole finite number"));
            }
        }
        if r.w < 2.0 || r.h < 2.0 {
            return Err(format!("item {i}: drawn at {}x{}", r.w, r.h));
        }
        let nest = r.w >= th.nest_w && r.h >= th.nest_h;
        let label_fits = r.w >= th.label_w && r.h >= th.label_h;
        match it.kind {
            ItemKind::FolderTitled if !nest => {
                return Err(format!("item {i}: nested at only {}x{}", r.w, r.h));
            }
            ItemKind::FolderBlock if nest && it.depth < 250 => {
                return Err(format!("item {i}: block at {}x{}", r.w, r.h));
            }
            _ => {}
        }
        let expect_label = it.kind == ItemKind::FolderTitled || label_fits;
        if it.label.is_some() != expect_label {
            return Err(format!("item {i}: label presence wrong at {}x{}", r.w, r.h));
        }
        if i > 0 && it.bytes == 0 {
            return Err(format!("item {i}: a zero-byte box was drawn"));
        }
        if !ids.insert(it.id) {
            return Err(format!("item {i}: id {} repeated", it.id));
        }
        if layout.item_ref(it.id).is_none() {
            return Err(format!("item {i}: id {} has no meaning", it.id));
        }
        let d = usize::from(it.depth);
        if i == 0 {
            if d != 0 {
                return Err("the first item is not at depth 0".into());
            }
            parents.push(None);
        } else {
            if d == 0 || d > last_at_depth.len() {
                return Err(format!("item {i}: depth {d} skips a level"));
            }
            parents.push(Some(last_at_depth[d - 1]));
        }
        last_at_depth.truncate(d);
        last_at_depth.push(i);
    }

    let mut children: Vec<Vec<usize>> = vec![Vec::new(); items.len()];
    for (i, p) in parents.iter().enumerate() {
        if let Some(p) = p {
            children[*p].push(i);
        }
    }
    for (p, kids) in children.iter().enumerate() {
        if kids.is_empty() {
            continue;
        }
        let parent = &items[p];
        if parent.kind != ItemKind::FolderTitled {
            return Err(format!("item {p}: {:?} has children", parent.kind));
        }
        let content = content_rect(parent.rect, m);
        // What the children were sized from.
        let sizes: Vec<u64> = match layout.item_ref(parent.id) {
            Some(ItemRef::Node(n)) => snap
                .node(n)
                .ok_or("parent node missing")?
                .children()
                .map(|c| c.allocated())
                .collect(),
            Some(ItemRef::Group(spec)) => spec
                .members
                .iter()
                .map(|&n| snap.node(n).map_or(0, |n| n.allocated()))
                .collect(),
            None => return Err("parent id unknown".into()),
        };
        let total: f64 = sizes.iter().map(|&s| s as f64).sum();
        let mut covered = 0.0;
        for &k in kids {
            let c = &items[k];
            if c.depth != parent.depth + 1 {
                return Err(format!(
                    "item {k}: depth {} under depth {}",
                    c.depth, parent.depth
                ));
            }
            if !content.contains_rect(&c.rect) {
                return Err(format!(
                    "item {k}: {:?} leaves the content area {:?} of item {p}",
                    c.rect, content
                ));
            }
            let area = c.rect.area();
            covered += area;
            let expected = content.area() * c.bytes as f64 / total;
            let tolerance = f64::from(c.rect.w) + f64::from(c.rect.h) + 2.0;
            if (area - expected).abs() > tolerance {
                return Err(format!(
                    "item {k}: area {area} is not proportional to its bytes (expected {expected} +/- {tolerance})"
                ));
            }
            if let Some(ItemRef::Node(n)) = layout.item_ref(c.id) {
                let node = snap.node(n).ok_or("child node missing")?;
                if c.bytes != node.allocated() {
                    return Err(format!("item {k}: bytes differ from the tree"));
                }
            }
        }
        if covered > content.area() + 0.5 {
            return Err(format!("children of item {p} cover more than their parent"));
        }
        if kids.len() <= 1500 {
            for (a, &ka) in kids.iter().enumerate() {
                for &kb in &kids[a + 1..] {
                    if items[ka].rect.overlaps(&items[kb].rect) {
                        return Err(format!("items {ka} and {kb} overlap"));
                    }
                }
            }
        }
    }
    for (id, target) in layout.enterable() {
        if !ids.contains(id) {
            return Err(format!("enterable id {id} is not an item"));
        }
        if let (bs_layout::ViewTarget::Group(spec), Some(ItemRef::Group(g))) =
            (target, layout.item_ref(*id))
            && spec != g
        {
            return Err("enterable group differs from the item's group".into());
        }
    }
    Ok(())
}
