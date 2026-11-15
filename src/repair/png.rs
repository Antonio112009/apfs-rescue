//! Reading a damaged PNG as far as it goes.
//!
//! A PNG's pixels are one compressed stream; after a lost piece nothing more
//! can be unpacked. Every row before it is still exact, so those rows are
//! decoded here (image programs usually refuse the whole file).

use super::raster::Rgb;
use crate::util::be32;
use anyhow::{Result, bail};
use flate2::{Decompress, FlushDecompress, Status};

pub struct Partial {
    pub picture: Rgb,
    /// Rows 0..rows are exact; the rest is black.
    pub rows: usize,
    /// The embedded colour profile, if any.
    pub icc: Option<Vec<u8>>,
}

/// `first_bad`: file offset of the first unreadable byte, if known. Without
/// it, unpacking stops at the first error and the last two rows before it are
/// dropped to be safe.
pub fn decode(data: &[u8], first_bad: Option<u64>) -> Result<Partial> {
    if data.len() < 8 || &data[..8] != b"\x89PNG\r\n\x1a\n" {
        bail!("not a PNG file");
    }
    let (mut pos, mut idat, mut icc, mut cut) = (8usize, Vec::new(), None, None);
    let mut hdr = None;
    while pos + 8 <= data.len() {
        let n = be32(data, pos) as usize;
        let typ = &data[pos + 4..pos + 8];
        let body = &data[(pos + 8).min(data.len())..(pos + 8 + n).min(data.len())];
        match typ {
            b"IHDR" if body.len() >= 13 => {
                hdr = Some((
                    be32(body, 0) as usize,
                    be32(body, 4) as usize,
                    body[8],
                    body[9],
                    body[12],
                ))
            }
            b"IDAT" => {
                if let Some(fb) = first_bad
                    && cut.is_none()
                    && (fb as usize) < pos + 8 + n
                {
                    cut = Some(idat.len() + (fb as usize).saturating_sub(pos + 8));
                }
                idat.extend_from_slice(body);
            }
            b"iCCP" => {
                if let Some(z) = body.iter().position(|&b| b == 0) {
                    let mut out = Vec::new();
                    if flate2::read::ZlibDecoder::new(&body[(z + 2).min(body.len())..]).read_to_end_ok(&mut out) {
                        icc = Some(out);
                    }
                }
            }
            b"IEND" => break,
            _ => {}
        }
        pos += 12 + n;
    }
    let Some((w, h, depth, ctype, interlace)) = hdr else {
        bail!("PNG header missing")
    };
    if interlace != 0 {
        bail!("interlaced PNG files are not supported");
    }
    let ch = match ctype {
        0 => 1,
        2 => 3,
        4 => 2,
        6 => 4,
        _ => bail!("PNG colour type {ctype} is not supported"),
    };
    if depth != 8 && depth != 16 {
        bail!("PNG bit depth {depth} is not supported");
    }
    let bpp = ch * depth as usize / 8;
    let stride = w * bpp;
    let input = &idat[..cut.unwrap_or(idat.len()).min(idat.len())];
    let mut raw = vec![0u8; (stride + 1) * h];
    let mut z = Decompress::new(true);
    let clean = loop {
        let before = z.total_out() as usize;
        match z.decompress(
            &input[z.total_in() as usize..],
            &mut raw[before..],
            FlushDecompress::Sync,
        ) {
            Ok(Status::StreamEnd) => break true,
            Ok(_) if z.total_out() as usize == before || z.total_out() as usize >= raw.len() => break true,
            Ok(_) => {}
            Err(_) => break false,
        }
    };
    let got = z.total_out() as usize;
    let mut rows = (got / (stride + 1)).min(h);
    if !clean && first_bad.is_none() {
        rows = rows.saturating_sub(2);
    }

    let mut img = vec![0u8; w * h * 3];
    let mut prev = vec![0u8; stride];
    let mut cur = vec![0u8; stride];
    let mut good = 0;
    for y in 0..rows {
        let line = &raw[y * (stride + 1)..(y + 1) * (stride + 1)];
        let (f, line) = (line[0], &line[1..]);
        for x in 0..stride {
            let a = if x >= bpp { cur[x - bpp] as i32 } else { 0 };
            let b = prev[x] as i32;
            let c = if x >= bpp { prev[x - bpp] as i32 } else { 0 };
            let p = match f {
                0 => 0,
                1 => a,
                2 => b,
                3 => (a + b) >> 1,
                4 => {
                    let (pa, pb, pc) = ((b - c).abs(), (a - c).abs(), (a + b - 2 * c).abs());
                    if pa <= pb && pa <= pc {
                        a
                    } else if pb <= pc {
                        b
                    } else {
                        c
                    }
                }
                _ => break,
            };
            cur[x] = (line[x] as i32 + p) as u8;
        }
        if f > 4 {
            break;
        }
        let step = depth as usize / 8; // 16-bit samples: keep the high byte
        for x in 0..w {
            let px = &cur[x * bpp..];
            let s = |i: usize| px[i * step];
            let (r, g, bl) = if ch >= 3 {
                (s(0), s(1), s(2))
            } else {
                (s(0), s(0), s(0))
            };
            img[(y * w + x) * 3..(y * w + x) * 3 + 3].copy_from_slice(&[r, g, bl]);
        }
        std::mem::swap(&mut prev, &mut cur);
        good = y + 1;
    }
    Ok(Partial {
        picture: Rgb::from_bytes(w, h, &img),
        rows: good,
        icc,
    })
}

trait ReadAllOk {
    fn read_to_end_ok(self, out: &mut Vec<u8>) -> bool;
}

impl<R: std::io::Read> ReadAllOk for R {
    fn read_to_end_ok(mut self, out: &mut Vec<u8>) -> bool {
        self.read_to_end(out).is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{Compression, write::ZlibEncoder};
    use std::io::Write;

    fn chunk(out: &mut Vec<u8>, typ: &[u8], body: &[u8]) {
        out.extend((body.len() as u32).to_be_bytes());
        out.extend(typ);
        out.extend(body);
        out.extend([0, 0, 0, 0]); // CRC is not checked
    }

    fn png(w: usize, h: usize) -> (Vec<u8>, Vec<u8>, usize) {
        let mut raw = Vec::new();
        let mut pixels = Vec::new();
        for y in 0..h {
            raw.push((y % 5) as u8); // every filter type
            for x in 0..w {
                let p = [(x * 7 + y) as u8, (y * 3) as u8, (x ^ y) as u8];
                pixels.extend(p);
                raw.extend(p);
            }
        }
        // Store the rows "pre-filtered": undo each filter so decoding gives `pixels`.
        let stride = w * 3;
        let mut filtered = raw.clone();
        for y in 0..h {
            let f = raw[y * (stride + 1)];
            for x in 0..stride {
                let cur = |xx: usize| pixels[y * stride + xx] as i32;
                let up = |xx: usize| if y > 0 { pixels[(y - 1) * stride + xx] as i32 } else { 0 };
                let a = if x >= 3 { cur(x - 3) } else { 0 };
                let b = up(x);
                let c = if x >= 3 { up(x - 3) } else { 0 };
                let p = match f {
                    0 => 0,
                    1 => a,
                    2 => b,
                    3 => (a + b) >> 1,
                    _ => {
                        let (pa, pb, pc) = ((b - c).abs(), (a - c).abs(), (a + b - 2 * c).abs());
                        if pa <= pb && pa <= pc {
                            a
                        } else if pb <= pc {
                            b
                        } else {
                            c
                        }
                    }
                };
                filtered[y * (stride + 1) + 1 + x] = (cur(x) - p) as u8;
            }
        }
        let mut z = ZlibEncoder::new(Vec::new(), Compression::default());
        z.write_all(&filtered).unwrap();
        let comp = z.finish().unwrap();
        let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
        let mut ihdr = Vec::new();
        ihdr.extend((w as u32).to_be_bytes());
        ihdr.extend((h as u32).to_be_bytes());
        ihdr.extend([8, 2, 0, 0, 0]);
        chunk(&mut out, b"IHDR", &ihdr);
        let idat_at = out.len() + 8;
        chunk(&mut out, b"IDAT", &comp);
        chunk(&mut out, b"IEND", &[]);
        (out, pixels, idat_at)
    }

    #[test]
    fn intact_png_decodes_fully() {
        let (data, pixels, _) = png(40, 30);
        let p = decode(&data, None).unwrap();
        assert_eq!(p.rows, 30);
        assert_eq!(p.picture.to_bytes(), pixels);
    }

    #[test]
    fn damaged_png_keeps_rows_before_the_damage() {
        let (mut data, pixels, idat_at) = png(64, 64);
        let bad = idat_at + 400;
        data[bad..bad + 64].fill(0);
        let p = decode(&data, Some(bad as u64)).unwrap();
        assert!(p.rows > 0 && p.rows < 64, "rows {}", p.rows);
        let n = p.rows * 64 * 3;
        assert_eq!(&p.picture.to_bytes()[..n], &pixels[..n]);
    }
}
