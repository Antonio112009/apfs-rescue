//! Putting a damaged JPEG back together.
//!
//! A JPEG stores the picture as a stream of blocks (MCUs), left to right and
//! top to bottom, and each block's average colour as a difference from the
//! previous block. When a piece of the stream is lost, the decoder does not
//! stop: it picks up again some blocks too early or too late, and every block
//! after that is shifted and has a wrong colour offset. The picture is still
//! there at full resolution. Comparing with an intact smaller copy of the same
//! shot tells how far the blocks moved and by how much the colour changed, so
//! they can be put back. Only blocks that are really lost are reported.

use super::raster::{Rgb, median, percentile};
use rayon::prelude::*;

/// What [`realign`] did.
pub struct Realigned {
    /// The corrected picture (lost blocks are copied from the reference).
    pub picture: Rgb,
    /// One flag per pixel: the pixel's block was lost.
    pub lost: Vec<bool>,
    /// Where the block order changed: (first block, shift in blocks).
    pub shifts: Vec<(usize, i64)>,
}

struct Blocks {
    n: usize,
    nr: usize,
    nc: usize,
    mh: usize,
    mw: usize,
}

impl Blocks {
    /// Block `j` as a list of pixel indexes (clamped at the picture edges).
    fn pixels(&self, j: usize, w: usize, h: usize) -> impl Iterator<Item = usize> + '_ {
        let (by, bx) = (j / self.nc, j % self.nc);
        (0..self.mh).flat_map(move |dy| {
            (0..self.mw).map(move |dx| {
                let y = (by * self.mh + dy).min(h - 1);
                let x = (bx * self.mw + dx).min(w - 1);
                y * w + x
            })
        })
    }
}

/// 4×4 cells of average luma and colour for every block.
fn cells(p: &Rgb, b: &Blocks) -> (Vec<[f32; 16]>, Vec<[f32; 3]>) {
    (0..b.n)
        .into_par_iter()
        .map(|j| {
            let (by, bx) = (j / b.nc, j % b.nc);
            let mut y16 = [0.0f32; 16];
            let mut rgb = [0.0f32; 3];
            let (ch, cw) = (b.mh / 4, b.mw / 4);
            for cy in 0..4 {
                for cx in 0..4 {
                    let mut s = 0.0;
                    for dy in 0..ch {
                        for dx in 0..cw {
                            let y = (by * b.mh + cy * ch + dy).min(p.h - 1);
                            let x = (bx * b.mw + cx * cw + dx).min(p.w - 1);
                            let i = (y * p.w + x) * 3;
                            s += 0.299 * p.px[i] + 0.587 * p.px[i + 1] + 0.114 * p.px[i + 2];
                            for (c, v) in rgb.iter_mut().enumerate() {
                                *v += p.px[i + c];
                            }
                        }
                    }
                    y16[cy * 4 + cx] = s / (ch * cw) as f32;
                }
            }
            let n = (b.mh * b.mw) as f32;
            (y16, [rgb[0] / n, rgb[1] / n, rgb[2] / n])
        })
        .unzip()
}

fn moving_avg(e: &[f32], n: usize) -> Vec<f32> {
    let mut c = vec![0.0f64; e.len() + 1];
    for (i, v) in e.iter().enumerate() {
        c[i + 1] = c[i] + *v as f64;
    }
    (0..e.len())
        .map(|i| {
            let lo = i.saturating_sub(n / 2);
            let hi = (i + n - n / 2).min(e.len());
            ((c[hi] - c[lo]) / (hi - lo) as f64) as f32
        })
        .collect()
}

/// `mh`×`mw` is the block size: 16×16 for 4:2:0 sampling, 8×16 for 4:2:2.
pub fn realign(damaged: &Rgb, reference: &Rgb, mh: usize, mw: usize) -> Realigned {
    let (w, h) = (damaged.w, damaged.h);
    let b = Blocks {
        nr: h.div_ceil(mh),
        nc: w.div_ceil(mw),
        n: 0,
        mh,
        mw,
    };
    let b = Blocks { n: b.nr * b.nc, ..b };
    let (n, nc) = (b.n, b.nc);
    let (yd, cd) = cells(damaged, &b);
    let (yr, cr) = cells(reference, &b);
    let l = nc; // compare one row of blocks at a time

    let resid = |j0: usize, j1: usize, k: i64, off: f32| -> Vec<f32> {
        (j0..j1)
            .map(|j| {
                let s = j as i64 + k;
                if s < 0 || s >= n as i64 {
                    return 255.0;
                }
                let (a, r) = (&yd[s as usize], &yr[j]);
                a.iter().zip(r).map(|(x, y)| (x - y - off).abs()).sum::<f32>() / 16.0
            })
            .collect()
    };
    let offset = |j0: usize, j1: usize, k: i64| -> f32 {
        let mut d: Vec<f32> = (j0..j1)
            .filter_map(|j| {
                let s = j as i64 + k;
                (s >= 0 && s < n as i64).then(|| {
                    let (a, r) = (&yd[s as usize], &yr[j]);
                    a.iter().zip(r).map(|(x, y)| x - y).sum::<f32>() / 16.0
                })
            })
            .collect();
        median(&mut d)
    };
    // Best shift for the true blocks s..s+l, ignoring any colour offset.
    let search = |s: usize, k: i64| -> Option<(i64, f32, f32)> {
        let lo = (k - 6000).max(-(s as i64));
        let hi = (k + 6000).min(n as i64 - l as i64 - s as i64);
        if lo > hi {
            return None;
        }
        let costs: Vec<(i64, f32)> = (lo..=hi)
            .into_par_iter()
            .map(|kk| {
                let base = (s as i64 + kk) as usize;
                let mut sum = 0.0f32;
                for i in 0..l {
                    for (x, y) in yd[base + i].iter().zip(&yr[s + i]) {
                        sum += x - y;
                    }
                }
                let mean = sum / (16 * l) as f32;
                let mut cost = 0.0f32;
                for i in 0..l {
                    for (x, y) in yd[base + i].iter().zip(&yr[s + i]) {
                        cost += (x - y - mean).abs();
                    }
                }
                (kk, cost / (16 * l) as f32)
            })
            .collect();
        let best = *costs.iter().min_by(|a, b| a.1.total_cmp(&b.1))?;
        let second = costs
            .iter()
            .filter(|c| (c.0 - best.0).abs() > 2)
            .map(|c| c.1)
            .min_by(|a, b| a.total_cmp(b))
            .unwrap_or(best.1);
        Some((best.0, best.1, second))
    };

    let top = (2 * nc).min(n);
    let off0 = offset(0, top, 0);
    let noise = percentile(&resid(0, top, 0, off0), 75.0);
    // Per-block limit: well above how well this area matched just before.
    let limits = |e: &[f32]| -> Vec<f32> {
        let mut c = vec![0.0f64; e.len() + 1];
        for (i, v) in e.iter().enumerate() {
            c[i + 1] = c[i] + v.min(30.0) as f64;
        }
        (0..e.len())
            .map(|i| {
                let lo = i.saturating_sub(nc);
                let local = if i - lo >= 6 {
                    ((c[i] - c[lo]) / (i - lo) as f64) as f32
                } else {
                    noise
                };
                (3.0 * local).max(10.0)
            })
            .collect()
    };

    let mut segs: Vec<(usize, usize, i64)> = Vec::new();
    let mut lost_block = vec![false; n];
    let (mut j, mut k, mut off) = (0usize, 0i64, off0);
    while j < n {
        let e = resid(j, n, k, off);
        let lim = limits(&e);
        let sm = moving_avg(&e, 6);
        let Some(first) = (0..e.len()).find(|&i| sm[i] > lim[i]) else {
            segs.push((j, n, k));
            break;
        };
        let bk = j + first;
        let thr = lim[first];
        segs.push((j, bk, k));
        let mut found = None;
        for g in [8, nc / 2, nc, 2 * nc, 4 * nc, 8 * nc, 16 * nc] {
            let s = bk + g;
            if s + l > n {
                break;
            }
            if let Some((k2, c1, c2)) = search(s, k)
                && c1 < 12.0
                && c1 < 0.7 * c2
            {
                found = Some((s, k2));
                break;
            }
        }
        let Some((s, k2)) = found else {
            lost_block[bk..].fill(true);
            break;
        };
        let off2 = offset(s, s + l, k2);
        // Walk back from s to the first block that already fits the new placement.
        let e2 = moving_avg(&resid(bk, s, k2, off2), 6);
        let mut g = s;
        while g > bk && e2[g - 1 - bk] <= thr {
            g -= 1;
        }
        lost_block[bk..g].fill(true);
        (j, k, off) = (g, k2, off2);
    }

    // Colour difference between the original and the reference rendering,
    // measured where nothing moved.
    let chan_median = |js: &[usize], k: i64| -> [f32; 3] {
        let mut out = [0.0; 3];
        for (c, o) in out.iter_mut().enumerate() {
            let mut v: Vec<f32> = js.iter().map(|&j| cd[(j as i64 + k) as usize][c] - cr[j][c]).collect();
            *o = median(&mut v);
        }
        out
    };
    let base = chan_median(&(0..top).collect::<Vec<_>>(), 0);

    let mut px = damaged.px.clone();
    let mut shifts = Vec::new();
    for &(j0, j1, k) in &segs {
        let ok: Vec<usize> = (j0..j1)
            .filter(|&j| {
                let s = j as i64 + k;
                let fits = s >= 0 && s < n as i64;
                if !fits {
                    lost_block[j] = true;
                }
                fits
            })
            .collect();
        if ok.is_empty() {
            continue;
        }
        let o = chan_median(&ok, k);
        let fix = [o[0] - base[0], o[1] - base[1], o[2] - base[2]];
        if k != 0 {
            shifts.push((j0, k));
        }
        for &j in &ok {
            let src = (j as i64 + k) as usize;
            for (dst, from) in b.pixels(j, w, h).zip(b.pixels(src, w, h)) {
                for c in 0..3 {
                    px[dst * 3 + c] = damaged.px[from * 3 + c] - fix[c];
                }
            }
        }
    }
    let mut lost = vec![false; w * h];
    for (j, _) in lost_block.iter().enumerate().filter(|x| *x.1) {
        for p in b.pixels(j, w, h) {
            lost[p] = true;
            for c in 0..3 {
                px[p * 3 + c] = reference.px[p * 3 + c];
            }
        }
    }
    shifts.dedup_by_key(|s| s.1);
    Realigned {
        picture: Rgb { w, h, px },
        lost,
        shifts,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A smooth test picture with some texture.
    fn scene(w: usize, h: usize) -> Rgb {
        let mut px = Vec::with_capacity(w * h * 3);
        for y in 0..h {
            for x in 0..w {
                let (fx, fy) = (x as f32, y as f32);
                let v = 120.0 + 60.0 * (fx / 23.0).sin() * (fy / 17.0).cos() + 30.0 * ((fx + 2.0 * fy) / 9.0).sin();
                px.extend([v, v * 0.8 + 20.0, 255.0 - v]);
            }
        }
        Rgb { w, h, px }
    }

    #[test]
    fn puts_shifted_blocks_back() {
        let (w, h, m) = (512, 384, 16);
        let truth = scene(w, h);
        let (nc, nr) = (w / m, h / m);
        let n = nc * nr;
        // Damage at block 300: 5 blocks of garbage, then every later block
        // shows up 37 places early (true block j lands at j - 37) and with a
        // colour offset, the way a decoder resynchronises after lost data.
        let (cut, lost_n, shift) = (300usize, 5usize, 37usize);
        let mut dmg = truth.clone();
        let bl = Blocks {
            n,
            nr,
            nc,
            mh: m,
            mw: m,
        };
        for j in cut..n {
            let src = j + shift;
            let pix: Vec<usize> = bl.pixels(j, w, h).collect();
            if j < cut + lost_n {
                for p in pix {
                    dmg.px[p * 3..p * 3 + 3].copy_from_slice(&[0.0, 255.0, 0.0]);
                }
            } else if src < n {
                for (d, s) in pix.into_iter().zip(bl.pixels(src, w, h)) {
                    for c in 0..3 {
                        dmg.px[d * 3 + c] = truth.px[s * 3 + c] + [25.0, -15.0, 10.0][c];
                    }
                }
            } else {
                for p in pix {
                    dmg.px[p * 3..p * 3 + 3].copy_from_slice(&[128.0, 128.0, 128.0]);
                }
            }
        }
        // The reference is the truth, a bit softened (as a scaled-up preview is).
        let r = realign(&dmg, &truth, m, m);
        assert!(r.shifts.iter().any(|s| s.1 == -(shift as i64)), "shifts {:?}", r.shifts);
        let lost = r.lost.iter().filter(|&&v| v).count() as f32 / r.lost.len() as f32;
        assert!(lost < 0.08, "too much lost: {lost}");
        // Away from the lost blocks the picture matches the truth.
        let err: f32 = r
            .picture
            .px
            .chunks(3)
            .zip(truth.px.chunks(3))
            .map(|(a, b)| (a[0] - b[0]).abs() + (a[1] - b[1]).abs() + (a[2] - b[2]).abs())
            .sum::<f32>()
            / (w * h * 3) as f32;
        assert!(err < 1.0, "mean error {err}");
    }
}
