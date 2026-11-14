//! `mount`: shows a volume (or one folder of it) as a read-only disk, so it can
//! be browsed in Finder or a file manager. Uses FUSE: macFUSE on macOS, the
//! kernel's FUSE on Linux.

use crate::fs::{Fs, Inode, ReadReport, S_IFDIR, S_IFLNK, S_IFMT, user_xattr};
use anyhow::{Context, Result, bail};
use fuser::{
    Config, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo, LockOwner, MountOption,
    OpenFlags, ReplyAttr, ReplyData, ReplyDirectory, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyXattr, Request,
};
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The image never changes while mounted, so the kernel may cache freely.
const TTL: Duration = Duration::from_secs(3600);
/// Decompressed contents of compressed files kept in memory.
const COMPRESSED_CACHE_BYTES: usize = 256 << 20;

/// Where macFUSE puts its library; used to give a clear message when missing.
#[cfg(target_os = "macos")]
const MACFUSE_LIB: &str = "/usr/local/lib/libfuse.2.dylib";

pub const INSTALL_HELP: &str = "\
`mount` needs FUSE:
  macOS:  macFUSE 5.5 or later (free): `brew install --cask macfuse` or
          https://macfuse.github.io, then turn it on once in System Settings
          (the command explains how if it is still off).
  Linux:  install the fuse3 package (e.g. `sudo apt install fuse3`).
  Windows: not supported yet; use `copy` instead.";

/// Total bytes held, and decompressed contents by file id.
type ByteCache = (usize, HashMap<u64, Arc<Vec<u8>>>);

pub struct RescueFs {
    fs: Fs<'static>,
    /// APFS id of the folder shown as the top of the mounted disk.
    root: u64,
    uid: u32,
    gid: u32,
    total_blocks: u64,
    files: u64,
    dirs: Mutex<HashMap<u64, Arc<Vec<crate::fs::DirEntry>>>>,
    attrs: Mutex<HashMap<u64, (FileAttr, Inode)>>,
    /// id -> (parent id, name), learned from lookups, to name files in warnings.
    names: Mutex<HashMap<u64, (u64, String)>>,
    compressed: Mutex<ByteCache>,
    warned: Mutex<HashSet<u64>>,
}

fn time(ns: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_nanos(ns)
}

fn eio<E: std::fmt::Display>(what: &str) -> impl FnOnce(E) -> Errno + '_ {
    move |e| {
        eprintln!("Read error ({what}): {e:#}");
        Errno::EIO
    }
}

/// Names compare equal when they differ only in Unicode normalization
/// (macOS hands Finder names in decomposed form).
fn same_name(a: &str, b: &str) -> bool {
    use unicode_normalization::UnicodeNormalization;
    a == b || a.nfc().eq(b.nfc())
}

impl RescueFs {
    pub fn new(fs: Fs<'static>, root: u64, total_blocks: u64) -> RescueFs {
        // SAFETY: getuid/getgid have no preconditions.
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        let files = fs.info.num_files + fs.info.num_dirs;
        RescueFs {
            fs,
            root,
            uid,
            gid,
            total_blocks,
            files,
            dirs: Mutex::default(),
            attrs: Mutex::default(),
            names: Mutex::default(),
            compressed: Mutex::default(),
            warned: Mutex::default(),
        }
    }

    // FUSE reserves inode 1 for the root; APFS uses 1 only internally.
    fn to_fuse(&self, id: u64) -> INodeNo {
        INodeNo(if id == self.root { 1 } else { id })
    }
    fn apfs_id(&self, ino: INodeNo) -> u64 {
        if ino.0 == 1 { self.root } else { ino.0 }
    }

    fn entry(&self, id: u64) -> Result<(FileAttr, Inode), Errno> {
        if let Some(a) = self.attrs.lock().unwrap().get(&id) {
            return Ok(a.clone());
        }
        let ino = self.fs.inode(id).map_err(eio("inode"))?;
        let (kind, size) = match ino.mode & S_IFMT {
            S_IFDIR => (FileType::Directory, 0),
            S_IFLNK => (
                FileType::Symlink,
                self.fs.symlink_target(id).map(|t| t.len() as u64).unwrap_or(0),
            ),
            _ => (FileType::RegularFile, self.fs.logical_size(&ino).map_err(eio("size"))?),
        };
        let dir = kind == FileType::Directory;
        // Read-only, and readable by the person who mounted it.
        let perm = ((ino.mode & 0o777) | if dir { 0o555 } else { 0o444 }) & !0o222;
        let attr = FileAttr {
            ino: self.to_fuse(id),
            size,
            blocks: size.div_ceil(512),
            atime: time(ino.access_time),
            mtime: time(ino.mod_time),
            ctime: time(ino.change_time),
            crtime: time(ino.create_time),
            kind,
            perm,
            nlink: if dir { 2 } else { 1 },
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        };
        self.attrs.lock().unwrap().insert(id, (attr, ino.clone()));
        Ok((attr, ino))
    }

    fn dir(&self, id: u64) -> Result<Arc<Vec<crate::fs::DirEntry>>, Errno> {
        if let Some(d) = self.dirs.lock().unwrap().get(&id) {
            return Ok(d.clone());
        }
        let mut list = self.fs.readdir(id).map_err(eio("folder"))?;
        list.sort_by(|a, b| a.name.cmp(&b.name));
        let list = Arc::new(list);
        self.dirs.lock().unwrap().insert(id, list.clone());
        Ok(list)
    }

    fn path_of(&self, mut id: u64) -> String {
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

    fn decompressed(&self, ino: &Inode) -> Result<Arc<Vec<u8>>, Errno> {
        if let Some(d) = self.compressed.lock().unwrap().1.get(&ino.id) {
            return Ok(d.clone());
        }
        let mut v = Vec::new();
        self.fs
            .read_file(ino, &mut v, &mut ReadReport::default())
            .map_err(eio("compressed file"))?;
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

    fn xattr_list(&self, id: u64) -> Result<Vec<crate::fs::Xattr>, Errno> {
        let (_, ino) = self.entry(id)?;
        let list = self.fs.xattrs(id).map_err(eio("attributes"))?;
        Ok(list
            .into_iter()
            .filter(|x| user_xattr(&x.name, ino.is_compressed()))
            .collect())
    }
}

fn reply_sized(reply: ReplyXattr, size: u32, data: &[u8]) {
    if size == 0 {
        reply.size(data.len() as u32);
    } else if data.len() <= size as usize {
        reply.data(data);
    } else {
        reply.error(Errno::ERANGE);
    }
}

impl Filesystem for RescueFs {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let parent = self.apfs_id(parent);
        let name = name.to_string_lossy();
        let res = self.dir(parent).and_then(|list| {
            let e = list
                .iter()
                .find(|e| e.name == name)
                .or_else(|| list.iter().find(|e| same_name(&e.name, &name)))
                .ok_or(Errno::ENOENT)?;
            self.names.lock().unwrap().insert(e.id, (parent, e.name.clone()));
            self.entry(e.id)
        });
        match res {
            Ok((attr, _)) => reply.entry(&TTL, &attr, Generation(0)),
            Err(e) => reply.error(e),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        match self.entry(self.apfs_id(ino)) {
            Ok((attr, _)) => reply.attr(&TTL, &attr),
            Err(e) => reply.error(e),
        }
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        match self.fs.symlink_target(self.apfs_id(ino)) {
            Ok(t) => reply.data(t.as_bytes()),
            Err(_) => reply.error(Errno::EIO),
        }
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        if flags.0 & libc::O_ACCMODE != libc::O_RDONLY {
            return reply.error(Errno::EROFS);
        }
        match self.entry(self.apfs_id(ino)) {
            Ok(_) => reply.opened(FileHandle(0), FopenFlags::FOPEN_KEEP_CACHE),
            Err(e) => reply.error(e),
        }
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let id = self.apfs_id(ino);
        let (attr, inode) = match self.entry(id) {
            Ok(x) => x,
            Err(e) => return reply.error(e),
        };
        if inode.is_compressed() {
            return match self.decompressed(&inode) {
                Ok(d) => {
                    let s = (offset as usize).min(d.len());
                    let e = (s + size as usize).min(d.len());
                    reply.data(&d[s..e])
                }
                Err(e) => reply.error(e),
            };
        }
        let mut buf = Vec::with_capacity(size as usize);
        let mut rep = ReadReport::default();
        match self
            .fs
            .read_stream_range(inode.private_id, attr.size, offset, size as u64, &mut buf, &mut rep)
        {
            Ok(()) => {
                self.warn_damage(id, &rep);
                reply.data(&buf)
            }
            Err(e) => {
                eprintln!("Read error in {}: {e:#}", self.path_of(id));
                reply.error(Errno::EIO)
            }
        }
    }

    fn readdir(&self, _req: &Request, ino: INodeNo, _fh: FileHandle, offset: u64, mut reply: ReplyDirectory) {
        let id = self.apfs_id(ino);
        let list = match self.dir(id) {
            Ok(l) => l,
            Err(e) => return reply.error(e),
        };
        let parent = self.names.lock().unwrap().get(&id).map(|p| p.0).unwrap_or(self.root);
        let mut all: Vec<(INodeNo, FileType, &str)> = vec![
            (ino, FileType::Directory, "."),
            (self.to_fuse(parent), FileType::Directory, ".."),
        ];
        for e in list.iter() {
            let kind = match e.dtype {
                4 => FileType::Directory,
                10 => FileType::Symlink,
                _ => FileType::RegularFile,
            };
            all.push((self.to_fuse(e.id), kind, &e.name));
            self.names.lock().unwrap().entry(e.id).or_insert((id, e.name.clone()));
        }
        for (i, (ino, kind, name)) in all.into_iter().enumerate().skip(offset as usize) {
            if reply.add(ino, (i + 1) as u64, kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        reply.statfs(
            self.total_blocks,
            0,
            0,
            self.files,
            0,
            self.fs.store.bs as u32,
            255,
            self.fs.store.bs as u32,
        );
    }

    fn getxattr(&self, _req: &Request, ino: INodeNo, name: &OsStr, size: u32, reply: ReplyXattr) {
        let id = self.apfs_id(ino);
        let res = self.xattr_list(id).and_then(|l| {
            let x = l
                .iter()
                .find(|x| *x.name == *name.to_string_lossy())
                .ok_or(Errno::ENOATTR)?;
            self.fs
                .xattr_bytes(x, &mut ReadReport::default())
                .map_err(eio("attribute"))
        });
        match res {
            Ok(v) => reply_sized(reply, size, &v),
            Err(e) => reply.error(e),
        }
    }

    fn listxattr(&self, _req: &Request, ino: INodeNo, size: u32, reply: ReplyXattr) {
        match self.xattr_list(self.apfs_id(ino)) {
            Ok(l) => {
                let mut names = Vec::new();
                for x in l {
                    names.extend_from_slice(x.name.as_bytes());
                    names.push(0);
                }
                reply_sized(reply, size, &names)
            }
            Err(e) => reply.error(e),
        }
    }
}

/// Checks that FUSE is available before trying to mount.
pub fn check_fuse() -> Result<()> {
    #[cfg(target_os = "macos")]
    if !Path::new(MACFUSE_LIB).exists() {
        bail!("macFUSE is not installed.\n\n{INSTALL_HELP}");
    }
    #[cfg(target_os = "linux")]
    if !Path::new("/dev/fuse").exists() {
        bail!("FUSE is not available (/dev/fuse is missing).\n\n{INSTALL_HELP}");
    }
    Ok(())
}

#[cfg(target_os = "macos")]
const MACOS_ENABLE_HELP: &str = "\
macOS has not allowed macFUSE to run yet. Turn it on once:
  1. Use macFUSE 5.5.0 or later (read-only mounts need it):
       brew install --cask macfuse      (or https://macfuse.github.io)
  2. Open the macFUSE app once:
       open /Library/Filesystems/macfuse.fs/Contents/Resources/macfuse.app
  3. System Settings > General > Login Items & Extensions. Switch the
     Extensions list to \"By Category\", open File System Extensions (i) and
     turn on macFUSE. (The switch in the \"By Application\" view does not
     react: a known macOS bug, macfuse/macfuse#1194.)
  4. If it says \"File system extension not found\", macOS has not noticed the
     switch yet: eject external disks and restart the Mac.
Then run the same command again.";

/// macFUSE has two ways to run: its kernel extension (creates /dev/macfuse*
/// when loaded) and Apple's FSKit (macOS 15.4 and later, no kernel extension).
#[cfg(target_os = "macos")]
fn macfuse_kext_loaded() -> bool {
    std::fs::read_dir("/dev")
        .map(|d| {
            d.flatten()
                .any(|e| e.file_name().to_string_lossy().starts_with("macfuse"))
        })
        .unwrap_or(false)
}

/// Mounts read-only at `mountpoint` and waits until it is unmounted (Ctrl+C,
/// or Eject in Finder).
pub fn run(fs: RescueFs, mountpoint: &Path, volname: &str) -> Result<()> {
    check_fuse()?;
    let mut opts = vec![
        MountOption::RO,
        MountOption::FSName("apfs-rescue".into()),
        MountOption::NoExec,
    ];
    #[cfg(target_os = "macos")]
    {
        opts.push(MountOption::CUSTOM(format!("volname={}", volname.replace(',', " "))));
        opts.push(MountOption::CUSTOM("noappledouble".into()));
        if !macfuse_kext_loaded() {
            opts.push(MountOption::CUSTOM("backend=fskit".into()));
        }
    }
    // Extra FUSE options for special setups, comma-separated.
    if let Ok(extra) = std::env::var("APFS_RESCUE_FUSE_OPTS") {
        opts.extend(
            extra
                .split(',')
                .filter(|o| !o.is_empty())
                .map(|o| MountOption::CUSTOM(o.to_string())),
        );
    }
    std::fs::create_dir_all(mountpoint).with_context(|| format!("creating {}", mountpoint.display()))?;
    let mut config = Config::default();
    config.mount_options = opts;
    config.n_threads = Some(4);
    let session = match fuser::spawn_mount(fs, mountpoint, &config) {
        Ok(s) => s,
        #[cfg(target_os = "macos")]
        Err(e) => {
            let _ = std::fs::remove_dir(mountpoint);
            bail!("could not mount ({e}).\n\n{MACOS_ENABLE_HELP}");
        }
        #[cfg(not(target_os = "macos"))]
        Err(e) => {
            let _ = std::fs::remove_dir(mountpoint);
            bail!("could not mount at {} ({e}).\n\n{INSTALL_HELP}", mountpoint.display());
        }
    };

    let mp = mountpoint.to_path_buf();
    ctrlc::set_handler(move || {
        eprintln!("\nUnmounting...");
        unmount(&mp);
    })
    .context("setting up Ctrl+C")?;

    eprintln!("Mounted read-only at {}", mountpoint.display());
    if cfg!(target_os = "macos") {
        eprintln!("It is in Finder's sidebar under Locations as {volname:?}.");
    }
    eprintln!("Press Ctrl+C here (or eject it in Finder) to unmount.");
    session.join().context("FUSE session")?;
    let _ = std::fs::remove_dir(mountpoint); // only removes it if empty
    eprintln!("Unmounted.");
    Ok(())
}

fn unmount(mp: &Path) {
    let ok = if cfg!(target_os = "macos") {
        std::process::Command::new("umount").arg(mp).status()
    } else {
        std::process::Command::new("fusermount3").arg("-u").arg(mp).status()
    };
    if !ok.is_ok_and(|s| s.success()) {
        eprintln!(
            "Could not unmount {}; close any windows using it and try again.",
            mp.display()
        );
    }
}
