//! `mount`: shows a volume (or one folder of it) as a read-only disk that can
//! be browsed in Finder or a file manager.
//!
//! - macOS: a local NFS server mounted with the system's `mount_nfs`
//!   (`nfs_mount.rs`); nothing to install.
//! - Linux: the kernel's FUSE (this file); needs the fuse3 package.

use crate::view::View;
use anyhow::Result;
use std::path::Path;

pub const INSTALL_HELP: &str = "\
How `mount` works on each system:
  macOS:   built in (a local NFS server); nothing to install.
  Linux:   needs FUSE: install the fuse3 package (e.g. `sudo apt install fuse3`).
  Windows: not supported yet; use `copy` instead.";

pub fn run(view: View, mountpoint: &Path, volname: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    return crate::nfs_mount::run(view, mountpoint, volname);
    #[cfg(target_os = "linux")]
    return fuse::run(view, mountpoint, volname);
}

#[cfg(target_os = "linux")]
mod fuse {
    use super::INSTALL_HELP;
    use crate::view::{Attr, Kind, View, ViewError};
    use anyhow::{Context, Result, bail};
    use fuser::{
        Config, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo, LockOwner,
        MountOption, OpenFlags, ReplyAttr, ReplyData, ReplyDirectory, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyXattr,
        Request,
    };
    use std::ffi::OsStr;
    use std::path::Path;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    /// The image never changes while mounted, so the kernel may cache freely.
    const TTL: Duration = Duration::from_secs(3600);

    struct FuseView {
        view: View,
        uid: u32,
        gid: u32,
    }

    fn errno(e: ViewError) -> Errno {
        match e {
            ViewError::NotFound => Errno::ENOENT,
            ViewError::Io => Errno::EIO,
        }
    }

    fn time(ns: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_nanos(ns)
    }

    fn kind(k: Kind) -> FileType {
        match k {
            Kind::Dir => FileType::Directory,
            Kind::File => FileType::RegularFile,
            Kind::Symlink => FileType::Symlink,
        }
    }

    impl FuseView {
        // FUSE reserves inode 1 for the root; APFS uses 1 only internally.
        fn ino(&self, id: u64) -> INodeNo {
            INodeNo(if id == self.view.root { 1 } else { id })
        }
        fn id(&self, ino: INodeNo) -> u64 {
            if ino.0 == 1 { self.view.root } else { ino.0 }
        }

        fn fattr(&self, a: &Attr) -> FileAttr {
            FileAttr {
                ino: self.ino(a.id),
                size: a.size,
                blocks: a.size.div_ceil(512),
                atime: time(a.atime),
                mtime: time(a.mtime),
                ctime: time(a.ctime),
                crtime: time(a.crtime),
                kind: kind(a.kind),
                perm: a.perm,
                nlink: if a.kind == Kind::Dir { 2 } else { 1 },
                uid: self.uid,
                gid: self.gid,
                rdev: 0,
                blksize: 4096,
                flags: 0,
            }
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

    impl Filesystem for FuseView {
        fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
            let parent = self.id(parent);
            match self
                .view
                .lookup(parent, &name.to_string_lossy())
                .and_then(|id| self.view.attr(id))
            {
                Ok(a) => reply.entry(&TTL, &self.fattr(&a), Generation(0)),
                Err(e) => reply.error(errno(e)),
            }
        }

        fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
            match self.view.attr(self.id(ino)) {
                Ok(a) => reply.attr(&TTL, &self.fattr(&a)),
                Err(e) => reply.error(errno(e)),
            }
        }

        fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
            match self.view.readlink(self.id(ino)) {
                Ok(t) => reply.data(t.as_bytes()),
                Err(e) => reply.error(errno(e)),
            }
        }

        fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
            if flags.0 & libc::O_ACCMODE != libc::O_RDONLY {
                return reply.error(Errno::EROFS);
            }
            match self.view.attr(self.id(ino)) {
                Ok(_) => reply.opened(FileHandle(0), FopenFlags::FOPEN_KEEP_CACHE),
                Err(e) => reply.error(errno(e)),
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
            match self.view.read(self.id(ino), offset, size) {
                Ok(d) => reply.data(&d),
                Err(e) => reply.error(errno(e)),
            }
        }

        fn readdir(&self, _req: &Request, ino: INodeNo, _fh: FileHandle, offset: u64, mut reply: ReplyDirectory) {
            let id = self.id(ino);
            let list = match self.view.dir(id) {
                Ok(l) => l,
                Err(e) => return reply.error(errno(e)),
            };
            let parent = self.view.parent(id);
            let mut all: Vec<(INodeNo, FileType, &str)> = vec![
                (ino, FileType::Directory, "."),
                (self.ino(parent), FileType::Directory, ".."),
            ];
            for e in list.iter() {
                let k = match e.dtype {
                    4 => FileType::Directory,
                    10 => FileType::Symlink,
                    _ => FileType::RegularFile,
                };
                all.push((self.ino(e.id), k, &e.name));
            }
            for (i, (ino, k, name)) in all.into_iter().enumerate().skip(offset as usize) {
                if reply.add(ino, (i + 1) as u64, k, name) {
                    break;
                }
            }
            reply.ok();
        }

        fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
            let info = &self.view.fs.info;
            let bs = self.view.fs.store.bs as u32;
            reply.statfs(0, 0, 0, info.num_files + info.num_dirs, 0, bs, 255, bs);
        }

        fn getxattr(&self, _req: &Request, ino: INodeNo, name: &OsStr, size: u32, reply: ReplyXattr) {
            let res = self.view.xattrs(self.id(ino)).and_then(|l| {
                let x = l
                    .iter()
                    .find(|x| *x.name == *name.to_string_lossy())
                    .ok_or(ViewError::NotFound)?;
                self.view.xattr_value(x)
            });
            match res {
                Ok(v) => reply_sized(reply, size, &v),
                Err(ViewError::NotFound) => reply.error(Errno::ENODATA),
                Err(e) => reply.error(errno(e)),
            }
        }

        fn listxattr(&self, _req: &Request, ino: INodeNo, size: u32, reply: ReplyXattr) {
            match self.view.xattrs(self.id(ino)) {
                Ok(l) => {
                    let mut names = Vec::new();
                    for x in l {
                        names.extend_from_slice(x.name.as_bytes());
                        names.push(0);
                    }
                    reply_sized(reply, size, &names)
                }
                Err(e) => reply.error(errno(e)),
            }
        }
    }

    fn unmount(mp: &Path) {
        if !std::process::Command::new("fusermount3")
            .arg("-u")
            .arg(mp)
            .status()
            .is_ok_and(|s| s.success())
        {
            eprintln!(
                "Could not unmount {}; close any windows using it and try again.",
                mp.display()
            );
        }
    }

    pub fn run(view: View, mountpoint: &Path, _volname: &str) -> Result<()> {
        if !Path::new("/dev/fuse").exists() {
            bail!("FUSE is not available (/dev/fuse is missing).\n\n{INSTALL_HELP}");
        }
        // SAFETY: getuid/getgid have no preconditions.
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        std::fs::create_dir_all(mountpoint).with_context(|| format!("creating {}", mountpoint.display()))?;
        let mut config = Config::default();
        config.mount_options = vec![
            MountOption::RO,
            MountOption::FSName("apfs-rescue".into()),
            MountOption::NoExec,
        ];
        config.n_threads = Some(4);
        let session = match fuser::spawn_mount(FuseView { view, uid, gid }, mountpoint, &config) {
            Ok(s) => s,
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
        eprintln!("Press Ctrl+C here (or unmount it in the file manager) to finish.");
        session.join().context("FUSE session")?;
        let _ = std::fs::remove_dir(mountpoint);
        eprintln!("Unmounted.");
        Ok(())
    }
}
