//! Parallel work over horizontal bands, and the zoom compositor.

use std::sync::Mutex;

use crate::geom::Rect;

/// Runs `work(first_row, band)` over disjoint horizontal bands of `data`
/// (rows of `row_len` elements) on up to `threads` threads. Bands are handed
/// out as threads become free, so uneven bands balance themselves.
pub fn parallel_bands<T, F>(
    data: &mut [T],
    row_len: usize,
    band_rows: usize,
    threads: usize,
    work: F,
) where
    T: Send,
    F: Fn(usize, &mut [T]) + Sync,
{
    if row_len == 0 || band_rows == 0 {
        return;
    }
    let bands: Vec<(usize, &mut [T])> = data
        .chunks_mut(row_len * band_rows)
        .enumerate()
        .map(|(i, b)| (i * band_rows, b))
        .collect();
    let threads = threads.clamp(1, bands.len().max(1));
    if threads == 1 {
        for (row, band) in bands {
            work(row, band);
        }
        return;
    }
    let queue = Mutex::new(bands.into_iter());
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| {
                loop {
                    let next = queue.lock().unwrap_or_else(|e| e.into_inner()).next();
                    match next {
                        Some((row, band)) => work(row, band),
                        None => break,
                    }
                }
            });
        }
    });
}

/// Where the two images of a zoom sit on screen for one frame.
#[derive(Clone, Copy, Debug)]
pub struct ZoomFrame {
    /// Region of the large image (the parent view) that fills the screen.
    pub big_src: Rect,
    /// Where the small image (the child view) lands on screen.
    pub small_dst: Rect,
    /// Opacity of the small image over the large one, `0.0..=1.0`.
    pub small_alpha: f32,
}

/// The two views' geometry at progress `p` of a zoom in (`0.0` shows the
/// parent view as it is, `1.0` shows the child view full screen).
/// `child` is the child's box inside the parent view.
pub fn zoom_geometry(width: f32, height: f32, child: Rect, p: f32) -> ZoomFrame {
    let full = Rect::new(0.0, 0.0, width, height);
    let lerp = |a: f32, b: f32| a + (b - a) * p;
    let big_src = Rect::new(
        lerp(full.x, child.x),
        lerp(full.y, child.y),
        lerp(full.w, child.w),
        lerp(full.h, child.h),
    );
    let sx = width / big_src.w;
    let sy = height / big_src.h;
    let small_dst = Rect::new(
        (child.x - big_src.x) * sx,
        (child.y - big_src.y) * sy,
        child.w * sx,
        child.h * sy,
    );
    ZoomFrame {
        big_src,
        small_dst,
        small_alpha: p,
    }
}

/// Composes one zoom frame into `out` (`width × height` native pixel words).
/// Images are sampled with nearest-neighbour steps: the frame lasts a
/// fraction of a second, and the full-quality view replaces it at the end.
pub fn compose_zoom(
    out: &mut [u32],
    width: usize,
    height: usize,
    big: &[u8],
    small: &[u8],
    frame: &ZoomFrame,
    threads: usize,
) {
    if width == 0 || height == 0 || out.len() < width * height {
        return;
    }
    let xmap: Vec<u32> = (0..width)
        .map(|x| {
            let sx = frame.big_src.x + (x as f32 + 0.5) * frame.big_src.w / width as f32;
            (sx.max(0.0) as usize).min(width - 1) as u32
        })
        .collect();
    let dst = frame.small_dst;
    let dx0 = (dst.x.round().max(0.0) as usize).min(width);
    let dx1 = (dst.right().round().max(0.0) as usize).min(width);
    let dy0 = (dst.y.round().max(0.0) as usize).min(height);
    let dy1 = (dst.bottom().round().max(0.0) as usize).min(height);
    let smap: Vec<u32> = (dx0..dx1)
        .map(|x| {
            let sx = (x as f32 + 0.5 - dst.x) / dst.w * width as f32;
            (sx.max(0.0) as usize).min(width - 1) as u32
        })
        .collect();
    let a = ((frame.small_alpha.clamp(0.0, 1.0)) * 256.0).round() as u32;
    let band_rows = (height / (threads.max(1) * 4)).max(8);
    parallel_bands(
        &mut out[..width * height],
        width,
        band_rows,
        threads,
        |row0, band| {
            for (i, line) in band.chunks_exact_mut(width).enumerate() {
                let y = row0 + i;
                let sy = frame.big_src.y + (y as f32 + 0.5) * frame.big_src.h / height as f32;
                let big_row =
                    &big[(sy.max(0.0) as usize).min(height - 1) * width * 4..][..width * 4];
                for (px, &m) in line.iter_mut().zip(&xmap) {
                    *px = word_at(big_row, m as usize);
                }
                if a == 0 || y < dy0 || y >= dy1 || dx0 >= dx1 {
                    continue;
                }
                let sy = (y as f32 + 0.5 - dst.y) / dst.h * height as f32;
                let small_row =
                    &small[(sy.max(0.0) as usize).min(height - 1) * width * 4..][..width * 4];
                for (px, &m) in line[dx0..dx1].iter_mut().zip(&smap) {
                    *px = blend(*px, word_at(small_row, m as usize), a);
                }
            }
        },
    );
}

#[inline]
fn word_at(row: &[u8], x: usize) -> u32 {
    let p = &row[x * 4..x * 4 + 4];
    u32::from_le_bytes([p[0], p[1], p[2], p[3]])
}

/// `a·(1 − t) + b·t` for `t = alpha / 256`, on opaque native pixel words.
#[inline]
fn blend(a: u32, b: u32, alpha: u32) -> u32 {
    if alpha >= 256 {
        return b;
    }
    let inv = 256 - alpha;
    let rb = ((a & 0x00FF_00FF) * inv + (b & 0x00FF_00FF) * alpha) >> 8;
    let g = (((a >> 8) & 0xFF) * inv + ((b >> 8) & 0xFF) * alpha) >> 8;
    0xFF00_0000 | (rb & 0x00FF_00FF) | ((g & 0xFF) << 8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bands_cover_every_row_once() {
        let mut data = vec![0u32; 10 * 37];
        parallel_bands(&mut data, 10, 5, 4, |row0, band| {
            for (i, row) in band.as_chunks_mut::<10>().0.iter_mut().enumerate() {
                for v in row {
                    *v += (row0 + i) as u32 + 1;
                }
            }
        });
        for (y, row) in data.as_chunks::<10>().0.iter().enumerate() {
            assert!(row.iter().all(|&v| v == y as u32 + 1), "row {y}");
        }
    }

    #[test]
    fn geometry_endpoints() {
        let child = Rect::new(100.0, 50.0, 200.0, 100.0);
        let start = zoom_geometry(400.0, 200.0, child, 0.0);
        assert_eq!(start.big_src, Rect::new(0.0, 0.0, 400.0, 200.0));
        assert_eq!(start.small_dst, child);
        let end = zoom_geometry(400.0, 200.0, child, 1.0);
        assert_eq!(end.big_src, child);
        assert_eq!(end.small_dst, Rect::new(0.0, 0.0, 400.0, 200.0));
        assert_eq!(end.small_alpha, 1.0);
    }

    #[test]
    fn compose_endpoints_show_each_view() {
        let (w, h) = (64usize, 32usize);
        let big = 0xFF11_2233u32.to_le_bytes().repeat(w * h);
        let small = 0xFF44_5566u32.to_le_bytes().repeat(w * h);
        let child = Rect::new(16.0, 8.0, 32.0, 16.0);
        let mut out = vec![0u32; w * h];
        compose_zoom(
            &mut out,
            w,
            h,
            &big,
            &small,
            &zoom_geometry(w as f32, h as f32, child, 0.0),
            2,
        );
        assert!(out.iter().all(|&p| p == 0xFF11_2233));
        compose_zoom(
            &mut out,
            w,
            h,
            &big,
            &small,
            &zoom_geometry(w as f32, h as f32, child, 1.0),
            2,
        );
        assert!(out.iter().all(|&p| p == 0xFF44_5566));
        compose_zoom(
            &mut out,
            w,
            h,
            &big,
            &small,
            &zoom_geometry(w as f32, h as f32, child, 0.5),
            3,
        );
        let mid = out[(h / 2) * w + w / 2];
        assert!(mid != 0xFF11_2233 && mid != 0xFF44_5566, "{mid:08x}");
    }

    #[test]
    fn blend_is_exact_at_the_ends() {
        assert_eq!(blend(0xFF00_0000, 0xFFFF_FFFF, 0), 0xFF00_0000);
        assert_eq!(blend(0xFF00_0000, 0xFFFF_FFFF, 256), 0xFFFF_FFFF);
    }
}
