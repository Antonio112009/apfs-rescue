//! `mount` on macOS: serves the view over NFSv3 on 127.0.0.1 (the `nfsserve`
//! crate) and mounts it with the system's own `mount_nfs`. Nothing has to be
//! installed or switched on, unlike macFUSE.

use crate::view::{Attr, Kind, View, ViewError};
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use nfsserve::nfs::{fattr3, fileid3, filename3, ftype3, nfspath3, nfsstat3, nfsstring, nfstime3, sattr3, specdata3};
use nfsserve::tcp::{NFSTcp, NFSTcpListener};
use nfsserve::vfs::{DirEntry, NFSFileSystem, ReadDirResult, VFSCapabilities};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

struct NfsView {
    view: Arc<View>,
    uid: u32,
    gid: u32,
}

fn status(e: ViewError) -> nfsstat3 {
    match e {
        ViewError::NotFound => nfsstat3::NFS3ERR_NOENT,
        ViewError::Io => nfsstat3::NFS3ERR_IO,
    }
}

fn time(ns: u64) -> nfstime3 {
    nfstime3 {
        seconds: (ns / 1_000_000_000) as u32,
        nseconds: (ns % 1_000_000_000) as u32,
    }
}

impl NfsView {
    fn fattr(&self, a: &Attr) -> fattr3 {
        fattr3 {
            ftype: match a.kind {
                Kind::Dir => ftype3::NF3DIR,
                Kind::File => ftype3::NF3REG,
                Kind::Symlink => ftype3::NF3LNK,
            },
            mode: a.perm as u32,
            nlink: if a.kind == Kind::Dir { 2 } else { 1 },
            uid: self.uid,
            gid: self.gid,
            size: a.size,
            used: a.size,
            rdev: specdata3::default(),
            fsid: 0,
            fileid: a.id,
            atime: time(a.atime),
            mtime: time(a.mtime),
            ctime: time(a.ctime),
        }
    }

    /// Reading the image blocks; let the runtime move other work elsewhere.
    fn blocking<T>(&self, f: impl FnOnce(&View) -> T) -> T {
        tokio::task::block_in_place(|| f(&self.view))
    }
}

#[async_trait]
impl NFSFileSystem for NfsView {
    fn capabilities(&self) -> VFSCapabilities {
        VFSCapabilities::ReadOnly
    }

    fn root_dir(&self) -> fileid3 {
        self.view.root
    }

    async fn lookup(&self, dirid: fileid3, filename: &filename3) -> Result<fileid3, nfsstat3> {
        let name = String::from_utf8_lossy(&filename.0);
        match name.as_ref() {
            "." => Ok(dirid),
            ".." => Ok(self.view.parent(dirid)),
            n => self.blocking(|v| v.lookup(dirid, n)).map_err(status),
        }
    }

    async fn getattr(&self, id: fileid3) -> Result<fattr3, nfsstat3> {
        let a = self.blocking(|v| v.attr(id)).map_err(status)?;
        Ok(self.fattr(&a))
    }

    async fn setattr(&self, _id: fileid3, _attr: sattr3) -> Result<fattr3, nfsstat3> {
        Err(nfsstat3::NFS3ERR_ROFS)
    }

    async fn read(&self, id: fileid3, offset: u64, count: u32) -> Result<(Vec<u8>, bool), nfsstat3> {
        self.blocking(|v| {
            let size = v.attr(id)?.size;
            let data = v.read(id, offset, count)?;
            let eof = offset + data.len() as u64 >= size;
            Ok((data, eof))
        })
        .map_err(status)
    }

    async fn write(&self, _id: fileid3, _offset: u64, _data: &[u8]) -> Result<fattr3, nfsstat3> {
        Err(nfsstat3::NFS3ERR_ROFS)
    }

    async fn create(&self, _d: fileid3, _f: &filename3, _a: sattr3) -> Result<(fileid3, fattr3), nfsstat3> {
        Err(nfsstat3::NFS3ERR_ROFS)
    }

    async fn create_exclusive(&self, _d: fileid3, _f: &filename3) -> Result<fileid3, nfsstat3> {
        Err(nfsstat3::NFS3ERR_ROFS)
    }

    async fn mkdir(&self, _d: fileid3, _f: &filename3) -> Result<(fileid3, fattr3), nfsstat3> {
        Err(nfsstat3::NFS3ERR_ROFS)
    }

    async fn remove(&self, _d: fileid3, _f: &filename3) -> Result<(), nfsstat3> {
        Err(nfsstat3::NFS3ERR_ROFS)
    }

    async fn rename(&self, _a: fileid3, _b: &filename3, _c: fileid3, _d: &filename3) -> Result<(), nfsstat3> {
        Err(nfsstat3::NFS3ERR_ROFS)
    }

    async fn readdir(
        &self,
        dirid: fileid3,
        start_after: fileid3,
        max_entries: usize,
    ) -> Result<ReadDirResult, nfsstat3> {
        self.blocking(|v| {
            let list = v.dir(dirid)?;
            let start = if start_after == 0 {
                0
            } else {
                list.iter()
                    .position(|e| e.id == start_after)
                    .map(|i| i + 1)
                    .unwrap_or(list.len())
            };
            let mut entries = Vec::new();
            for e in list.iter().skip(start).take(max_entries) {
                let attr = v.attr(e.id).map(|a| self.fattr(&a)).unwrap_or_default();
                entries.push(DirEntry {
                    fileid: e.id,
                    name: nfsstring(e.name.as_bytes().to_vec()),
                    attr,
                });
            }
            let end = start + entries.len() >= list.len();
            Ok(ReadDirResult { entries, end })
        })
        .map_err(status)
    }

    async fn symlink(
        &self,
        _d: fileid3,
        _n: &filename3,
        _t: &nfspath3,
        _a: &sattr3,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        Err(nfsstat3::NFS3ERR_ROFS)
    }

    async fn readlink(&self, id: fileid3) -> Result<nfspath3, nfsstat3> {
        let t = self.blocking(|v| v.readlink(id)).map_err(status)?;
        Ok(nfsstring(t.into_bytes()))
    }
}

/// True while something else is mounted on `mp` (its device differs from the
/// folder that contains it).
fn is_mounted(mp: &Path) -> bool {
    let (Some(parent), Ok(m)) = (mp.parent(), std::fs::metadata(mp)) else {
        return false;
    };
    std::fs::metadata(parent).is_ok_and(|p| p.dev() != m.dev())
}

fn unmount(mp: &Path, force: bool) {
    let mut cmd = std::process::Command::new("/sbin/umount");
    if force {
        cmd.arg("-f");
    }
    if !cmd.arg(mp).status().is_ok_and(|s| s.success()) {
        eprintln!("Could not unmount yet: close Finder windows and files that use it, then press Ctrl+C again.");
    }
}

/// Serves `view` and mounts it read-only at `mountpoint` until it is unmounted
/// (Ctrl+C here, or Eject in Finder).
pub fn run(view: View, mountpoint: &Path, volname: &str) -> Result<()> {
    if !Path::new("/sbin/mount_nfs").exists() {
        bail!("/sbin/mount_nfs is missing; this macOS cannot mount NFS");
    }
    // SAFETY: getuid/getgid have no preconditions.
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    rt.block_on(async {
        let fs = NfsView {
            view: Arc::new(view),
            uid,
            gid,
        };
        let listener = NFSTcpListener::bind("127.0.0.1:0", fs)
            .await
            .context("starting the local NFS server")?;
        let port = listener.get_listen_port();
        tokio::spawn(async move { listener.handle_forever().await });

        std::fs::create_dir_all(mountpoint).with_context(|| format!("creating {}", mountpoint.display()))?;
        if is_mounted(mountpoint) {
            bail!("something is already mounted at {}", mountpoint.display());
        }
        let opts =
            format!("rdonly,nolocks,vers=3,tcp,soft,intr,rsize=1048576,actimeo=3600,port={port},mountport={port}");
        let mp: PathBuf = mountpoint.to_path_buf();
        let out = tokio::task::spawn_blocking(move || {
            std::process::Command::new("/sbin/mount_nfs")
                .args(["-o", &opts, "localhost:/"])
                .arg(&mp)
                .output()
        })
        .await??;
        if !out.status.success() {
            crate::mount::cleanup(mountpoint);
            bail!("mount_nfs failed: {}", String::from_utf8_lossy(&out.stderr).trim());
        }

        eprintln!("Mounted read-only at {}", mountpoint.display());
        eprintln!("({volname:?} is shown read-only; nothing on it can be changed.)");
        // Show it in a Finder window right away.
        let _ = std::process::Command::new("/usr/bin/open").arg(mountpoint).status();
        eprintln!("Press Ctrl+C here (or eject it in Finder) to unmount.");

        let mut presses = 0;
        let mut tick = tokio::time::interval(Duration::from_secs(2));
        loop {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {
                    presses += 1;
                    eprintln!("Unmounting...");
                    let mp = mountpoint.to_path_buf();
                    let _ = tokio::task::spawn_blocking(move || unmount(&mp, presses > 1)).await;
                }
                _ = tick.tick() => {
                    if !is_mounted(mountpoint) {
                        break;
                    }
                }
            }
        }
        crate::mount::cleanup(mountpoint);
        eprintln!("Unmounted.");
        Ok(())
    })
}
