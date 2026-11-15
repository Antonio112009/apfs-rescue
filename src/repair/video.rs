//! Repairing a damaged video.
//!
//! A lost piece of a video spoils the frame stored there and every frame that
//! is built on it, up to the next key frame. Such frames show as coloured
//! blocks. They are found from the unreadable byte ranges, removed, and the
//! video is re-encoded at a constant frame rate, so the last whole frame
//! holds for that moment (usually a fraction of a second) while the sound
//! goes on. Date, place, rotation, sound and HDR are kept. The result is
//! decoded once more and kept only if it has no errors.

use super::tools::{self, TempDir};
use anyhow::{Context, Result, bail};
use std::collections::HashMap;
use std::ffi::OsString;
use std::path::Path;

#[derive(Debug, Clone, PartialEq)]
pub struct Packet {
    pub pts: f64,
    pub pos: u64,
    pub size: u64,
    pub key: bool,
}

pub struct Report {
    pub duration: f64,
    /// Removed stretches, in seconds from the start.
    pub removed: Vec<(f64, f64)>,
    pub audio_packets_hit: usize,
    pub encoder: String,
}

fn fields(line: &str) -> HashMap<&str, &str> {
    line.split('|').filter_map(|kv| kv.split_once('=')).collect()
}

/// Stretches of time whose frames are spoiled: from each damaged frame (a
/// little earlier, for frames shown before it but built on it) to the next
/// key frame.
pub fn spoiled(video: &[Packet], bad: &[(u64, u64)], duration: f64) -> Vec<(f64, f64)> {
    let mut by_time: Vec<&Packet> = video.iter().collect();
    by_time.sort_by(|a, b| a.pts.total_cmp(&b.pts));
    let mut spans: Vec<(f64, f64)> = Vec::new();
    for p in &by_time {
        if !bad.iter().any(|&(s, e)| p.pos < e && p.pos + p.size > s) {
            continue;
        }
        let end = by_time
            .iter()
            .find(|q| q.key && q.pts > p.pts)
            .map(|q| q.pts)
            .unwrap_or(duration.max(p.pts));
        spans.push(((p.pts - 0.1).max(0.0), end));
    }
    spans.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut merged: Vec<(f64, f64)> = Vec::new();
    for (a, b) in spans {
        match merged.last_mut() {
            Some(last) if a <= last.1 + 0.05 => last.1 = last.1.max(b),
            _ => merged.push((a, b)),
        }
    }
    merged
}

fn probe_duration(path: &Path) -> f64 {
    tools::text(
        "ffprobe",
        [
            OsString::from("-v"),
            "error".into(),
            "-show_entries".into(),
            "format=duration".into(),
            "-of".into(),
            "csv=p=0".into(),
            path.as_os_str().to_os_string(),
        ],
    )
    .ok()
    .and_then(|s| s.trim().parse().ok())
    .unwrap_or(0.0)
}

pub fn repair(damaged: &Path, out: &Path, bad: &[(u64, u64)]) -> Result<Report> {
    tools::need("ffmpeg")?;
    tools::need("ffprobe")?;
    if bad.is_empty() {
        bail!("give the unreadable byte ranges with --bad (as printed by `damaged` or in the copy report)");
    }
    let src = damaged.as_os_str().to_os_string();
    let streams = tools::text(
        "ffprobe",
        [
            OsString::from("-v"),
            "error".into(),
            "-show_entries".into(),
            "stream=index,codec_type,codec_name,pix_fmt,bit_rate,r_frame_rate,color_primaries,color_transfer,color_space:format=duration,bit_rate".into(),
            "-of".into(),
            "compact=p=0".into(),
            src.clone(),
        ],
    )
    .with_context(|| format!("FFmpeg cannot open {}", damaged.display()))?;
    let lines: Vec<HashMap<&str, &str>> = streams.lines().map(fields).collect();
    let v = lines
        .iter()
        .find(|f| f.get("codec_type") == Some(&"video"))
        .context("no picture track in this file")?;
    let fmt = lines.iter().find(|f| !f.contains_key("index"));
    let duration: f64 = fmt
        .and_then(|f| f.get("duration"))
        .and_then(|d| d.parse().ok())
        .unwrap_or(0.0);
    let vindex: usize = v.get("index").and_then(|i| i.parse().ok()).unwrap_or(0);

    let packets = tools::text(
        "ffprobe",
        [
            OsString::from("-v"),
            "error".into(),
            "-show_entries".into(),
            "packet=stream_index,pts_time,pos,size,flags".into(),
            "-of".into(),
            "compact=p=0".into(),
            src.clone(),
        ],
    )?;
    let (mut video, mut audio_hit, mut covered) = (Vec::new(), 0usize, Vec::new());
    for line in packets.lines() {
        let f = fields(line);
        let (Some(pos), Some(size)) = (
            f.get("pos").and_then(|v| v.parse::<u64>().ok()),
            f.get("size").and_then(|v| v.parse::<u64>().ok()),
        ) else {
            continue;
        };
        covered.push((pos, pos + size));
        let idx: usize = f.get("stream_index").and_then(|v| v.parse().ok()).unwrap_or(usize::MAX);
        if idx == vindex {
            let Some(pts) = f.get("pts_time").and_then(|v| v.parse::<f64>().ok()) else {
                continue;
            };
            video.push(Packet {
                pts,
                pos,
                size,
                key: f.get("flags").is_some_and(|fl| fl.starts_with('K')),
            });
        } else if bad.iter().any(|&(s, e)| pos < e && pos + size > s) {
            audio_hit += 1;
        }
    }
    let removed = spoiled(&video, bad, duration);
    if removed.is_empty() {
        let in_media = bad.iter().any(|&(s, e)| covered.iter().any(|&(a, b)| a < e && b > s));
        if !in_media {
            bail!(
                "the damage is in the file's index, not in the picture: the video needs its structure rebuilt, which `repair` does not do"
            );
        }
        bail!("no picture frames are damaged (only sound), nothing to repair");
    }

    // Encoder: the same format as the original, hardware encoder if there is one.
    let codec = v.get("codec_name").copied().unwrap_or("");
    let pix = v.get("pix_fmt").copied().unwrap_or("");
    let ten_bit = pix.contains("10");
    let encoders = tools::text("ffmpeg", ["-hide_banner", "-encoders"]).unwrap_or_default();
    let has = |e: &str| encoders.split_whitespace().any(|w| w == e);
    let rate = v
        .get("bit_rate")
        .or_else(|| fmt.and_then(|f| f.get("bit_rate")))
        .and_then(|b| b.parse::<u64>().ok())
        .unwrap_or(10_000_000);
    let br = rate * 13 / 10;
    let mut enc: Vec<OsString> = Vec::new();
    let mut push = |a: &[&str]| enc.extend(a.iter().map(OsString::from));
    let encoder = if codec == "hevc" && has("hevc_videotoolbox") {
        push(&[
            "-c:v",
            "hevc_videotoolbox",
            "-tag:v",
            "hvc1",
            "-b:v",
            &br.to_string(),
            "-maxrate",
            &(2 * br).to_string(),
            "-bufsize",
            &(2 * br).to_string(),
        ]);
        push(if ten_bit {
            &["-profile:v", "main10", "-pix_fmt", "p010le"]
        } else {
            &["-profile:v", "main", "-pix_fmt", "yuv420p"]
        });
        "hevc_videotoolbox"
    } else if codec == "hevc" && has("libx265") {
        push(&["-c:v", "libx265", "-tag:v", "hvc1", "-crf", "18", "-preset", "medium"]);
        push(&["-pix_fmt", if ten_bit { "yuv420p10le" } else { "yuv420p" }]);
        "libx265"
    } else if codec == "h264" && has("h264_videotoolbox") {
        push(&[
            "-c:v",
            "h264_videotoolbox",
            "-b:v",
            &br.to_string(),
            "-maxrate",
            &(2 * br).to_string(),
            "-bufsize",
            &(2 * br).to_string(),
            "-pix_fmt",
            "yuv420p",
        ]);
        "h264_videotoolbox"
    } else if has("libx264") {
        push(&[
            "-c:v", "libx264", "-crf", "17", "-preset", "medium", "-pix_fmt", "yuv420p",
        ]);
        "libx264"
    } else {
        bail!("this FFmpeg has no suitable video encoder (libx264, libx265 or VideoToolbox)");
    };
    for (key, opt) in [
        ("color_primaries", "-color_primaries"),
        ("color_transfer", "-color_trc"),
        ("color_space", "-colorspace"),
    ] {
        if let Some(val) = v.get(key).filter(|x| !x.is_empty() && **x != "unknown") {
            enc.push(opt.into());
            enc.push(val.into());
        }
    }

    let keep = removed
        .iter()
        .map(|(a, b)| format!("between(t,{a:.3},{:.3})", b - 0.001))
        .collect::<Vec<_>>()
        .join("+");
    let tmp = TempDir::new()?;
    let ext = out.extension().and_then(|e| e.to_str()).unwrap_or("mov").to_lowercase();
    let part = tmp.join(&format!("part.{ext}"));
    let mut args: Vec<OsString> = ["-hide_banner", "-v", "error", "-y", "-err_detect", "ignore_err", "-i"]
        .map(OsString::from)
        .to_vec();
    args.push(src.clone());
    args.extend(["-map", &format!("0:{vindex}"), "-map", "0:a?", "-c:a", "copy", "-vf"].map(OsString::from));
    args.push(format!("select='not({keep})'").into());
    args.extend(["-r", v.get("r_frame_rate").copied().unwrap_or("30"), "-fps_mode", "cfr"].map(OsString::from));
    args.extend(enc);
    args.extend(["-map_metadata", "0", "-movflags", "+faststart+use_metadata_tags"].map(OsString::from));
    args.push(part.clone().into_os_string());
    let (ok, err) = tools::status("ffmpeg", args)?;
    if !ok || !part.exists() {
        bail!("FFmpeg could not re-encode the video: {}", err.trim());
    }

    // Keep the result only if it plays without a single error.
    let (_, errors) = tools::status(
        "ffmpeg",
        [
            OsString::from("-v"),
            "error".into(),
            "-i".into(),
            part.clone().into_os_string(),
            "-f".into(),
            "null".into(),
            "-".into(),
        ],
    )?;
    if !errors.trim().is_empty() {
        bail!("the re-encoded video still has errors: {}", errors.trim());
    }
    let d2 = probe_duration(&part);
    if duration > 0.0 && (d2 - duration).abs() > 1.0 {
        bail!("the re-encoded video is {d2:.1} s long instead of {duration:.1} s");
    }
    std::fs::copy(&part, out).with_context(|| format!("cannot write {}", out.display()))?;
    if let Ok(m) = std::fs::metadata(damaged) {
        let _ = filetime::set_file_mtime(out, filetime::FileTime::from_last_modification_time(&m));
    }
    Ok(Report {
        duration,
        removed,
        audio_packets_hit: audio_hit,
        encoder: encoder.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pk(pts: f64, pos: u64, key: bool) -> Packet {
        Packet {
            pts,
            pos,
            size: 100,
            key,
        }
    }

    #[test]
    fn spoiled_runs_to_next_key_frame() {
        // Key frames every second, a frame every 0.25 s, 100 bytes each.
        let v: Vec<Packet> = (0..12).map(|i| pk(i as f64 * 0.25, i * 100, i % 4 == 0)).collect();
        // Bytes of the frame at 1.5 s are lost.
        let s = spoiled(&v, &[(650, 660)], 3.0);
        assert_eq!(s.len(), 1);
        assert!((s[0].0 - 1.4).abs() < 1e-9 && (s[0].1 - 2.0).abs() < 1e-9, "{s:?}");
        // Damage in the last stretch runs to the end; nearby stretches merge.
        let s = spoiled(&v, &[(950, 960), (1050, 1060)], 3.0);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].1, 3.0);
        // Damage that touches no frame spoils nothing.
        assert!(spoiled(&v, &[(5000, 6000)], 3.0).is_empty());
    }

    #[test]
    fn parses_compact_lines() {
        let f = fields("stream_index=0|pts_time=1.5|pos=123|size=45|flags=K__");
        assert_eq!(f["pos"], "123");
        assert_eq!(f["flags"], "K__");
    }
}
