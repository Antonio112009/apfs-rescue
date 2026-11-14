//! Read-only view of a volume for `mount`: folders, file attributes and file
//! contents by APFS object id, with caches. The FUSE (Linux) and NFS (macOS)
//! front ends both sit on top of this.

use crate::fs::{DirEntry, Fs, Inode, ReadReport, S_IFDIR, S_IFLNK, S_IFMT, Xattr, user_xattr};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

/// Decompressed contents of compressed files kept in memory.
const COMPRESSED_CACHE_BYTES: usize = 256 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Dir,
    File,
    Symlink,
}

#[derive(Debug)]
pub enum ViewError {
    NotFound,
    /// Reading the image failed (details already printed).
    Io,
}

pub type VResult<T> = std::result::Result<T, ViewError>;

/// What a file manager needs to know about one item. Times in ns since 1970.
#[derive(Debug, Clone, Copy)]
pub struct Attr {
    pub id: u64,
    pub kind: Kind,
    pub size: u64,
    /// Permission bits, read-only and always readable by the viewer.
    pub perm: u16,
    pub atime: u64,
    pub mtime: u64,
    pub ctime: u64,
    /// Creation time (FUSE can show it; NFSv3 has no field for it).
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub crtime: u64,
}

/// Total bytes held, and decompressed contents by file id.
type ByteCache = (usize, HashMap<u64, Arc<Vec<u8>>>);

pub struct View {
    pub fs: Fs<'static>,
    /// APFS id of the folder shown as the top of the mounted disk.
    pub root: u64,
    attrs: Mutex<HashMap<u64, (Attr, Inode)>>,
    dirs: Mutex<HashMap<u64, Arc<Vec<DirEntry>>>>,
    /// id -> (parent id, name), learned while browsing, to name files in warnings.
    names: Mutex<HashMap<u64, (u64, String)>>,
    compressed: Mutex<ByteCache>,
    warned: Mutex<HashSet<u64>>,
}

fn io_err<E: std::fmt::Display>(what: &str) -> impl FnOnce(E) -> ViewError + '_ {
    move |e| {
        eprintln!("Read error ({what}): {e:#}");
        ViewError::Io
    }
}

/// Names compare equal when they differ only in Unicode normalization
/// (macOS hands names to file systems in decomposed form).
fn same_name(a: &str, b: &str) -> bool {
    use unicode_normalization::UnicodeNormalization;
    a == b || a.nfc().eq(b.nfc())
}

impl View {
    pub fn new(fs: Fs<'static>, root: u64) -> View {
        View {
            fs,
            root,
            attrs: Mutex::default(),
            dirs: Mutex::default(),
            names: Mutex::default(),
            compressed: Mutex::default(),
            warned: Mutex::default(),
        }
    }

    pub fn attr(&self, id: u64) -> VResult<Attr> {
        Ok(self.entry(id)?.0)
    }

    fn entry(&self, id: u64) -> VResult<(Attr, Inode)> {
        if let Some(a) = self.attrs.lock().unwrap().get(&id) {
            return Ok(a.clone());
        }
        let ino = self.fs.inode(id).map_err(io_err("inode"))?;
        let (kind, size) = match ino.mode & S_IFMT {
            S_IFDIR => (Kind::Dir, 0),
            S_IFLNK => (
                Kind::Symlink,
                self.fs.symlink_target(id).map(|t| t.len() as u64).unwrap_or(0),
            ),
            _ => (Kind::File, self.fs.logical_size(&ino).map_err(io_err("size"))?),
        };
        let perm = ((ino.mode & 0o777) | if kind == Kind::Dir { 0o555 } else { 0o444 }) & !0o222;
        let attr = Attr {
            id,
            kind,
            size,
            perm,
            atime: ino.access_time,
            mtime: ino.mod_time,
            ctime: ino.change_time,
            crtime: ino.create_time,
        };
        self.attrs.lock().unwrap().insert(id, (attr, ino.clone()));
        Ok((attr, ino))
    }

    /// Folder contents, sorted by name.
    pub fn dir(&self, id: u64) -> VResult<Arc<Vec<DirEntry>>> {
        if let Some(d) = self.dirs.lock().unwrap().get(&id) {
            return Ok(d.clone());
        }
        let mut list = self.fs.readdir(id).map_err(io_err("folder"))?;
        list.sort_by(|a, b| a.name.cmp(&b.name));
        {
            let mut names = self.names.lock().unwrap();
            for e in &list {
                names.entry(e.id).or_insert((id, e.name.clone()));
            }
        }
        let list = Arc::new(list);
        self.dirs.lock().unwrap().insert(id, list.clone());
        Ok(list)
    }

    pub fn lookup(&self, parent: u64, name: &str) -> VResult<u64> {
        let list = self.dir(parent)?;
        let e = list
            .iter()
            .find(|e| e.name == name)
            .or_else(|| list.iter().find(|e| same_name(&e.name, name)))
            .ok_or(ViewError::NotFound)?;
        Ok(e.id)
    }

    pub fn parent(&self, id: u64) -> u64 {
        self.names.lock().unwrap().get(&id).map(|p| p.0).unwrap_or(self.root)
    }

    pub fn path_of(&self, mut id: u64) -> String {
        let names = self.names.lock().unwrap();
        let mut parts = Vec::new();
        while id != self.root {
            match names.get(&id) {
                Some((p, n)) => {
                    parts.push(n.clone());
                    id = *p;
                }
                None => {
                    parts.push(format!("<file {id}>"));
                    break;
                }
            }
        }
        parts.reverse();
        parts.join("/")
    }

    /// Up to `len` bytes of a file from `offset`. Unreadable areas read as
    /// zeros and are reported once per file on the terminal.
    pub fn read(&self, id: u64, offset: u64, len: u32) -> VResult<Vec<u8>> {
        let (attr, ino) = self.entry(id)?;
        if ino.is_compressed() {
            let d = self.decompressed(&ino)?;
            let s = (offset as usize).min(d.len());
            let e = (s + len as usize).min(d.len());
            return Ok(d[s..e].to_vec());
        }
        let mut buf = Vec::with_capacity(len as usize);
        let mut rep = ReadReport::default();
        self.fs
            .read_stream_range(ino.private_id, attr.size, offset, len as u64, &mut buf, &mut rep)
            .map_err(|e| {
                eprintln!("Read error in {}: {e:#}", self.path_of(id));
                ViewError::Io
            })?;
        self.warn_damage(id, &rep);
        Ok(buf)
    }

    fn decompressed(&self, ino: &Inode) -> VResult<Arc<Vec<u8>>> {
        if let Some(d) = self.compressed.lock().unwrap().1.get(&ino.id) {
            return Ok(d.clone());
        }
        let mut v = Vec::new();
        self.fs
            .read_file(ino, &mut v, &mut ReadReport::default())
            .map_err(io_err("compressed file"))?;
        let v = Arc::new(v);
        let mut c = self.compressed.lock().unwrap();
        if c.0 + v.len() > COMPRESSED_CACHE_BYTES {
            c.1.clear();
            c.0 = 0;
        }
        c.0 += v.len();
        c.1.insert(ino.id, v.clone());
        Ok(v)
    }

    fn warn_damage(&self, id: u64, rep: &ReadReport) {
        if rep.bad.is_empty() || !self.warned.lock().unwrap().insert(id) {
            return;
        }
        eprintln!(
            "Warning: {} has unreadable parts (bytes {}); they read as zeros.",
            self.path_of(id),
            crate::extract::fmt_ranges(&rep.bad)
        );
    }

    pub fn readlink(&self, id: u64) -> VResult<String> {
        self.fs.symlink_target(id).map_err(io_err("symbolic link"))
    }

    /// Extended attributes that belong to the user (Finder tags, comments...).
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub fn xattrs(&self, id: u64) -> VResult<Vec<Xattr>> {
        let (_, ino) = self.entry(id)?;
        let list = self.fs.xattrs(id).map_err(io_err("attributes"))?;
        Ok(list
            .into_iter()
            .filter(|x| user_xattr(&x.name, ino.is_compressed()))
            .collect())
    }

    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub fn xattr_value(&self, x: &Xattr) -> VResult<Vec<u8>> {
        self.fs
            .xattr_bytes(x, &mut ReadReport::default())
            .map_err(io_err("attribute"))
    }
}
