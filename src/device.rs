//! Read-only access to raw disk images and the Fusion block address space.
//!
//! Images are opened with `File::open`, which is read-only: this program has
//! no code path that writes to an image.

use crate::damage::DamageSet;
use crate::obj::checksum_ok;
use crate::util::{le32, le64};
use anyhow::{Context, Result, bail};
use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

/// GPT partition type GUID for Apple APFS (7C3457EF-0000-11AA-AA11-00306543ECAC)
/// as stored on disk (first three fields little-endian).
const APFS_TYPE_GUID: [u8; 16] = [
    0xEF, 0x57, 0x34, 0x7C, 0x00, 0x00, 0xAA, 0x11, 0xAA, 0x11, 0x00, 0x30, 0x65, 0x43, 0xEC, 0xAC,
];

/// Apple File System Reference, "Fusion": tier-2 addresses start at this byte
/// address; in blocks it is this value shifted right by log2(block size).
pub const FUSION_TIER2_DEVICE_BYTE_ADDR: u64 = 0x4000_0000_0000_0000;

pub struct Image {
    pub path: PathBuf,
    file: File,
    /// Byte offset of the APFS container inside the image file.
    pub offset: u64,
    pub len: u64,
}

impl Image {
    pub fn open(path: &Path) -> Result<Image> {
        let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
        let total = file.metadata()?.len();
        let (offset, len) = match find_apfs_partition(&file)? {
            Some(p) => p,
            None => (0, total),
        };
        let img = Image {
            path: path.to_path_buf(),
            file,
            offset,
            len,
        };
        let mut magic = [0u8; 4];
        img.read_at(&mut magic, 32)?;
        if &magic != b"NXSB" {
            bail!(
                "{}: no APFS container found (no NXSB at partition start)",
                path.display()
            );
        }
        Ok(img)
    }

    pub fn read_at(&self, buf: &mut [u8], pos: u64) -> Result<()> {
        if pos + buf.len() as u64 > self.len {
            bail!("{}: read past end of partition at byte {pos}", self.path.display());
        }
        crate::platform::read_exact_at(&self.file, buf, self.offset + pos)
            .with_context(|| format!("reading {} at {}", self.path.display(), self.offset + pos))
    }
}

/// Finds the first APFS partition in a GPT (512-byte sectors).
fn find_apfs_partition(file: &File) -> Result<Option<(u64, u64)>> {
    let mut hdr = [0u8; 512];
    if crate::platform::read_exact_at(file, &mut hdr, 512).is_err() || &hdr[0..8] != b"EFI PART" {
        return Ok(None);
    }
    let entries_lba = le64(&hdr, 72);
    let n = le32(&hdr, 80) as usize;
    let esz = le32(&hdr, 84) as usize;
    if n == 0 || esz < 128 || n * esz > 1 << 20 {
        bail!("unexpected GPT layout ({n} entries of {esz} bytes)");
    }
    let mut tab = vec![0u8; n * esz];
    crate::platform::read_exact_at(file, &mut tab, entries_lba * 512)?;
    for e in tab.chunks_exact(esz) {
        if e[0..16] == APFS_TYPE_GUID {
            let first = le64(e, 32);
            let last = le64(e, 40);
            return Ok(Some((first * 512, (last - first + 1) * 512)));
        }
    }
    Ok(None)
}

/// One entry of the Fusion middle tree: tier-2 blocks `[t2, t2+len)` have a
/// copy on the main device starting at block `main`.
#[derive(Debug, Clone, Copy)]
pub struct CacheRange {
    pub t2: u64,
    pub main: u64,
    pub len: u64,
    pub flags: u32,
}
pub const FUSION_MT_DIRTY: u32 = 1;

/// The container's physical address space, made of one or two images.
pub struct Store {
    pub main: Image,
    pub tier2: Option<Image>,
    pub bs: u64,
    pub tier2_base: u64,
    pub damage: DamageSet,
    /// Fusion write-back cache, sorted by `t2`. Empty when not loaded/used.
    pub cache: OnceLock<Vec<CacheRange>>,
    blocks: Mutex<HashMap<u64, Arc<Vec<u8>>>>,
}

const BLOCK_CACHE_MAX: usize = 65536; // 256 MiB at 4 KiB blocks

impl Store {
    pub fn new(main: Image, tier2: Option<Image>, bs: u64) -> Store {
        let tier2_base = FUSION_TIER2_DEVICE_BYTE_ADDR >> bs.trailing_zeros();
        Store {
            main,
            tier2,
            bs,
            tier2_base,
            damage: DamageSet::default(),
            cache: OnceLock::new(),
            blocks: Mutex::new(HashMap::new()),
        }
    }

    pub fn is_tier2(&self, paddr: u64) -> bool {
        paddr >= self.tier2_base
    }

    /// Reads raw blocks straight from the device holding `paddr`
    /// (no Fusion cache redirection).
    pub fn read_device(&self, paddr: u64, buf: &mut [u8]) -> Result<()> {
        if self.is_tier2(paddr) {
            let t2 = self
                .tier2
                .as_ref()
                .context("address on Fusion tier 2, but no second image given")?;
            t2.read_at(buf, (paddr - self.tier2_base) * self.bs)
        } else {
            self.main.read_at(buf, paddr * self.bs)
        }
    }

    fn cache_entry(&self, p: u64) -> Option<CacheRange> {
        let c = self.cache.get()?;
        let i = c.partition_point(|r| r.t2 <= p);
        if i == 0 {
            return None;
        }
        let r = c[i - 1];
        (p < r.t2 + r.len).then_some(r)
    }

    /// Next cache range starting after `p`, to bound a direct read.
    fn next_cache_start(&self, p: u64) -> Option<u64> {
        let c = self.cache.get()?;
        let i = c.partition_point(|r| r.t2 <= p);
        c.get(i).map(|r| r.t2)
    }

    /// Reads `buf.len()` bytes (a whole number of blocks) starting at block
    /// `paddr`. Tier-2 blocks that have a newer (dirty) copy in the Fusion
    /// cache, or that are unreadable in the image but have a cached copy, are
    /// read from the main device instead.
    pub fn read_blocks(&self, paddr: u64, buf: &mut [u8]) -> Result<()> {
        let bs = self.bs as usize;
        debug_assert!(buf.len().is_multiple_of(bs));
        if !self.is_tier2(paddr) || self.cache.get().is_none_or(|c| c.is_empty()) {
            return self.read_device(paddr, buf);
        }
        let n = (buf.len() / bs) as u64;
        let mut i = 0u64;
        while i < n {
            let p = paddr + i;
            let (count, from_main) = match self.cache_entry(p) {
                Some(r) => {
                    let count = (r.t2 + r.len - p).min(n - i);
                    let use_main = r.flags & FUSION_MT_DIRTY != 0 || !self.damage.overlaps(p, count).is_empty();
                    (count, use_main.then_some(r.main + (p - r.t2)))
                }
                None => {
                    let lim = self.next_cache_start(p).map_or(n - i, |s| (s - p).min(n - i));
                    (lim, None)
                }
            };
            let chunk = &mut buf[(i as usize) * bs..((i + count) as usize) * bs];
            match from_main {
                Some(m) => self.read_device(m, chunk)?,
                None => self.read_device(p, chunk)?,
            }
            i += count;
        }
        Ok(())
    }

    /// Damaged blocks within `[p, p+n)` that cannot be recovered from the
    /// Fusion cache, plus the number of damaged blocks that were recovered.
    pub fn damage_report(&self, p: u64, n: u64) -> (Vec<(u64, u64)>, u64) {
        let mut bad = Vec::new();
        let mut repaired = 0;
        for (s, e) in self.damage.overlaps(p, n) {
            for b in s..e {
                if self.is_tier2(b) && self.cache_entry(b).is_some() {
                    repaired += 1;
                } else {
                    match bad.last_mut() {
                        Some((_, le)) if *le == b => *le = b + 1,
                        _ => bad.push((b, b + 1)),
                    }
                }
            }
        }
        (bad, repaired)
    }

    /// Reads one metadata block (cached) and, if `verify`, checks its checksum.
    pub fn read_object(&self, paddr: u64, verify: bool) -> Result<Arc<Vec<u8>>> {
        if let Some(b) = self.blocks.lock().unwrap().get(&paddr) {
            return Ok(b.clone());
        }
        let mut buf = vec![0u8; self.bs as usize];
        self.read_blocks(paddr, &mut buf)?;
        if verify && !checksum_ok(&buf) {
            let (bad, _) = self.damage_report(paddr, 1);
            if bad.is_empty() {
                bail!("checksum mismatch in metadata block {paddr:#x}");
            }
            bail!("metadata block {paddr:#x} lies in an unreadable area of the image");
        }
        let b = Arc::new(buf);
        let mut m = self.blocks.lock().unwrap();
        if m.len() >= BLOCK_CACHE_MAX {
            m.clear();
        }
        m.insert(paddr, b.clone());
        Ok(b)
    }
}
