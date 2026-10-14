//! A deterministic synthetic folder tree and a squarified layout for it.
//!
//! This stands in for the real size tree and layout code while the drawing
//! path is being proven. It produces the same flat list of boxes the real
//! layout will: a rectangle, a kind, a depth, an optional label, an id and a
//! category per box. Swapping in the real layout means writing an adapter from
//! that list to [`bs_render::Item`].

use std::collections::HashMap;
use std::sync::Arc;

use bs_render::{Flags, Item, ItemKind, ItemLabel, Rect};
use bs_ui::{Layout, ViewId, ViewSource};

/// Small, fast, seedable generator (splitmix64).
#[derive(Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    pub fn unit(&mut self) -> f32 {
        (self.next_u64() >> 40) as f32 / (1u64 << 24) as f32
    }

    /// Roughly normal, mean 0, deviation 1.
    pub fn gauss(&mut self) -> f32 {
        (0..4).map(|_| self.unit()).sum::<f32>() * 1.732 - 3.464
    }
}

#[derive(Clone, Copy, Debug)]
struct Node {
    seed: u64,
    size: u64,
    depth: u8,
    dir: bool,
}

const WORDS: [&str; 40] = [
    "Documents",
    "cache",
    "build",
    "target",
    "node_modules",
    "Pictures",
    "backup",
    "Downloads",
    "Library",
    "share",
    "lib",
    "src",
    "assets",
    "video",
    "music",
    "vendor",
    "logs",
    "tmp",
    "projects",
    "archive",
    "data",
    "index",
    "release",
    "images",
    "IMG_20240312_141516_HDR",
    "Screenshot from 2024-03-12 14-15-16",
    "Quarterly report (final) v2",
    "Übersicht-über-Änderungen",
    "café-menu-résumé",
    "Фотографии",
    "写真アルバム",
    "שלום-עולם",
    "ملفات",
    "com.example.application.cache.webpack.production",
    "site-packages",
    "x86_64-unknown-linux-gnu",
    "Program Files (x86)",
    "thumbnails-large-2048",
    "a",
    "WindowsApps",
];
const EXTS: [&str; 8] = ["mp4", "png", "flac", "pdf", "zip", "dll", "rs", "tmp"];

fn node_name(n: &Node) -> String {
    let w = WORDS[(n.seed >> 8) as usize % WORDS.len()];
    // Every third name carries a second word, so lengths vary widely.
    let extra = if (n.seed >> 14).is_multiple_of(3) {
        format!("-{}", WORDS[(n.seed >> 30) as usize % WORDS.len()])
    } else {
        String::new()
    };
    if n.dir {
        format!("{w}{extra}-{}", n.seed % 997)
    } else {
        format!(
            "{w}{extra}-{}.{}",
            n.seed % 997,
            EXTS[(n.seed >> 20) as usize % EXTS.len()]
        )
    }
}

pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1000.0 && u < UNITS.len() - 1 {
        v /= 1000.0;
        u += 1;
    }
    if u == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

fn children(node: &Node, max_depth: u8) -> Vec<Node> {
    let mut rng = Rng::new(node.seed);
    let n = 20 + (rng.next_u64() % 100) as usize;
    // Folders carry more bytes than files on average, and sizes are spread
    // log-normally, which gives the long tail of small boxes a real tree has.
    let picks: Vec<(bool, f32)> = (0..n)
        .map(|_| {
            let dir = node.depth < max_depth && rng.unit() < 0.6;
            let w = (0.95 * rng.gauss()).exp() * if dir { 4.0 } else { 1.0 };
            (dir, w)
        })
        .collect();
    let total: f32 = picks.iter().map(|p| p.1).sum();
    picks
        .iter()
        .enumerate()
        .map(|(i, (dir, w))| {
            let size = ((node.size as f64) * (*w / total) as f64).max(1.0) as u64;
            let seed = rng.next_u64() ^ (i as u64).wrapping_mul(0x2545_F491_4F6C_DD1D);
            Node {
                seed,
                size,
                depth: node.depth + 1,
                dir: *dir && size > 4096,
            }
        })
        .collect()
}

/// Splits `rect` into one rectangle per area (areas in descending order,
/// summing to the rectangle's area) with the squarified algorithm.
pub fn squarify(areas: &[f32], rect: Rect) -> Vec<Rect> {
    let mut out = Vec::with_capacity(areas.len());
    let mut r = rect;
    let mut i = 0;
    while i < areas.len() {
        let short = r.w.min(r.h).max(1e-3);
        let mut j = i + 1;
        let mut sum = areas[i];
        let mut best = worst_ratio(&areas[i..j], sum, short);
        while j < areas.len() {
            let s2 = sum + areas[j];
            let w = worst_ratio(&areas[i..=j], s2, short);
            if w > best {
                break;
            }
            best = w;
            sum = s2;
            j += 1;
        }
        // Lay the row along the short side.
        if r.w >= r.h {
            let col_w = (sum / r.h.max(1e-3)).min(r.w);
            let mut y = r.y;
            for a in &areas[i..j] {
                let h = if sum > 0.0 { r.h * a / sum } else { 0.0 };
                out.push(Rect::new(r.x, y, col_w, h));
                y += h;
            }
            r = Rect::new(r.x + col_w, r.y, (r.w - col_w).max(0.0), r.h);
        } else {
            let row_h = (sum / r.w.max(1e-3)).min(r.h);
            let mut x = r.x;
            for a in &areas[i..j] {
                let w = if sum > 0.0 { r.w * a / sum } else { 0.0 };
                out.push(Rect::new(x, r.y, w, row_h));
                x += w;
            }
            r = Rect::new(r.x, r.y + row_h, r.w, (r.h - row_h).max(0.0));
        }
        i = j;
    }
    out
}

fn worst_ratio(row: &[f32], sum: f32, side: f32) -> f32 {
    let max = row.iter().cloned().fold(0.0, f32::max);
    let min = row.iter().cloned().fold(f32::MAX, f32::min);
    let s2 = side * side;
    ((s2 * max) / (sum * sum)).max((sum * sum) / (s2 * min.max(1e-6)))
}

/// Tunable layout parameters.
#[derive(Clone, Copy, Debug)]
pub struct LayoutParams {
    pub scale: f32,
    pub em: f32,
    /// Boxes smaller than this many square device pixels merge into a group
    /// or are not drawn.
    pub min_area: f32,
}

struct Ctx {
    p: LayoutParams,
    max_depth: u8,
    items: Vec<Item>,
    folders: Vec<(u32, Node)>,
    next_id: u32,
    labels: bool,
}

impl Ctx {
    fn title_h(&self) -> f32 {
        (self.p.em * 1.6).round()
    }

    fn label_for(&self, node: &Node, r: Rect, text_size: bool) -> Option<Box<ItemLabel>> {
        if !self.labels || r.w < 3.5 * self.p.em || r.h < 1.6 * self.p.em {
            return None;
        }
        Some(Box::new(ItemLabel {
            name: node_name(node).into(),
            size: if text_size {
                format_size(node.size).into()
            } else {
                "".into()
            },
        }))
    }

    fn place(&mut self, node: &Node, rect: Rect, depth: u8) {
        let id = self.next_id;
        self.next_id += 1;
        let em = self.p.em;
        if !node.dir {
            let category = (node.seed % 9) as u8;
            let label = self.label_for(node, rect, false);
            self.items.push(Item {
                rect,
                kind: ItemKind::FileTile,
                depth,
                label,
                id,
                category,
                flags: Flags::default(),
            });
            return;
        }
        let nest = rect.w >= 5.0 * em && rect.h >= 3.2 * em;
        let kind = if nest {
            ItemKind::FolderTitled
        } else {
            ItemKind::FolderBlock
        };
        let label = self.label_for(node, rect, true);
        self.items.push(Item {
            rect,
            kind,
            depth,
            label,
            id,
            category: 0,
            flags: Flags::default(),
        });
        if depth == 1 {
            self.folders.push((id, *node));
        }
        if !nest {
            return;
        }
        let pad = (2.0 * self.p.scale).round() + 1.0;
        let inner = Rect::new(
            rect.x + pad,
            rect.y + pad + self.title_h() - 1.0,
            rect.w - 2.0 * pad,
            rect.h - 2.0 * pad - self.title_h() + 1.0,
        );
        if inner.w < 2.0 || inner.h < 2.0 {
            return;
        }
        let mut kids = children(node, self.max_depth);
        kids.sort_by_key(|k| std::cmp::Reverse(k.size));
        let total: f64 = kids.iter().map(|k| k.size as f64).sum();
        let area = inner.w * inner.h;
        let proj = |k: &Node| (k.size as f64 / total) as f32 * area;
        // Merge the tail of tiny siblings into one group.
        let keep = kids
            .iter()
            .take_while(|k| proj(k) >= self.p.min_area)
            .count();
        let tail = &kids[keep..];
        let mut areas: Vec<f32> = kids[..keep].iter().map(proj).collect();
        let group_size: u64 = tail.iter().map(|k| k.size).sum();
        let make_group = tail.len() >= 3 && proj_sum(tail, total, area) >= self.p.min_area;
        if make_group {
            areas.push(proj_sum(tail, total, area));
        } else if !tail.is_empty() {
            // Not enough tiny siblings to group: their area stays empty.
        }
        let kept: f32 = areas.iter().sum();
        if kept <= 0.0 {
            return;
        }
        // Normalize so the laid-out rows fill the box.
        let scale = area / kept;
        let areas: Vec<f32> = areas.iter().map(|a| a * scale).collect();
        let rects = squarify(&areas, inner);
        for (i, r) in rects.iter().enumerate() {
            let r = Rect::new(r.x, r.y, r.w, r.h);
            if make_group && i == keep {
                let gid = self.next_id;
                self.next_id += 1;
                let g = Node {
                    seed: kids[keep].seed,
                    size: group_size,
                    depth: node.depth + 1,
                    dir: false,
                };
                self.items.push(Item {
                    rect: r,
                    kind: ItemKind::Group,
                    depth: depth + 1,
                    label: self
                        .labels
                        .then(|| {
                            Box::new(ItemLabel {
                                name: format!("{} small files", tail.len()).into(),
                                size: format_size(g.size).into(),
                            })
                        })
                        .filter(|_| r.w >= 3.5 * em && r.h >= 1.6 * em),
                    id: gid,
                    category: 8,
                    flags: Flags::default(),
                });
            } else if r.w >= 2.0 && r.h >= 2.0 {
                self.place(&kids[i], r, depth + 1);
            }
        }
    }
}

fn proj_sum(tail: &[Node], total: f64, area: f32) -> f32 {
    tail.iter()
        .map(|k| (k.size as f64 / total) as f32 * area)
        .sum()
}

/// A synthetic tree whose views can be laid out and entered.
pub struct SynthSource {
    max_depth: u8,
    pub params: LayoutParams,
    views: Vec<(Node, Option<ViewId>)>,
    by_seed: HashMap<u64, ViewId>,
    cache: HashMap<(ViewId, u32, u32), Layout>,
    pub labels: bool,
}

impl SynthSource {
    pub fn new(seed: u64, params: LayoutParams) -> Self {
        let root = Node {
            seed,
            size: 2_000_000_000_000,
            depth: 0,
            dir: true,
        };
        let mut by_seed = HashMap::new();
        by_seed.insert(root.seed, 0);
        Self {
            max_depth: 12,
            params,
            views: vec![(root, None)],
            by_seed,
            cache: HashMap::new(),
            labels: true,
        }
    }

    pub fn set_min_area(&mut self, min_area: f32) {
        self.params.min_area = min_area;
        self.cache.clear();
    }

    pub fn set_geometry(&mut self, scale: f32, em: f32) {
        self.params.scale = scale;
        self.params.em = em;
        self.cache.clear();
    }

    /// Lays out `view` without caching, returning the items and the folders
    /// one level down.
    fn layout_uncached(&mut self, view: ViewId, map: Rect) -> Layout {
        let (node, _) = self.views[view as usize];
        // A view's root is shown as depth 0 with its children at depth 1.
        let mut ctx = Ctx {
            p: self.params,
            max_depth: self.max_depth,
            items: Vec::with_capacity(32_000),
            folders: Vec::new(),
            next_id: 1,
            labels: self.labels,
        };
        let root = Node { depth: 0, ..node };
        ctx.place(&root, map, 0);
        let mut folders = Vec::new();
        for (id, n) in ctx.folders {
            let child = Node { depth: 0, ..n };
            let vid = *self.by_seed.entry(n.seed).or_insert_with(|| {
                self.views.push((child, Some(view)));
                (self.views.len() - 1) as ViewId
            });
            folders.push((id, vid));
        }
        Layout {
            items: Arc::new(ctx.items),
            folders,
        }
    }

    /// The number of boxes a layout of the root view draws.
    pub fn count_root(&mut self, map: Rect) -> usize {
        let was = self.labels;
        self.labels = false;
        let n = self.layout_uncached(0, map).items.len();
        self.labels = was;
        n
    }

    /// Finds the smallest `min_area` whose root layout draws about `target`
    /// boxes (within 5%), by bisection. Returns the area and the count.
    pub fn calibrate(&mut self, map: Rect, target: usize) -> (f32, usize) {
        let (mut lo, mut hi) = (4.0_f32, 20_000.0_f32);
        let mut best = (lo, 0);
        for _ in 0..24 {
            let mid = (lo * hi).sqrt();
            self.params.min_area = mid;
            let n = self.count_root(map);
            best = (mid, n);
            let off = (n as f64 - target as f64).abs() / target as f64;
            if off <= 0.02 {
                break;
            }
            if n > target {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        self.cache.clear();
        (best.0, best.1)
    }

    /// The biggest folder directly below the root view, as `(item id, view)`.
    pub fn biggest_child(&mut self, map: Rect) -> Option<(u32, ViewId)> {
        let layout = self.layout(0, map);
        layout
            .folders
            .iter()
            .filter_map(|(id, v)| {
                let it = layout.items.iter().find(|i| i.id == *id)?;
                Some((it.rect.w * it.rect.h, *id, *v))
            })
            .max_by(|a, b| a.0.total_cmp(&b.0))
            .map(|(_, id, v)| (id, v))
    }
}

impl ViewSource for SynthSource {
    fn root(&self) -> ViewId {
        0
    }

    fn layout(&mut self, view: ViewId, map: Rect) -> Layout {
        let key = (view, map.w as u32, map.h as u32);
        if let Some(hit) = self.cache.get(&key) {
            return hit.clone();
        }
        let layout = self.layout_uncached(view, map);
        self.cache.insert(key, layout.clone());
        layout
    }

    fn parent(&self, view: ViewId) -> Option<ViewId> {
        self.views.get(view as usize).and_then(|v| v.1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> LayoutParams {
        LayoutParams {
            scale: 1.0,
            em: 13.0,
            min_area: 100.0,
        }
    }

    #[test]
    fn squarify_fills_the_rectangle() {
        let areas = [600.0, 300.0, 200.0, 100.0, 100.0];
        let total: f32 = areas.iter().sum();
        let rect = Rect::new(0.0, 0.0, 50.0, total / 50.0);
        let rects = squarify(&areas, rect);
        assert_eq!(rects.len(), areas.len());
        let covered: f32 = rects.iter().map(|r| r.w * r.h).sum();
        assert!(
            (covered - total).abs() / total < 0.001,
            "{covered} vs {total}"
        );
        for r in &rects {
            assert!(
                r.x >= -0.01
                    && r.y >= -0.01
                    && r.right() <= 50.01
                    && r.bottom() <= rect.bottom() + 0.01
            );
        }
    }

    #[test]
    fn layout_is_deterministic() {
        let map = Rect::new(0.0, 0.0, 800.0, 600.0);
        let a = SynthSource::new(7, params()).layout_uncached(0, map);
        let b = SynthSource::new(7, params()).layout_uncached(0, map);
        assert_eq!(*a.items, *b.items);
        let c = SynthSource::new(8, params()).layout_uncached(0, map);
        assert_ne!(*a.items, *c.items);
    }

    #[test]
    fn items_stay_inside_the_map_and_parents_come_first() {
        let map = Rect::new(0.0, 28.0, 800.0, 600.0);
        let l = SynthSource::new(3, params()).layout_uncached(0, map);
        assert!(l.items.len() > 50);
        for it in l.items.iter() {
            assert!(it.rect.x >= map.x - 0.5 && it.rect.right() <= map.right() + 0.5);
            assert!(it.rect.y >= map.y - 0.5 && it.rect.bottom() <= map.bottom() + 0.5);
        }
        assert_eq!(l.items[0].depth, 0);
        assert!(!l.folders.is_empty());
    }

    #[test]
    fn calibration_hits_the_target_box_count() {
        let map = Rect::new(0.0, 0.0, 1280.0, 720.0);
        let mut s = SynthSource::new(11, params());
        let (_, n) = s.calibrate(map, 2_000);
        assert!((1_900..=2_100).contains(&n), "{n}");
    }

    #[test]
    fn views_have_parents() {
        let map = Rect::new(0.0, 0.0, 800.0, 600.0);
        let mut s = SynthSource::new(5, params());
        let (_, view) = s.biggest_child(map).expect("a folder");
        assert_eq!(s.parent(view), Some(0));
        assert_eq!(s.parent(0), None);
    }

    #[test]
    fn sizes_format_with_units() {
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(12_300_000_000), "12.3 GB");
    }
}
