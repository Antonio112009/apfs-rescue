//! APFS container (`nx_superblock_t`): checkpoint selection, object maps,
//! volume list and the Fusion middle tree.

use crate::btree::{Loc, Physical, Resolve, Tree};
use crate::device::{CacheRange, Image, Store};
use crate::obj::{OBJECT_TYPE_NX_SUPERBLOCK, OBJECT_TYPE_OMAP, ObjHeader, checksum_ok};
use crate::util::{cstr, le16, le32, le64};
use anyhow::{Context, Result, bail};
use std::cmp::Ordering;
use std::path::PathBuf;

const NX_INCOMPAT_FUSION: u64 = 0x100;
const NX_MAX_FILE_SYSTEMS: usize = 100;

/// Object map (`omap_phys_t`): virtual object id + transaction id -> block.
pub struct Omap<'a> {
    pub tree: Tree<'a, Physical>,
    pub xid: u64,
}

impl<'a> Omap<'a> {
    pub fn open(store: &'a Store, omap_paddr: u64, xid: u64) -> Result<Omap<'a>> {
        let b = store.read_object(omap_paddr, true).context("reading object map")?;
        if ObjHeader::parse(&b).otype != OBJECT_TYPE_OMAP {
            bail!("block {omap_paddr:#x} is not an object map");
        }
        let tree_oid = le64(&b, 48);
        Ok(Omap {
            tree: Tree::open(store, Loc::phys(tree_oid), Physical)?,
            xid,
        })
    }
}

impl Resolve for Omap<'_> {
    fn resolve(&self, oid: u64) -> Result<Loc> {
        let target = (oid, self.xid);
        let hit = self.tree.find_le(|k| (le64(k, 0), le64(k, 8)).cmp(&target))?;
        match hit {
            Some((k, v)) if le64(&k, 0) == oid => {
                let flags = le32(&v, 0);
                if flags & 1 != 0 {
                    bail!("object {oid:#x} is marked deleted in the object map");
                }
                if flags & 4 != 0 {
                    bail!("object {oid:#x} is encrypted (not supported)");
                }
                // OMAP_VAL_NOHEADER
                Ok(Loc {
                    paddr: le64(&v, 8),
                    noheader: flags & 8 != 0,
                })
            }
            _ => bail!("object {oid:#x} not found in the object map (xid {})", self.xid),
        }
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct VolumeInfo {
    pub index: usize,
    pub paddr: u64,
    pub name: String,
    pub role: u16,
    pub fs_flags: u64,
    pub incompat: u64,
    pub num_files: u64,
    pub num_dirs: u64,
    pub omap_oid: u64,
    pub root_tree_oid: u64,
    /// Physical address of the snapshot metadata tree (0 if none).
    pub snap_meta_tree_oid: u64,
    pub num_snapshots: u64,
}

impl VolumeInfo {
    pub fn encrypted(&self) -> bool {
        self.fs_flags & 1 == 0
    }
    pub fn hashed_names(&self) -> bool {
        // APFS_INCOMPAT_CASE_INSENSITIVE | APFS_INCOMPAT_NORMALIZATION_INSENSITIVE
        self.incompat & (0x1 | 0x8) != 0
    }
    pub fn role_name(&self) -> &'static str {
        match self.role {
            0 => "-",
            0x1 => "System",
            0x2 => "User",
            0x4 => "Recovery",
            0x8 => "VM",
            0x10 => "Preboot",
            0x20 => "Installer",
            0x40 => "Data",
            0x80 => "Baseband",
            0xc0 => "Update",
            _ => "other",
        }
    }
}

pub struct Checkpoint {
    pub xid: u64,
    pub paddr: u64,
}

pub struct Container {
    pub store: Store,
    pub sb: Vec<u8>,
    pub xid: u64,
    pub checkpoints: Vec<Checkpoint>,
    pub fusion: bool,
    pub volumes: Vec<VolumeInfo>,
}

fn read_sb(img: &Image) -> Result<Vec<u8>> {
    let mut b = vec![0u8; 4096];
    img.read_at(&mut b, 0)?;
    let bs = le32(&b, 36) as usize;
    if !(4096..=65536).contains(&bs) || !bs.is_power_of_two() {
        bail!("{}: unsupported block size {bs}", img.path.display());
    }
    if bs != b.len() {
        b = vec![0u8; bs];
        img.read_at(&mut b, 0)?;
    }
    Ok(b)
}

impl Container {
    pub fn open(paths: &[PathBuf], xid: Option<u64>) -> Result<Container> {
        let mut imgs: Vec<(Image, Vec<u8>)> = Vec::new();
        for p in paths {
            let img = Image::open(p)?;
            let sb = read_sb(&img)?;
            imgs.push((img, sb));
        }
        let fusion = le64(&imgs[0].1, 64) & NX_INCOMPAT_FUSION != 0;
        let (main, tier2) = match (imgs.len(), fusion) {
            (1, false) => (imgs.pop().unwrap().0, None),
            (1, true) => bail!("this is a Fusion container: pass both images (SSD and HDD)"),
            (2, true) => {
                let (b, a) = (imgs.pop().unwrap(), imgs.pop().unwrap());
                if a.1[72..88] != b.1[72..88] {
                    bail!("the two images belong to different containers");
                }
                // The tier-2 device's Fusion UUID differs only in the lowest bit
                // of the last byte (observed: SSD ...fc, HDD ...fd).
                let a_t2 = a.1[1295] & 1 == 1;
                let b_t2 = b.1[1295] & 1 == 1;
                if a.1[1280..1295] != b.1[1280..1295] || a_t2 == b_t2 {
                    bail!("cannot tell which image is the Fusion tier-2 (HDD) device");
                }
                if a_t2 { (b.0, Some(a.0)) } else { (a.0, Some(b.0)) }
            }
            (2, false) => bail!("two images given, but the container is not Fusion"),
            _ => bail!("give one image, or two for a Fusion drive"),
        };
        let sb0 = read_sb(&main)?;
        let bs = le32(&sb0, 36) as u64;
        let store = Store::new(main, tier2, bs);

        // Checkpoint descriptor area: pick the newest valid superblock.
        let desc_blocks = le32(&sb0, 104);
        let desc_base = le64(&sb0, 112);
        if desc_blocks & 0x8000_0000 != 0 {
            bail!("non-contiguous checkpoint area is not supported");
        }
        let mut checkpoints = Vec::new();
        let mut buf = vec![0u8; bs as usize];
        for i in 0..desc_blocks as u64 {
            if store.read_device(desc_base + i, &mut buf).is_err() || !checksum_ok(&buf) {
                continue;
            }
            let h = ObjHeader::parse(&buf);
            if h.otype == OBJECT_TYPE_NX_SUPERBLOCK && &buf[32..36] == b"NXSB" {
                checkpoints.push(Checkpoint {
                    xid: h.xid,
                    paddr: desc_base + i,
                });
            }
        }
        checkpoints.sort_by_key(|c| std::cmp::Reverse(c.xid));
        let chosen = match xid {
            Some(x) => checkpoints
                .iter()
                .find(|c| c.xid == x)
                .with_context(|| format!("no checkpoint with xid {x}"))?,
            None => checkpoints.first().context("no valid checkpoint superblock found")?,
        };
        let (xid, cp_paddr) = (chosen.xid, chosen.paddr);
        let mut sb = vec![0u8; bs as usize];
        store.read_device(cp_paddr, &mut sb)?;

        let mut c = Container {
            store,
            sb,
            xid,
            checkpoints,
            fusion,
            volumes: Vec::new(),
        };
        c.volumes = c.load_volumes()?;
        Ok(c)
    }

    pub fn omap(&self) -> Result<Omap<'_>> {
        Omap::open(&self.store, le64(&self.sb, 160), self.xid)
    }

    fn load_volumes(&self) -> Result<Vec<VolumeInfo>> {
        let omap = self.omap()?;
        let max = (le32(&self.sb, 180) as usize).min(NX_MAX_FILE_SYSTEMS);
        let mut out = Vec::new();
        for i in 0..max {
            let oid = le64(&self.sb, 184 + i * 8);
            if oid == 0 {
                continue;
            }
            let paddr = omap.resolve(oid)?.paddr;
            let b = self.store.read_object(paddr, true)?;
            if &b[32..36] != b"APSB" {
                bail!("volume superblock {paddr:#x} has no APSB magic");
            }
            out.push(VolumeInfo {
                index: i,
                paddr,
                name: cstr(&b[704..960]),
                role: le16(&b, 964),
                fs_flags: le64(&b, 264),
                incompat: le64(&b, 56),
                num_files: le64(&b, 184),
                num_dirs: le64(&b, 192),
                omap_oid: le64(&b, 128),
                root_tree_oid: le64(&b, 136),
                snap_meta_tree_oid: le64(&b, 152),
                num_snapshots: le64(&b, 216),
            });
        }
        Ok(out)
    }

    pub fn volume(&self, sel: &str) -> Result<&VolumeInfo> {
        if let Ok(i) = sel.parse::<usize>()
            && let Some(v) = self.volumes.iter().find(|v| v.index == i)
        {
            return Ok(v);
        }
        self.volumes
            .iter()
            .find(|v| v.name == sel)
            .with_context(|| format!("no volume named {sel:?} (use `info` to list volumes)"))
    }

    /// Raw entries of the Fusion middle tree (`fusion_mt_oid`).
    /// Key: tier-2 block address. Value: `fusion_mt_val_t {lba u64, length u32, flags u32}`.
    pub fn fusion_middle_tree(&self) -> Result<Vec<(u64, u64, u32, u32)>> {
        let oid = le64(&self.sb, 1352);
        if !self.fusion || oid == 0 {
            return Ok(Vec::new());
        }
        let tree = Tree::open(&self.store, Loc::phys(oid), Physical).context("opening Fusion middle tree")?;
        let mut out = Vec::new();
        for r in tree.iter_all()? {
            let (k, v) = r?;
            out.push((le64(&k, 0), le64(&v, 0), le32(&v, 8), le32(&v, 12)));
        }
        Ok(out)
    }

    /// Turns the Fusion write-back cache on for reads. `len_in_bytes` says how
    /// the `length` field is interpreted (checked with `fusion-check`).
    pub fn enable_fusion_cache(&self, len_in_bytes: bool) -> Result<usize> {
        let raw = self.fusion_middle_tree()?;
        let bs = self.store.bs;
        let mut v: Vec<CacheRange> = raw
            .into_iter()
            .map(|(t2, main, len, flags)| CacheRange {
                t2,
                main,
                len: if len_in_bytes {
                    (len as u64).div_ceil(bs)
                } else {
                    len as u64
                },
                flags,
            })
            .collect();
        v.sort_by_key(|r| r.t2);
        let n = v.len();
        let _ = self.store.cache.set(v);
        Ok(n)
    }
}

/// Compares an fs-tree key header with (oid, type).
pub fn cmp_hdr(k: &[u8], oid: u64, typ: u8) -> Ordering {
    let v = le64(k, 0);
    ((v & crate::fs::OBJ_ID_MASK), (v >> 60) as u8).cmp(&(oid, typ))
}
