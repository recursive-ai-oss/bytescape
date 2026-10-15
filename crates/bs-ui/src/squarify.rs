//! A small squarified treemap for the few blocks the interface places itself:
//! drive cards and the special blocks of a drive view. Folder contents are laid
//! out by the layout crate; this is only for handfuls of boxes.

use bs_render::Rect;

/// Splits `area` into one rectangle per weight, in the order of `weights`.
///
/// Areas follow the weights. Neighbours share their edges, which are rounded
/// to whole pixels, so there are no gaps and no overlaps. Weights that are not
/// positive get an empty rectangle.
pub fn squarify(weights: &[f64], area: Rect) -> Vec<Rect> {
    let mut order: Vec<usize> = (0..weights.len()).filter(|&i| weights[i] > 0.0).collect();
    order.sort_by(|&a, &b| weights[b].total_cmp(&weights[a]).then(a.cmp(&b)));
    let mut out = vec![Rect::new(area.x, area.y, 0.0, 0.0); weights.len()];
    let total: f64 = order.iter().map(|&i| weights[i]).sum();
    if order.is_empty() || total <= 0.0 || area.w <= 0.0 || area.h <= 0.0 {
        return out;
    }
    let scale = f64::from(area.w) * f64::from(area.h) / total;
    let areas: Vec<f64> = order.iter().map(|&i| weights[i] * scale).collect();
    let (mut x0, mut y0) = (f64::from(area.x), f64::from(area.y));
    let (x1, y1) = (f64::from(area.right()), f64::from(area.bottom()));
    let mut i = 0;
    while i < areas.len() {
        let (w, h) = ((x1 - x0).max(0.0), (y1 - y0).max(0.0));
        let side = w.min(h).max(1e-9);
        let worst = |row: &[f64]| -> f64 {
            let sum: f64 = row.iter().sum();
            let (lo, hi) = row
                .iter()
                .fold((f64::MAX, 0.0f64), |(lo, hi), &a| (lo.min(a), hi.max(a)));
            let s2 = side * side;
            ((s2 * hi) / (sum * sum)).max((sum * sum) / (s2 * lo.max(1e-9)))
        };
        let mut j = i + 1;
        while j < areas.len() && worst(&areas[i..=j]) <= worst(&areas[i..j]) {
            j += 1;
        }
        let last = j == areas.len();
        let sum: f64 = areas[i..j].iter().sum();
        let mut edges: Vec<[f64; 4]> = Vec::new();
        if w >= h {
            let nx = if last {
                x1
            } else {
                (x0 + sum / h.max(1e-9)).min(x1)
            };
            let mut y = y0;
            let mut cum = 0.0;
            for (k, a) in areas[i..j].iter().enumerate() {
                cum += a;
                let ny = if i + k + 1 == j {
                    y1
                } else {
                    y0 + h * cum / sum
                };
                edges.push([x0, y, nx, ny]);
                y = ny;
            }
            x0 = nx;
        } else {
            let ny = if last {
                y1
            } else {
                (y0 + sum / w.max(1e-9)).min(y1)
            };
            let mut x = x0;
            let mut cum = 0.0;
            for (k, a) in areas[i..j].iter().enumerate() {
                cum += a;
                let nx = if i + k + 1 == j {
                    x1
                } else {
                    x0 + w * cum / sum
                };
                edges.push([x, y0, nx, ny]);
                x = nx;
            }
            y0 = ny;
        }
        for (k, e) in edges.into_iter().enumerate() {
            let r = |v: f64| v.round() as f32;
            let (l, t, rt, b) = (r(e[0]), r(e[1]), r(e[2]), r(e[3]));
            out[order[i + k]] = Rect::new(l, t, (rt - l).max(0.0), (b - t).max(0.0));
        }
        i = j;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rectangles_tile_the_area_without_gaps_or_overlaps() {
        let area = Rect::new(10.0, 20.0, 300.0, 200.0);
        let rects = squarify(&[50.0, 30.0, 10.0, 5.0, 5.0], area);
        let covered: f32 = rects.iter().map(|r| r.w * r.h).sum();
        assert!((covered - area.w * area.h).abs() < 1.0, "{covered}");
        for (i, a) in rects.iter().enumerate() {
            assert!(a.x >= area.x && a.right() <= area.right() + 0.5);
            for b in &rects[i + 1..] {
                let ox = (a.right().min(b.right()) - a.x.max(b.x)).max(0.0);
                let oy = (a.bottom().min(b.bottom()) - a.y.max(b.y)).max(0.0);
                assert!(ox * oy < 0.5, "{a:?} overlaps {b:?}");
            }
        }
    }

    #[test]
    fn bigger_weights_get_bigger_areas_and_order_is_kept() {
        let rects = squarify(&[1.0, 8.0, 3.0], Rect::new(0.0, 0.0, 400.0, 300.0));
        let area = |r: &Rect| r.w * r.h;
        assert!(area(&rects[1]) > area(&rects[2]));
        assert!(area(&rects[2]) > area(&rects[0]));
    }

    #[test]
    fn empty_and_zero_weights_are_harmless() {
        assert!(squarify(&[], Rect::new(0.0, 0.0, 10.0, 10.0)).is_empty());
        let r = squarify(&[0.0, 5.0], Rect::new(0.0, 0.0, 10.0, 10.0));
        assert_eq!(r[0].w * r[0].h, 0.0);
        assert_eq!((r[1].w, r[1].h), (10.0, 10.0));
    }
}
