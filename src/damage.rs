//! Unreadable areas taken from a GNU ddrescue map file, converted into
//! container physical block numbers.
//!
//! Map format (ddrescue manual, "Mapfile structure"): comment lines start with
//! `#`; the first other line is the status line `pos status [pass]`; every
//! following line is `pos size status` with byte values (usually hex). Only
//! status `+` means "read successfully"; `-`, `*`, `/`, `?` mean the bytes in
//! the image are not real data (ddrescue leaves zeros there).

use anyhow::{Context, Result, bail};
use std::path::Path;

fn parse_num(s: &str) -> Result<u64> {
    let v = if let Some(h) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u64::from_str_radix(h, 16)
    } else {
        s.parse()
    };
    v.with_context(|| format!("bad number {s:?} in ddrescue map"))
}

/// Byte ranges `[start, end)` of the whole-disk image that are not `+`.
pub fn read_bad_byte_ranges(path: &Path) -> Result<Vec<(u64, u64)>> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut out = Vec::new();
    let mut seen_status = false;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let t: Vec<&str> = line.split_whitespace().collect();
        if !seen_status {
            seen_status = true;
            continue; // current_pos current_status [current_pass]
        }
        if t.len() < 3 {
            bail!("unexpected line in ddrescue map: {line:?}");
        }
        let (pos, size, status) = (parse_num(t[0])?, parse_num(t[1])?, t[2]);
        if status != "+" && size > 0 {
            out.push((pos, pos + size));
        }
    }
    out.sort();
    Ok(out)
}

/// Bad areas in container physical block numbers, `[start, end)`, sorted.
#[derive(Default, Debug, Clone)]
pub struct DamageSet {
    pub ranges: Vec<(u64, u64)>,
}

impl DamageSet {
    /// `part_offset`: byte offset of the APFS partition inside the disk image.
    /// `base`: first container block number of this device (0 for the main
    /// device, the Fusion tier-2 base for the second one).
    pub fn add_map(&mut self, bad_bytes: &[(u64, u64)], part_offset: u64, part_len: u64, bs: u64, base: u64) {
        for &(s, e) in bad_bytes {
            let s = s.max(part_offset);
            let e = e.min(part_offset + part_len);
            if s >= e {
                continue;
            }
            let (rs, re) = (s - part_offset, e - part_offset);
            // Any partly unreadable block counts as damaged.
            self.ranges.push((base + rs / bs, base + re.div_ceil(bs)));
        }
        self.ranges.sort();
        // merge
        let mut merged: Vec<(u64, u64)> = Vec::with_capacity(self.ranges.len());
        for r in self.ranges.drain(..) {
            match merged.last_mut() {
                Some(last) if r.0 <= last.1 => last.1 = last.1.max(r.1),
                _ => merged.push(r),
            }
        }
        self.ranges = merged;
    }

    /// Damaged sub-ranges of blocks `[p, p+n)`.
    pub fn overlaps(&self, p: u64, n: u64) -> Vec<(u64, u64)> {
        let end = p + n;
        let i = self.ranges.partition_point(|r| r.1 <= p);
        self.ranges[i..]
            .iter()
            .take_while(|r| r.0 < end)
            .map(|r| (r.0.max(p), r.1.min(end)))
            .collect()
    }

    pub fn total_blocks(&self) -> u64 {
        self.ranges.iter().map(|r| r.1 - r.0).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_to_blocks() {
        let dir = std::env::temp_dir().join(format!("apfs-rescue-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("t.map");
        std::fs::write(
            &p,
            "# Mapfile\n# current_pos  current_status  current_pass\n0x00000000     +               1\n\
             #      pos        size  status\n0x00000000  0x00001000  +\n0x00001000  0x00000200  -\n0x00001200  0x00010000  +\n",
        )
        .unwrap();
        let bad = read_bad_byte_ranges(&p).unwrap();
        assert_eq!(bad, vec![(0x1000, 0x1200)]);
        let mut d = DamageSet::default();
        d.add_map(&bad, 0x800, 0x100000, 0x1000, 100);
        // bytes 0x800..0xa00 of the partition -> block 0
        assert_eq!(d.ranges, vec![(100, 101)]);
        assert_eq!(d.overlaps(99, 3), vec![(100, 101)]);
        assert!(d.overlaps(101, 5).is_empty());
        std::fs::remove_dir_all(dir).ok();
    }
}
