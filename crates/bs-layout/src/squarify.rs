//! The squarified treemap algorithm (Bruls, Huizing, van Wijk), working on
//! exact edges so that neighbours share them.

/// The four edges of a rectangle: left, top, right, bottom.
pub(crate) type Edges = [f64; 4];

/// Splits the rectangle `[x0, x1) × [y0, y1)` into one rectangle per entry
/// of `areas`, which must be positive and in descending order and should sum
/// to the rectangle's area. Appends to `out`, in the order of `areas`.
///
/// Neighbouring rectangles share the very same edge values, and the last
/// rectangle of each row and the last row end exactly on the outer edges, so
/// rounding the edges to whole pixels can neither open gaps nor create
/// overlaps.
pub(crate) fn squarify(areas: &[f64], x0: f64, y0: f64, x1: f64, y1: f64, out: &mut Vec<Edges>) {
    let n = areas.len();
    out.reserve(n);
    let (mut cx0, mut cy0) = (x0, y0);
    let mut i = 0;
    while i < n {
        let w = (x1 - cx0).max(0.0);
        let h = (y1 - cy0).max(0.0);
        let side = w.min(h);
        // Grow the row while the worst aspect ratio does not get worse. The
        // areas are descending, so the first entry is the row's largest and
        // the newest the smallest.
        let first = areas[i];
        let mut sum = first;
        let mut worst = worst_ratio(first, first, sum, side);
        let mut j = i + 1;
        while j < n {
            let a = areas[j];
            let grown = sum + a;
            let ratio = worst_ratio(first, a, grown, side);
            if ratio > worst {
                break;
            }
            worst = ratio;
            sum = grown;
            j += 1;
        }
        let last_row = j == n;
        if w >= h {
            // A column on the left, its entries stacked top to bottom.
            let nx = if last_row || h <= 0.0 {
                x1
            } else {
                (cx0 + sum / h).min(x1)
            };
            let mut cum = 0.0;
            let mut y = cy0;
            for (k, &a) in areas[i..j].iter().enumerate() {
                cum += a;
                let ny = if i + k + 1 == j {
                    y1
                } else {
                    (cy0 + h * cum / sum).clamp(y, y1)
                };
                out.push([cx0, y, nx, ny]);
                y = ny;
            }
            cx0 = nx;
        } else {
            // A row at the top, its entries side by side.
            let ny = if last_row || w <= 0.0 {
                y1
            } else {
                (cy0 + sum / w).min(y1)
            };
            let mut cum = 0.0;
            let mut x = cx0;
            for (k, &a) in areas[i..j].iter().enumerate() {
                cum += a;
                let nx = if i + k + 1 == j {
                    x1
                } else {
                    (cx0 + w * cum / sum).clamp(x, x1)
                };
                out.push([x, cy0, nx, ny]);
                x = nx;
            }
            cy0 = ny;
        }
        i = j;
    }
}

/// The worst aspect ratio of a row whose largest entry is `max`, smallest is
/// `min` and total is `sum`, laid along a side of length `side`.
fn worst_ratio(max: f64, min: f64, sum: f64, side: f64) -> f64 {
    let s2 = side * side;
    let sum2 = sum * sum;
    let a = s2 * max / sum2;
    let b = sum2 / (s2 * min.max(f64::MIN_POSITIVE));
    if a.is_nan() || b.is_nan() {
        // A zero side: every row is equally bad, so keep growing it.
        return f64::INFINITY;
    }
    a.max(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(areas: &[f64], w: f64, h: f64) -> Vec<Edges> {
        let mut out = Vec::new();
        squarify(areas, 0.0, 0.0, w, h, &mut out);
        out
    }

    #[test]
    fn one_entry_fills_the_rectangle() {
        assert_eq!(run(&[50.0], 10.0, 5.0), vec![[0.0, 0.0, 10.0, 5.0]]);
    }

    #[test]
    fn rectangles_tile_the_area_exactly() {
        let areas = [30.0, 20.0, 20.0, 10.0, 10.0, 5.0, 5.0];
        let out = run(&areas, 10.0, 10.0);
        assert_eq!(out.len(), areas.len());
        let total: f64 = out.iter().map(|e| (e[2] - e[0]) * (e[3] - e[1])).sum();
        assert!((total - 100.0).abs() < 1e-9);
        for (e, a) in out.iter().zip(areas) {
            assert!(((e[2] - e[0]) * (e[3] - e[1]) - a).abs() < 1e-9);
        }
    }

    fn worst_aspect(rects: &[Edges]) -> f64 {
        rects
            .iter()
            .map(|e| {
                let (w, h) = (e[2] - e[0], e[3] - e[1]);
                (w / h).max(h / w)
            })
            .fold(1.0, f64::max)
    }

    #[test]
    fn squarified_rows_are_squarer_than_one_slice_per_entry() {
        // The example from the original paper: 6 x 4 split into seven areas.
        let areas = [6.0, 6.0, 4.0, 3.0, 2.0, 2.0, 1.0];
        let out = run(&areas, 6.0, 4.0);
        // Slicing the long side once per entry gives sliver-shaped boxes.
        let mut slices = Vec::new();
        let mut x = 0.0;
        for a in areas {
            slices.push([x, 0.0, x + a / 4.0, 4.0]);
            x += a / 4.0;
        }
        // The last, smallest box is the worst one: 0.6 x 1.67.
        assert!(worst_aspect(&out) < 3.0, "{}", worst_aspect(&out));
        assert!(worst_aspect(&out) < worst_aspect(&slices) / 4.0);
    }

    #[test]
    fn a_degenerate_rectangle_gives_empty_rectangles_not_nan() {
        for (w, h) in [(0.0, 10.0), (10.0, 0.0), (0.0, 0.0)] {
            let out = run(&[3.0, 2.0, 1.0], w, h);
            assert_eq!(out.len(), 3);
            assert!(out.iter().flatten().all(|v| v.is_finite()));
        }
    }
}
