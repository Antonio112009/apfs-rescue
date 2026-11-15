//! Repairing a damaged photo with an intact smaller copy of the same shot.
//!
//! Everything that was read correctly stays as in the original, at full size.
//! Only damaged places are taken from the reference (the small copy, scaled
//! up), with soft edges. JPEG pictures first get their shifted blocks put back
//! (see `jpeg.rs`), PNG pictures are read up to the damage (see `png.rs`).
//! The original's colour profile is kept; date, camera and place are copied
//! with ExifTool when it is installed.

use super::raster::{self, Gray, Rgb};
use super::tools::{self, TempDir, first_frame};
use super::{jpeg, png};
use anyhow::{Context, Result, bail};
use std::ffi::OsString;
use std::path::Path;

pub struct Report {
    pub width: usize,
    pub height: usize,
    /// Share of the picture taken from the reference, in percent.
    pub from_reference: f32,
    /// JPEG: places where shifted blocks were put back.
    pub moved_back: usize,
    /// PNG: rows read exactly from the original.
    pub exact_rows: Option<usize>,
    pub metadata_copied: bool,
}

/// EXIF orientation → (ImageMagick operation that turns the upright reference
/// back to how the pixels are stored, ImageMagick orientation name).
fn orientation(o: u8) -> (&'static [&'static str], &'static str) {
    match o {
        2 => (&["-flop"], "TopRight"),
        3 => (&["-rotate", "180"], "BottomRight"),
        4 => (&["-flip"], "BottomLeft"),
        5 => (&["-transpose"], "LeftTop"),
        6 => (&["-rotate", "270"], "RightTop"),
        7 => (&["-transverse"], "RightBottom"),
        8 => (&["-rotate", "90"], "LeftBottom"),
        _ => (&[], "TopLeft"),
    }
}

fn orientation_number(name: &str) -> u8 {
    match name.trim() {
        "TopRight" => 2,
        "BottomRight" => 3,
        "BottomLeft" => 4,
        "LeftTop" => 5,
        "RightTop" => 6,
        "RightBottom" => 7,
        "LeftBottom" => 8,
        _ => 1,
    }
}

fn identify(path: &Path, format: &str) -> Result<String> {
    let mut args: Vec<OsString> = vec!["identify".into(), "-format".into(), format.into()];
    args.push(first_frame(path));
    Ok(String::from_utf8_lossy(&tools::magick(args)?).into_owned())
}

fn size(path: &Path) -> Result<(usize, usize)> {
    let s = identify(path, "%w %h")?;
    let mut it = s.split_whitespace().map(|v| v.parse::<usize>());
    match (it.next(), it.next()) {
        (Some(Ok(w)), Some(Ok(h))) if w > 0 && h > 0 => Ok((w, h)),
        _ => bail!("cannot read the size of {}", path.display()),
    }
}

/// Decodes to 8-bit RGB with ImageMagick, applying `ops` first.
fn load(path: &Path, ops: &[OsString], w: usize, h: usize) -> Result<Rgb> {
    let mut args = vec![first_frame(path)];
    args.extend(ops.iter().cloned());
    args.extend(["-depth", "8", "rgb:-"].map(OsString::from));
    let raw = tools::magick(args)?;
    if raw.len() != w * h * 3 {
        bail!(
            "{} decoded to {} bytes instead of {}",
            path.display(),
            raw.len(),
            w * h * 3
        );
    }
    Ok(Rgb::from_bytes(w, h, &raw))
}

/// Where the picture differs a lot from the reference, judged at the
/// reference's size (fine detail the small copy lacks is ignored).
fn diff_mask(a: &Rgb, r: &Rgb, rw: usize, rh: usize) -> Gray {
    let d = raster::diff_luma(&raster::shrink(a, rw, rh), &raster::shrink(r, rw, rh));
    let mut m = raster::blur(&d, 1.5);
    m.px.iter_mut()
        .for_each(|v| *v = if *v > 255.0 * 0.09 { 1.0 } else { 0.0 });
    let m = raster::dilate(&raster::open(&m, 1), 6);
    raster::enlarge(&m, a.w, a.h)
}

pub fn repair(damaged: &Path, reference: &Path, out: &Path, bad: &[(u64, u64)]) -> Result<Report> {
    tools::need("magick")?;
    let tmp = TempDir::new()?;
    let head = {
        use std::io::Read;
        let mut b = [0u8; 8];
        let n = std::fs::File::open(damaged)
            .with_context(|| format!("cannot open {}", damaged.display()))?
            .read(&mut b)?;
        b[..n].to_vec()
    };
    let is_jpeg = head.starts_with(&[0xFF, 0xD8]);
    let is_png = head.starts_with(b"\x89PNG");

    // The damaged picture, as far as it can be decoded.
    let icc = tmp.join("profile.icc");
    let mut exact_rows = None;
    let (d, o, has_icc) = match size(damaged) {
        Ok((w, h)) => {
            // JPEG pixels are stored as shot and turned by the EXIF tag; other
            // formats decode upright.
            let o = if is_jpeg {
                orientation_number(&identify(damaged, "%[orientation]")?)
            } else {
                1
            };
            let mut args = vec![first_frame(damaged)];
            args.push(icc.clone().into_os_string());
            let has_icc = tools::magick(args).is_ok() && icc.metadata().map(|m| m.len() > 0).unwrap_or(false);
            (load(damaged, &[], w, h)?, o, has_icc)
        }
        Err(e) if is_png => {
            let first_bad = bad.iter().map(|r| r.0).min();
            let p = png::decode(&std::fs::read(damaged)?, first_bad).context(e)?;
            if p.rows == 0 {
                bail!("nothing of {} could be decoded", damaged.display());
            }
            exact_rows = Some(p.rows);
            let has_icc = match &p.icc {
                Some(b) => std::fs::write(&icc, b).is_ok(),
                None => false,
            };
            (p.picture, 1, has_icc)
        }
        Err(e) => return Err(e),
    };
    let (w, h) = (d.w, d.h);
    let (mut rw, mut rh) = size(reference)?;
    if matches!(o, 5..=8) {
        (rw, rh) = (rh, rw); // the reference is upright, the pixels are stored sideways
    }
    if ((w as f64 / h as f64) - (rw as f64 / rh as f64)).abs() > 0.01 {
        bail!("the reference has a different shape ({rw}×{rh} vs {w}×{h}): it is cropped, edited or another picture");
    }
    let (turn, orient_name) = orientation(o);
    let profile: Vec<OsString> = if has_icc {
        vec!["-profile".into(), icc.clone().into_os_string()]
    } else {
        vec![]
    };
    let mut ops: Vec<OsString> = turn.iter().map(OsString::from).collect();
    ops.extend(profile.iter().cloned());
    ops.extend(["-filter", "lanczos", "-resize"].map(OsString::from));
    ops.push(format!("{w}x{h}!").into());
    let r = load(reference, &ops, w, h)?;

    // Fix what can be fixed with the original's own data.
    let mut lost = Gray::new(w, h);
    let mut moved_back = 0;
    let a = if is_jpeg {
        let sampling = identify(damaged, "%[jpeg:sampling-factor]").unwrap_or_default();
        let first = sampling.split(',').next().unwrap_or("2x2");
        let mut f = first.split('x').map(|v| v.trim().parse::<usize>().unwrap_or(2));
        let (hs, vs) = (f.next().unwrap_or(2).clamp(1, 2), f.next().unwrap_or(2).clamp(1, 2));
        let fixed = jpeg::realign(&d, &r, 8 * vs, 8 * hs);
        moved_back = fixed.shifts.len();
        for (m, &l) in lost.px.iter_mut().zip(&fixed.lost) {
            *m = if l { 1.0 } else { 0.0 };
        }
        // Include the edges of lost blocks.
        lost = raster::blur(&lost, 3.0);
        lost.px.iter_mut().for_each(|v| *v = if *v > 0.01 { 1.0 } else { 0.0 });
        fixed.picture
    } else {
        if let Some(rows) = exact_rows {
            lost.px[rows * w..].fill(1.0);
        }
        d
    };

    // Everything that still differs a lot comes from the reference.
    let dm = diff_mask(&a, &r, rw, rh);
    for (l, m) in lost.px.iter_mut().zip(&dm.px) {
        *l = l.max(*m);
    }
    let share = lost.mean() * 100.0;
    if share > 60.0 && exact_rows.is_none() {
        bail!(
            "{share:.0}% of the picture differs from the reference: it is probably an edited version or another picture"
        );
    }
    let m = raster::blur(&lost, 6.0);
    let mut f = a;
    for (i, mv) in m.px.iter().enumerate() {
        for c in 0..3 {
            let p = &mut f.px[i * 3 + c];
            *p = *p * (1.0 - mv) + r.px[i * 3 + c] * mv;
        }
    }

    // Write: same colour profile, turned upright, high quality.
    let mut args: Vec<OsString> = [
        "-size".into(),
        format!("{w}x{h}").into(),
        "-depth".into(),
        "8".into(),
        "rgb:-".into(),
    ]
    .to_vec();
    args.extend(profile);
    args.extend(["-orient", orient_name, "-auto-orient", "-quality", "95"].map(OsString::from));
    args.push(out.as_os_str().to_os_string());
    tools::magick_in(args, &f.to_bytes())?;

    let metadata_copied = tools::available("exiftool")
        && tools::status(
            "exiftool",
            [
                OsString::from("-q"),
                "-overwrite_original".into(),
                "-TagsFromFile".into(),
                damaged.as_os_str().to_os_string(),
                "-all:all".into(),
                "-Orientation=1".into(),
                "-n".into(),
                out.as_os_str().to_os_string(),
            ],
        )
        .map(|s| s.0)
        .unwrap_or(false);
    Ok(Report {
        width: w,
        height: h,
        from_reference: share,
        moved_back,
        exact_rows,
        metadata_copied,
    })
}
