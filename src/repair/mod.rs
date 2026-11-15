//! `repair`: fix a damaged photo or video after it has been copied out.
//!
//! This works on ordinary files, never on the disk images. Decoding and
//! encoding are done by ImageMagick (pictures) and FFmpeg (videos); this code
//! decides what is damaged and how to put it right.

mod jpeg;
mod photo;
mod png;
mod raster;
mod tools;
mod video;

use anyhow::{Context, Result, bail};
use std::path::Path;

const PHOTO: &[&str] = &["jpg", "jpeg", "heic", "heif", "png", "tif", "tiff", "webp"];
const VIDEO: &[&str] = &["mov", "mp4", "m4v", "3gp"];

/// Parses byte ranges as printed by `damaged`: `100-200,4096-8192`.
pub fn parse_ranges(s: &str) -> Result<Vec<(u64, u64)>> {
    let mut out = Vec::new();
    for part in s.split([',', ' ', '\t']).filter(|p| !p.is_empty()) {
        let (a, b) = part
            .split_once('-')
            .with_context(|| format!("{part:?} is not a byte range like 4096-8192"))?;
        let (a, b): (u64, u64) = (a.trim().parse()?, b.trim().parse()?);
        if b <= a {
            bail!("byte range {part:?} is empty");
        }
        out.push((a, b));
    }
    out.sort_unstable();
    Ok(out)
}

fn pct(x: f64) -> String {
    format!("{x:.1}")
}

pub fn run(file: &Path, out: &Path, reference: Option<&Path>, bad: Option<&str>) -> Result<()> {
    if !file.is_file() {
        bail!("{} is not a file", file.display());
    }
    if out.exists() {
        bail!(
            "{} already exists; choose a new name (the damaged file is never changed)",
            out.display()
        );
    }
    if let (Ok(a), Some(parent)) = (file.canonicalize(), out.parent()) {
        let parent = if parent.as_os_str().is_empty() {
            Path::new(".")
        } else {
            parent
        };
        if let (Ok(p), Some(name)) = (parent.canonicalize(), out.file_name())
            && p.join(name) == a
        {
            bail!("the output must be a new file, not the damaged one");
        }
    }
    let bad = match bad {
        Some(s) => parse_ranges(s)?,
        None => Vec::new(),
    };
    let ext = file.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
    if PHOTO.contains(&ext.as_str()) {
        let reference = reference.context(
            "a photo is repaired with an intact smaller copy of the same picture: give it with --reference \
             (for example the preview a Photos or iPhoto library keeps)",
        )?;
        let r = photo::repair(file, reference, out, &bad)?;
        println!("Done: {} ({}×{})", out.display(), r.width, r.height);
        if r.moved_back > 0 {
            println!("  shifted parts of the picture put back in place: {}", r.moved_back);
        }
        if let Some(rows) = r.exact_rows {
            println!("  rows read exactly from the original: {rows} of {}", r.height);
        }
        println!(
            "  {}% of the picture is the original's own pixels; {}% (the damaged places) comes from the reference",
            pct(100.0 - r.from_reference as f64),
            pct(r.from_reference as f64)
        );
        if !r.metadata_copied {
            println!("  note: install ExifTool to also copy date, camera and place into the repaired file");
        }
    } else if VIDEO.contains(&ext.as_str()) {
        if reference.is_some() {
            eprintln!("note: --reference is not used for videos");
        }
        let r = video::repair(file, out, &bad)?;
        let cut: f64 = r.removed.iter().map(|(a, b)| b - a).sum();
        println!("Done: {} (encoded with {})", out.display(), r.encoder);
        for (a, b) in &r.removed {
            println!("  spoiled frames removed: {a:.2}–{b:.2} s (the last whole frame holds there)");
        }
        if r.duration > 0.0 {
            println!(
                "  {}% of the frames are whole; {:.2} s of {:.0} s replaced",
                pct(100.0 - cut / r.duration * 100.0),
                cut,
                r.duration
            );
        }
        if r.audio_packets_hit > 0 {
            println!(
                "  note: {} pieces of sound were damaged too; a short click may remain",
                r.audio_packets_hit
            );
        }
    } else {
        bail!(
            "cannot repair .{ext} files; supported: photos ({}) and videos ({})",
            PHOTO.join(", "),
            VIDEO.join(", ")
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges() {
        assert_eq!(parse_ranges("4096-8192,0-10").unwrap(), vec![(0, 10), (4096, 8192)]);
        assert!(parse_ranges("10-5").is_err());
        assert!(parse_ranges("abc").is_err());
    }
}
