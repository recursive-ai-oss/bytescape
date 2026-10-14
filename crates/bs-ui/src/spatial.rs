//! Moving the selection with the arrow keys: to the nearest box in that
//! direction among the boxes at the same nesting depth.
//!
//! Staying at one depth keeps the keys predictable: the arrows walk across
//! the siblings and cousins on screen, Enter goes into a folder and Backspace
//! comes back out. "Nearest" favors boxes that sit straight ahead over boxes
//! that are merely closer along the axis.

use bs_render::{Item, Rect};

/// An arrow key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

/// How much a box that is off to the side costs, per pixel of gap and per
/// pixel of offset between centers.
const GAP_WEIGHT: f32 = 3.0;
const OFFSET_WEIGHT: f32 = 0.5;

/// The id of the box to select when `dir` is pressed with `from` selected, or
/// `None` when nothing lies that way.
pub fn neighbor(items: &[Item], from: &Item, dir: Direction) -> Option<u32> {
    let a = from.rect;
    let (acx, acy) = center(a);
    let mut best: Option<(f32, u32)> = None;
    for it in items {
        if it.id == from.id || it.depth != from.depth || it.rect.w < 1.0 || it.rect.h < 1.0 {
            continue;
        }
        let b = it.rect;
        let (bcx, bcy) = center(b);
        let ahead = match dir {
            Direction::Right => bcx > acx,
            Direction::Left => bcx < acx,
            Direction::Down => bcy > acy,
            Direction::Up => bcy < acy,
        };
        if !ahead {
            continue;
        }
        let (along, across_gap, across_offset) = match dir {
            Direction::Right => (
                b.x - a.right(),
                gap(a.y, a.bottom(), b.y, b.bottom()),
                bcy - acy,
            ),
            Direction::Left => (
                a.x - b.right(),
                gap(a.y, a.bottom(), b.y, b.bottom()),
                bcy - acy,
            ),
            Direction::Down => (
                b.y - a.bottom(),
                gap(a.x, a.right(), b.x, b.right()),
                bcx - acx,
            ),
            Direction::Up => (
                a.y - b.bottom(),
                gap(a.x, a.right(), b.x, b.right()),
                bcx - acx,
            ),
        };
        let score = along.max(0.0) + GAP_WEIGHT * across_gap + OFFSET_WEIGHT * across_offset.abs();
        if best.is_none_or(|(s, id)| score < s || (score == s && it.id < id)) {
            best = Some((score, it.id));
        }
    }
    best.map(|(_, id)| id)
}

/// The box to select when an arrow is pressed with nothing selected: the
/// first one directly under the view's own box, which is the largest.
pub fn first(items: &[Item]) -> Option<u32> {
    items.iter().find(|i| i.depth == 1).map(|i| i.id)
}

fn center(r: Rect) -> (f32, f32) {
    (r.x + r.w / 2.0, r.y + r.h / 2.0)
}

/// The distance between two intervals, zero when they overlap.
fn gap(a0: f32, a1: f32, b0: f32, b1: f32) -> f32 {
    (b0 - a1).max(a0 - b1).max(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bs_render::{Flags, ItemKind};

    fn item(id: u32, depth: u8, x: f32, y: f32, w: f32, h: f32) -> Item {
        Item {
            rect: Rect::new(x, y, w, h),
            kind: ItemKind::FileTile,
            depth,
            label: None,
            id,
            category: 0,
            flags: Flags::default(),
        }
    }

    /// Three columns, two rows of 100 × 100 boxes, 10 apart; the root box
    /// (depth 0) surrounds them.
    ///
    /// ```text
    ///  1 2 3
    ///  4 5 6
    /// ```
    fn grid() -> Vec<Item> {
        let mut v = vec![item(100, 0, 0.0, 0.0, 340.0, 230.0)];
        for row in 0..2u32 {
            for col in 0..3u32 {
                v.push(item(
                    1 + row * 3 + col,
                    1,
                    10.0 + col as f32 * 110.0,
                    10.0 + row as f32 * 110.0,
                    100.0,
                    100.0,
                ));
            }
        }
        v
    }

    fn go(items: &[Item], from: u32, dir: Direction) -> Option<u32> {
        let f = items.iter().find(|i| i.id == from).expect("from");
        neighbor(items, f, dir)
    }

    #[test]
    fn arrows_walk_a_grid() {
        let g = grid();
        assert_eq!(go(&g, 1, Direction::Right), Some(2));
        assert_eq!(go(&g, 2, Direction::Right), Some(3));
        assert_eq!(go(&g, 5, Direction::Left), Some(4));
        assert_eq!(go(&g, 2, Direction::Down), Some(5));
        assert_eq!(go(&g, 6, Direction::Up), Some(3));
    }

    #[test]
    fn nothing_beyond_the_edge_means_no_move() {
        let g = grid();
        assert_eq!(go(&g, 3, Direction::Right), None);
        assert_eq!(go(&g, 1, Direction::Left), None);
        assert_eq!(go(&g, 2, Direction::Up), None);
        assert_eq!(go(&g, 5, Direction::Down), None);
    }

    #[test]
    fn the_root_box_is_never_a_target() {
        let g = grid();
        for from in 1..=6 {
            for dir in [
                Direction::Left,
                Direction::Right,
                Direction::Up,
                Direction::Down,
            ] {
                assert_ne!(go(&g, from, dir), Some(100));
            }
        }
    }

    #[test]
    fn a_box_straight_ahead_beats_a_nearer_box_off_to_the_side() {
        // From the wide box A: B is straight below but far; C is closer along
        // the axis yet sits well off to the right.
        let items = vec![
            item(1, 1, 0.0, 0.0, 200.0, 50.0),
            item(2, 1, 20.0, 150.0, 100.0, 50.0),
            item(3, 1, 400.0, 60.0, 100.0, 50.0),
        ];
        assert_eq!(go(&items, 1, Direction::Down), Some(2));
    }

    #[test]
    fn different_depths_are_skipped() {
        // A folder's file sits right next to the selected folder, but is one
        // level deeper.
        let items = vec![
            item(1, 1, 0.0, 0.0, 100.0, 100.0),
            item(2, 2, 110.0, 10.0, 20.0, 20.0),
            item(3, 1, 300.0, 0.0, 100.0, 100.0),
        ];
        assert_eq!(go(&items, 1, Direction::Right), Some(3));
    }

    #[test]
    fn cousins_in_other_folders_are_reached() {
        // Two folders (depth 1), each holding two files (depth 2).
        let items = vec![
            item(1, 1, 0.0, 0.0, 100.0, 100.0),
            item(2, 2, 10.0, 30.0, 40.0, 60.0),
            item(3, 2, 55.0, 30.0, 40.0, 60.0),
            item(4, 1, 110.0, 0.0, 100.0, 100.0),
            item(5, 2, 120.0, 30.0, 40.0, 60.0),
            item(6, 2, 165.0, 30.0, 40.0, 60.0),
        ];
        assert_eq!(go(&items, 3, Direction::Right), Some(5));
        assert_eq!(go(&items, 5, Direction::Left), Some(3));
    }

    #[test]
    fn ties_go_to_the_lower_id() {
        let items = vec![
            item(1, 1, 100.0, 100.0, 50.0, 50.0),
            item(7, 1, 0.0, 0.0, 50.0, 50.0),
            item(5, 1, 200.0, 0.0, 50.0, 50.0),
        ];
        assert_eq!(go(&items, 1, Direction::Up), Some(5));
    }

    #[test]
    fn with_nothing_selected_the_first_top_level_box_is_chosen() {
        let g = grid();
        assert_eq!(first(&g), Some(1));
        assert_eq!(first(&[item(1, 0, 0.0, 0.0, 10.0, 10.0)]), None);
    }
}
