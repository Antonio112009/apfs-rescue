//! Plain pixel arrays and the few filters the photo repair needs.

use rayon::prelude::*;

/// An RGB picture, 8-bit values stored as f32, row by row.
#[derive(Clone)]
pub struct Rgb {
    pub w: usize,
    pub h: usize,
    pub px: Vec<f32>,
}

impl Rgb {
    pub fn from_bytes(w: usize, h: usize, b: &[u8]) -> Self {
        Self {
            w,
            h,
            px: b.iter().map(|&v| v as f32).collect(),
        }
    }
    pub fn to_bytes(&self) -> Vec<u8> {
        self.px.iter().map(|&v| v.round().clamp(0.0, 255.0) as u8).collect()
    }
}

/// A one-channel picture (masks, grey levels).
#[derive(Clone)]
pub struct Gray {
    pub w: usize,
    pub h: usize,
    pub px: Vec<f32>,
}

impl Gray {
    pub fn new(w: usize, h: usize) -> Self {
        Self {
            w,
            h,
            px: vec![0.0; w * h],
        }
    }
    pub fn mean(&self) -> f32 {
        if self.px.is_empty() {
            return 0.0;
        }
        self.px.iter().sum::<f32>() / self.px.len() as f32
    }
}

/// Shrinks by averaging every source pixel that falls in each target pixel.
pub fn shrink(src: &Rgb, nw: usize, nh: usize) -> Rgb {
    let mut px = vec![0.0f32; nw * nh * 3];
    px.par_chunks_mut(nw * 3).enumerate().for_each(|(y, row)| {
        let y0 = y * src.h / nh;
        let y1 = ((y + 1) * src.h).div_ceil(nh).clamp(y0 + 1, src.h);
        for x in 0..nw {
            let x0 = x * src.w / nw;
            let x1 = ((x + 1) * src.w).div_ceil(nw).clamp(x0 + 1, src.w);
            let mut acc = [0.0f32; 3];
            for sy in y0..y1 {
                let base = (sy * src.w) * 3;
                for sx in x0..x1 {
                    let i = base + sx * 3;
                    acc[0] += src.px[i];
                    acc[1] += src.px[i + 1];
                    acc[2] += src.px[i + 2];
                }
            }
            let n = ((y1 - y0) * (x1 - x0)) as f32;
            for c in 0..3 {
                row[x * 3 + c] = acc[c] / n;
            }
        }
    });
    Rgb { w: nw, h: nh, px }
}

/// Luma of the absolute difference of two pictures of the same size.
pub fn diff_luma(a: &Rgb, b: &Rgb) -> Gray {
    let px =
        a.px.par_chunks(3)
            .zip(b.px.par_chunks(3))
            .map(|(p, q)| 0.2126 * (p[0] - q[0]).abs() + 0.7152 * (p[1] - q[1]).abs() + 0.0722 * (p[2] - q[2]).abs())
            .collect();
    Gray { w: a.w, h: a.h, px }
}

fn kernel(sigma: f32) -> Vec<f32> {
    let r = (sigma * 3.0).ceil() as i64;
    let mut k: Vec<f32> = (-r..=r)
        .map(|i| (-(i * i) as f32 / (2.0 * sigma * sigma)).exp())
        .collect();
    let s: f32 = k.iter().sum();
    k.iter_mut().for_each(|v| *v /= s);
    k
}

/// Gaussian blur (edges repeat the border pixel).
pub fn blur(g: &Gray, sigma: f32) -> Gray {
    if sigma <= 0.0 {
        return g.clone();
    }
    let k = kernel(sigma);
    let r = (k.len() / 2) as isize;
    let (w, h) = (g.w, g.h);
    let mut tmp = vec![0.0f32; w * h];
    tmp.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
        let src = &g.px[y * w..(y + 1) * w];
        for (x, out) in row.iter_mut().enumerate() {
            let mut s = 0.0;
            for (i, kv) in k.iter().enumerate() {
                let sx = (x as isize + i as isize - r).clamp(0, w as isize - 1) as usize;
                s += kv * src[sx];
            }
            *out = s;
        }
    });
    let mut px = vec![0.0f32; w * h];
    px.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
        for (i, kv) in k.iter().enumerate() {
            let sy = (y as isize + i as isize - r).clamp(0, h as isize - 1) as usize;
            let src = &tmp[sy * w..(sy + 1) * w];
            for (o, s) in row.iter_mut().zip(src) {
                *o += kv * s;
            }
        }
    });
    Gray { w, h, px }
}

fn disk(r: i64) -> Vec<(i64, i64)> {
    let mut v = Vec::new();
    for dy in -r..=r {
        for dx in -r..=r {
            if dx * dx + dy * dy <= r * r + r / 2 {
                v.push((dx, dy));
            }
        }
    }
    v
}

/// Grows (`max`) or shrinks (`min`) the white parts of a 0/1 mask by a disk.
fn morph(g: &Gray, r: i64, grow: bool) -> Gray {
    let d = disk(r);
    let (w, h) = (g.w as i64, g.h as i64);
    let mut px = vec![0.0f32; g.px.len()];
    px.par_chunks_mut(g.w).enumerate().for_each(|(y, row)| {
        for (x, out) in row.iter_mut().enumerate() {
            let mut v = if grow { 0.0f32 } else { 1.0f32 };
            for &(dx, dy) in &d {
                let sx = (x as i64 + dx).clamp(0, w - 1);
                let sy = (y as i64 + dy).clamp(0, h - 1);
                let s = g.px[(sy * w + sx) as usize];
                v = if grow { v.max(s) } else { v.min(s) };
            }
            *out = v;
        }
    });
    Gray { w: g.w, h: g.h, px }
}

pub fn dilate(g: &Gray, r: i64) -> Gray {
    morph(g, r, true)
}

pub fn open(g: &Gray, r: i64) -> Gray {
    morph(&morph(g, r, false), r, true)
}

/// Enlarges with nearest-neighbour sampling.
pub fn enlarge(g: &Gray, nw: usize, nh: usize) -> Gray {
    let mut px = vec![0.0f32; nw * nh];
    px.par_chunks_mut(nw).enumerate().for_each(|(y, row)| {
        let sy = (y * g.h / nh).min(g.h - 1);
        for (x, o) in row.iter_mut().enumerate() {
            *o = g.px[sy * g.w + (x * g.w / nw).min(g.w - 1)];
        }
    });
    Gray { w: nw, h: nh, px }
}

pub fn median(v: &mut [f32]) -> f32 {
    if v.is_empty() {
        return 0.0;
    }
    let mid = v.len() / 2;
    let (_, m, _) = v.select_nth_unstable_by(mid, |a, b| a.total_cmp(b));
    *m
}

pub fn percentile(v: &[f32], p: f32) -> f32 {
    if v.is_empty() {
        return 0.0;
    }
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.total_cmp(b));
    let i = ((s.len() - 1) as f32 * p / 100.0).round() as usize;
    s[i]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shrink_averages() {
        let src = Rgb::from_bytes(2, 2, &[0, 0, 0, 100, 100, 100, 200, 200, 200, 100, 100, 100]);
        let s = shrink(&src, 1, 1);
        assert_eq!(s.px, vec![100.0; 3]);
    }

    #[test]
    fn blur_keeps_flat_and_total() {
        let mut g = Gray::new(20, 20);
        g.px[210] = 1.0;
        let b = blur(&g, 1.5);
        assert!((b.px.iter().sum::<f32>() - 1.0).abs() < 1e-3);
        let flat = Gray {
            w: 5,
            h: 5,
            px: vec![0.5; 25],
        };
        assert!(blur(&flat, 2.0).px.iter().all(|v| (v - 0.5).abs() < 1e-5));
    }

    #[test]
    fn open_removes_specks_dilate_grows() {
        let mut g = Gray::new(9, 9);
        g.px[40] = 1.0;
        assert_eq!(open(&g, 1).mean(), 0.0);
        assert!(dilate(&g, 2).px.iter().filter(|&&v| v > 0.0).count() > 9);
    }
}
