//! File-system tree of one volume: inodes, directory records, extended
//! attributes, file extents and transparent (decmpfs) compression.

use crate::btree::{Loc, Physical, Tree};
use crate::container::{Container, Omap, VolumeInfo, cmp_hdr};
use crate::device::Store;
use crate::util::{be32, cstr, le16, le32, le64, slice};
use anyhow::{Context, Result, bail};
use std::cmp::Ordering;
use std::io::{Read, Write};

pub const OBJ_ID_MASK: u64 = 0x0fff_ffff_ffff_ffff;
pub const ROOT_DIR_INO: u64 = 2;

const T_INODE: u8 = 3;
const T_XATTR: u8 = 4;
const T_FILE_EXTENT: u8 = 8;
const T_DIR_REC: u8 = 9;

const INO_EXT_TYPE_DSTREAM: u8 = 8;
const UF_COMPRESSED: u32 = 0x20;
const XATTR_DATA_STREAM: u16 = 1;
const APFS_INCOMPAT_SEALED_VOLUME: u64 = 0x20;

pub const S_IFMT: u16 = 0o170000;
pub const S_IFDIR: u16 = 0o040000;
pub const S_IFLNK: u16 = 0o120000;

/// Fields not used by the tool yet are kept for library users.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct Inode {
    pub id: u64,
    pub parent: u64,
    pub private_id: u64,
    pub create_time: u64,
    pub mod_time: u64,
    pub access_time: u64,
    pub change_time: u64,
    pub bsd_flags: u32,
    pub mode: u16,
    pub uid: u32,
    pub gid: u32,
    /// Logical size from the data stream (0 if none).
    pub size: u64,
}

impl Inode {
    pub fn is_dir(&self) -> bool {
        self.mode & S_IFMT == S_IFDIR
    }
    pub fn is_compressed(&self) -> bool {
        self.bsd_flags & UF_COMPRESSED != 0
    }
}

#[derive(Debug, Clone)]
pub struct DirEntry {
    pub name: String,
    pub id: u64,
    pub dtype: u8,
}

#[derive(Debug, Clone)]
pub enum XattrData {
    Embedded(Vec<u8>),
    Stream { obj_id: u64, size: u64 },
}

#[derive(Debug, Clone)]
pub struct Xattr {
    pub name: String,
    pub data: XattrData,
}

/// A snapshot of a volume (`j_snap_metadata_val_t`).
#[derive(Debug, Clone)]
pub struct Snapshot {
    /// Transaction id when the snapshot was taken.
    pub xid: u64,
    pub name: String,
    /// Nanoseconds since 1970.
    pub create_time: u64,
    /// Physical address of the volume superblock saved with the snapshot.
    pub sblock: u64,
}

const T_SNAP_METADATA: u8 = 1;

/// Snapshots of a volume, oldest first.
pub fn snapshots(c: &Container, info: &VolumeInfo) -> Result<Vec<Snapshot>> {
    if info.snap_meta_tree_oid == 0 {
        return Ok(Vec::new());
    }
    let tree = Tree::open(&c.store, Loc::phys(info.snap_meta_tree_oid), Physical).context("snapshot metadata tree")?;
    let mut out = Vec::new();
    for r in tree.iter_all()? {
        let (k, v) = r?;
        let hdr = le64(&k, 0);
        if (hdr >> 60) as u8 != T_SNAP_METADATA || v.len() < 50 {
            continue;
        }
        let name_len = le16(&v, 48) as usize;
        out.push(Snapshot {
            xid: hdr & OBJ_ID_MASK,
            name: cstr(slice(&v, 50, name_len)?),
            create_time: le64(&v, 16),
            sblock: le64(&v, 8),
        });
    }
    out.sort_by_key(|s| (s.create_time, s.xid));
    Ok(out)
}

#[derive(Debug, Clone, Copy)]
pub struct Extent {
    pub logical: u64,
    pub len: u64,
    pub phys: u64,
}

/// Damage found while reading one file.
#[derive(Debug, Default, Clone)]
pub struct ReadReport {
    /// Byte ranges of the file `[start, end)` whose data came from unreadable
    /// areas of the image (they contain zeros).
    pub bad: Vec<(u64, u64)>,
    /// Damaged blocks that were recovered from the Fusion SSD cache.
    pub repaired_blocks: u64,
}

pub struct Fs<'a> {
    pub store: &'a Store,
    pub info: VolumeInfo,
    tree: Tree<'a, Omap<'a>>,
}

impl<'a> Fs<'a> {
    pub fn open(c: &'a Container, info: &VolumeInfo) -> Result<Fs<'a>> {
        if info.encrypted() {
            bail!(
                "volume {:?} is encrypted; this tool reads only unencrypted volumes",
                info.name
            );
        }
        if info.incompat & APFS_INCOMPAT_SEALED_VOLUME != 0 {
            // The index nodes of a sealed volume's hashed tree do not point to
            // children the way regular trees do; not implemented yet.
            bail!(
                "volume {:?} is a sealed system volume, which is not supported yet",
                info.name
            );
        }
        Self::open_at(c, info, info.omap_oid, info.root_tree_oid, c.xid)
    }

    /// Opens the volume as it was when `snap` was taken.
    pub fn open_snapshot(c: &'a Container, info: &VolumeInfo, snap: &Snapshot) -> Result<Fs<'a>> {
        if info.encrypted() {
            bail!(
                "volume {:?} is encrypted; this tool reads only unencrypted volumes",
                info.name
            );
        }
        let b = c
            .store
            .read_object(snap.sblock, true)
            .context("snapshot volume superblock")?;
        if &b[32..36] != b"APSB" {
            bail!("snapshot {:?}: its volume superblock is not valid", snap.name);
        }
        // The snapshot's saved superblock names the tree. Its object-map field
        // is empty (observed), so the volume's current object map is used: it
        // keeps the node versions as of the snapshot's transaction.
        let omap_oid = match le64(&b, 128) {
            0 => info.omap_oid,
            o => o,
        };
        Self::open_at(c, info, omap_oid, le64(&b, 136), snap.xid)
    }

    fn open_at(c: &'a Container, info: &VolumeInfo, omap_oid: u64, root_tree_oid: u64, xid: u64) -> Result<Fs<'a>> {
        let omap = Omap::open(&c.store, omap_oid, xid).context("volume object map")?;
        let root = crate::btree::Resolve::resolve(&omap, root_tree_oid)?;
        let tree = Tree::open(&c.store, root, omap).context("volume file-system tree")?;
        Ok(Fs {
            store: &c.store,
            info: info.clone(),
            tree,
        })
    }

    fn records(&self, oid: u64, typ: u8) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let mut out = Vec::new();
        for r in self.tree.iter_from(|k| cmp_hdr(k, oid, typ))? {
            let (k, v) = r?;
            match cmp_hdr(&k, oid, typ) {
                Ordering::Less => continue,
                Ordering::Equal => out.push((k, v)),
                Ordering::Greater => break,
            }
        }
        Ok(out)
    }

    pub fn inode(&self, id: u64) -> Result<Inode> {
        let recs = self.records(id, T_INODE)?;
        let (_, v) = recs.first().with_context(|| format!("inode {id} not found"))?;
        parse_inode(id, v)
    }

    pub fn readdir(&self, dir: u64) -> Result<Vec<DirEntry>> {
        let mut out = Vec::new();
        for (k, v) in self.records(dir, T_DIR_REC)? {
            let name = if self.info.hashed_names() {
                let len = (le32(&k, 8) & 0x3ff) as usize;
                cstr(slice(&k, 12, len)?)
            } else {
                let len = le16(&k, 8) as usize;
                cstr(slice(&k, 10, len)?)
            };
            if v.len() < 18 {
                bail!("corrupt directory record in {dir}");
            }
            out.push(DirEntry {
                name,
                id: le64(&v, 0),
                dtype: (le16(&v, 16) & 0xf) as u8,
            });
        }
        Ok(out)
    }

    pub fn xattrs(&self, id: u64) -> Result<Vec<Xattr>> {
        let mut out = Vec::new();
        for (k, v) in self.records(id, T_XATTR)? {
            let nlen = le16(&k, 8) as usize;
            let name = cstr(slice(&k, 10, nlen)?);
            let flags = le16(&v, 0);
            let dlen = le16(&v, 2) as usize;
            let d = slice(&v, 4, dlen)?;
            let data = if flags & XATTR_DATA_STREAM != 0 {
                XattrData::Stream {
                    obj_id: le64(d, 0),
                    size: le64(d, 8),
                }
            } else {
                XattrData::Embedded(d.to_vec())
            };
            out.push(Xattr { name, data });
        }
        Ok(out)
    }

    pub fn extents(&self, stream_id: u64) -> Result<Vec<Extent>> {
        let mut out = Vec::new();
        for (k, v) in self.records(stream_id, T_FILE_EXTENT)? {
            out.push(Extent {
                logical: le64(&k, 8),
                len: le64(&v, 0) & 0x00ff_ffff_ffff_ffff,
                phys: le64(&v, 8),
            });
        }
        out.sort_by_key(|e| e.logical);
        Ok(out)
    }

    /// Walks a path like `Users/NAME/Pictures` from the volume root.
    pub fn lookup(&self, path: &str) -> Result<u64> {
        let mut id = ROOT_DIR_INO;
        for comp in path.split('/').filter(|c| !c.is_empty()) {
            let entries = self.readdir(id)?;
            let hit = entries
                .iter()
                .find(|e| e.name == comp)
                .or_else(|| entries.iter().find(|e| names_equal(&e.name, comp)));
            id = hit.with_context(|| format!("{comp:?} not found in path {path:?}"))?.id;
        }
        Ok(id)
    }

    /// Streams `size` bytes of data stream `stream_id` into `out`.
    pub fn read_stream(&self, stream_id: u64, size: u64, out: &mut dyn Write, rep: &mut ReadReport) -> Result<()> {
        self.read_stream_range(stream_id, size, 0, size, out, rep)
    }

    /// Streams bytes `[start, start+len)` (clipped to `size`) of data stream
    /// `stream_id` into `out`. Holes and missing extents read as zeros; byte
    /// ranges that come from unreadable areas are added to `rep`.
    pub fn read_stream_range(
        &self,
        stream_id: u64,
        size: u64,
        start: u64,
        len: u64,
        out: &mut dyn Write,
        rep: &mut ReadReport,
    ) -> Result<()> {
        let bs = self.store.bs;
        let end = start.saturating_add(len).min(size);
        let mut pos = start;
        if pos >= end {
            return Ok(());
        }
        let mut buf = Vec::new();
        for e in self.extents(stream_id)? {
            let ext_end = e.logical.saturating_add(e.len).min(end);
            if ext_end <= pos {
                continue;
            }
            if e.logical >= end {
                break;
            }
            if e.logical > pos {
                write_zeros(out, e.logical - pos)?; // sparse gap
                pos = e.logical;
            }
            while pos < ext_end {
                let off = pos - e.logical;
                let skip = (off % bs) as usize;
                let chunk = (ext_end - pos).min(8 << 20);
                if e.phys == 0 {
                    write_zeros(out, chunk)?;
                } else {
                    let p = e.phys + off / bs;
                    let nblk = (skip as u64 + chunk).div_ceil(bs);
                    buf.resize((nblk * bs) as usize, 0);
                    self.store.read_blocks(p, &mut buf)?;
                    out.write_all(&buf[skip..skip + chunk as usize])?;
                    let (bad, repaired) = self.store.damage_report(p, nblk);
                    rep.repaired_blocks += repaired;
                    for (s, x) in bad {
                        // File byte range of the bad blocks, clipped to what was read.
                        let fs = (e.logical + (s - e.phys) * bs).max(pos);
                        let fe = (e.logical + (x - e.phys) * bs).min(pos + chunk);
                        if fs >= fe {
                            continue;
                        }
                        match rep.bad.last_mut() {
                            Some(l) if l.1 == fs => l.1 = fe,
                            _ => rep.bad.push((fs, fe)),
                        }
                    }
                }
                pos += chunk;
            }
        }
        if pos < end {
            write_zeros(out, end - pos)?;
        }
        Ok(())
    }

    pub fn xattr_bytes(&self, x: &Xattr, rep: &mut ReadReport) -> Result<Vec<u8>> {
        match &x.data {
            XattrData::Embedded(d) => Ok(d.clone()),
            XattrData::Stream { obj_id, size } => {
                let mut v = Vec::with_capacity(*size as usize);
                self.read_stream(*obj_id, *size, &mut v, rep)?;
                Ok(v)
            }
        }
    }

    /// Writes the contents of a regular file. Returns the number of bytes written.
    pub fn read_file(&self, ino: &Inode, out: &mut dyn Write, rep: &mut ReadReport) -> Result<u64> {
        if ino.is_compressed() {
            let xs = self.xattrs(ino.id)?;
            if let Some(d) = xs.iter().find(|x| x.name == "com.apple.decmpfs") {
                let hdr = self.xattr_bytes(d, rep)?;
                let rsrc = |rep: &mut ReadReport| -> Result<Vec<u8>> {
                    let x = xs
                        .iter()
                        .find(|x| x.name == "com.apple.ResourceFork")
                        .context("compressed file without resource fork")?;
                    self.xattr_bytes(x, rep)
                };
                let data = decmpfs(&hdr, rsrc, rep)?;
                out.write_all(&data)?;
                return Ok(data.len() as u64);
            }
        }
        self.read_stream(ino.private_id, ino.size, out, rep)?;
        Ok(ino.size)
    }

    /// Size of the file contents as a reader sees them (the uncompressed size
    /// for files that macOS stores compressed).
    pub fn logical_size(&self, ino: &Inode) -> Result<u64> {
        if ino.is_compressed()
            && let Some(d) = self.xattrs(ino.id)?.iter().find(|x| x.name == "com.apple.decmpfs")
        {
            let hdr = self.xattr_bytes(d, &mut ReadReport::default())?;
            if hdr.len() >= 16 {
                return Ok(le64(&hdr, 8));
            }
        }
        Ok(ino.size)
    }

    pub fn symlink_target(&self, id: u64) -> Result<String> {
        let xs = self.xattrs(id)?;
        let x = xs
            .iter()
            .find(|x| x.name == "com.apple.fs.symlink")
            .context("symlink without target")?;
        Ok(cstr(&self.xattr_bytes(x, &mut ReadReport::default())?))
    }
}

/// Extended attributes that belong to the user (Finder tags, comments...), as
/// opposed to ones that describe how APFS stores the file.
pub fn user_xattr(name: &str, compressed: bool) -> bool {
    match name {
        "com.apple.decmpfs" | "com.apple.quarantine" => false,
        "com.apple.ResourceFork" => !compressed, // holds the compressed data
        n => !n.starts_with("com.apple.fs.") && !n.starts_with("com.apple.system."),
    }
}

fn parse_inode(id: u64, v: &[u8]) -> Result<Inode> {
    if v.len() < 92 {
        bail!("inode {id}: record too short");
    }
    let mut ino = Inode {
        id,
        parent: le64(v, 0),
        private_id: le64(v, 8),
        create_time: le64(v, 16),
        mod_time: le64(v, 24),
        access_time: le64(v, 40),
        change_time: le64(v, 32),
        bsd_flags: le32(v, 68),
        uid: le32(v, 72),
        gid: le32(v, 76),
        mode: le16(v, 80),
        size: 0,
    };
    // Extended fields (xf_blob_t).
    if v.len() >= 96 {
        let n = le16(v, 92) as usize;
        let mut data = 96 + n * 4;
        for i in 0..n {
            let h = 96 + i * 4;
            if h + 4 > v.len() {
                break;
            }
            let (t, sz) = (v[h], le16(v, h + 2) as usize);
            if t == INO_EXT_TYPE_DSTREAM && data + 8 <= v.len() {
                ino.size = le64(v, data);
            }
            data += (sz + 7) & !7;
        }
    }
    Ok(ino)
}

fn write_zeros(out: &mut dyn Write, mut n: u64) -> Result<()> {
    let z = [0u8; 65536];
    while n > 0 {
        let k = n.min(z.len() as u64) as usize;
        out.write_all(&z[..k])?;
        n -= k as u64;
    }
    Ok(())
}

/// Compares names ignoring Unicode normalization form and case.
pub fn names_equal(a: &str, b: &str) -> bool {
    use unicode_normalization::UnicodeNormalization;
    let na: String = a.nfd().collect::<String>().to_lowercase();
    let nb: String = b.nfd().collect::<String>().to_lowercase();
    na == nb
}

// ---------------------------------------------------------------- decmpfs

const DECMPFS_MAGIC: u32 = 0x636d_7066; // "fpmc" on disk
const CHUNK: usize = 65536;

fn zlib(src: &[u8]) -> Result<Vec<u8>> {
    let mut v = Vec::new();
    flate2::read::ZlibDecoder::new(src)
        .read_to_end(&mut v)
        .context("zlib data is corrupt")?;
    Ok(v)
}

fn lzfse(src: &[u8]) -> Result<Vec<u8>> {
    let mut v = Vec::new();
    lzfse_rust::decode_bytes(src, &mut v).map_err(|e| anyhow::anyhow!("LZFSE data is corrupt: {e}"))?;
    Ok(v)
}

/// Raw LZVN data is decoded by wrapping it in an LZFSE "bvxn" block.
fn lzvn(src: &[u8], raw_len: usize) -> Result<Vec<u8>> {
    let mut s = Vec::with_capacity(src.len() + 16);
    s.extend_from_slice(b"bvxn");
    s.extend_from_slice(&(raw_len as u32).to_le_bytes());
    s.extend_from_slice(&(src.len() as u32).to_le_bytes());
    s.extend_from_slice(src);
    s.extend_from_slice(b"bvx$");
    lzfse(&s)
}

fn decmpfs(hdr: &[u8], rsrc: impl FnOnce(&mut ReadReport) -> Result<Vec<u8>>, rep: &mut ReadReport) -> Result<Vec<u8>> {
    if hdr.len() < 16 || le32(hdr, 0) != DECMPFS_MAGIC {
        bail!("bad decmpfs header");
    }
    let typ = le32(hdr, 4);
    let size = le64(hdr, 8) as usize;
    let d = &hdr[16..];
    let out = match typ {
        1 => d.to_vec(),
        3 => {
            if d.first().is_some_and(|b| b & 0x0f == 0x0f) {
                d[1..].to_vec()
            } else {
                zlib(d)?
            }
        }
        7 => {
            if d.first() == Some(&0x06) {
                d[1..].to_vec()
            } else {
                lzvn(d, size)?
            }
        }
        11 => lzfse(d)?,
        4 => {
            let rf = rsrc(rep)?;
            let data_off = be32(&rf, 0) as usize;
            let res = slice(&rf, data_off + 4, rf.len().saturating_sub(data_off + 4))?;
            let n = le32(res, 0) as usize;
            let mut out = Vec::with_capacity(size);
            for i in 0..n {
                let off = le32(res, 4 + i * 8) as usize;
                let len = le32(res, 8 + i * 8) as usize;
                let c = slice(res, off, len)?;
                if c.first() == Some(&0xff) {
                    out.extend_from_slice(&c[1..])
                } else {
                    out.extend(zlib(c)?)
                }
            }
            out
        }
        8 | 12 => {
            let rf = rsrc(rep)?;
            let n = size.div_ceil(CHUNK);
            let mut out = Vec::with_capacity(size);
            for i in 0..n {
                let (s, e) = (le32(&rf, i * 4) as usize, le32(&rf, i * 4 + 4) as usize);
                let c = slice(&rf, s, e.saturating_sub(s))?;
                let raw_len = CHUNK.min(size - i * CHUNK);
                if typ == 8 {
                    if c.first() == Some(&0x06) {
                        out.extend_from_slice(&c[1..])
                    } else {
                        out.extend(lzvn(c, raw_len)?)
                    }
                } else {
                    out.extend(lzfse(c)?)
                }
            }
            out
        }
        t => bail!("macOS compression type {t} is not supported yet; file not copied"),
    };
    if out.len() != size {
        bail!("decompressed size {} differs from expected {size}", out.len());
    }
    Ok(out)
}
