//! Copying a directory tree out of a volume, and scanning it for damage.

use crate::fs::{Fs, Inode, ReadReport};
use crate::platform;
use crate::util::human;
use anyhow::{Context, Result, bail};
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

const DT_DIR: u8 = 4;
const DT_REG: u8 = 8;
const DT_LNK: u8 = 10;

pub struct FileJob {
    pub ino: Inode,
    pub rel: String,
}

pub struct Walk {
    pub files: Vec<FileJob>,
    /// (relative path, inode) of every directory, parents before children.
    pub dirs: Vec<(String, Inode)>,
    pub links: Vec<(String, u64)>,
    pub problems: Vec<(String, String)>,
    /// (new relative path, original name) for names invalid on this system.
    pub renamed: Vec<(String, String)>,
}

/// Lists everything under directory `start` (recursively), without reading file data.
pub fn walk(fs: &Fs, start: u64) -> Result<Walk> {
    let mut w = Walk {
        files: Vec::new(),
        dirs: Vec::new(),
        links: Vec::new(),
        problems: Vec::new(),
        renamed: Vec::new(),
    };
    w.dirs.push((String::new(), fs.inode(start)?));
    let mut stack = vec![(start, String::new())];
    let t0 = Instant::now();
    let mut last = Instant::now();
    while let Some((id, rel)) = stack.pop() {
        let entries = match fs.readdir(id) {
            Ok(e) => e,
            Err(e) => {
                w.problems.push((rel.clone(), format!("cannot list directory: {e:#}")));
                continue;
            }
        };
        for e in entries {
            if e.name.contains('/') || e.name == "." || e.name == ".." || e.name.is_empty() {
                let r = if rel.is_empty() {
                    e.name.clone()
                } else {
                    format!("{rel}/{}", e.name)
                };
                w.problems.push((r, "unsafe file name, skipped".into()));
                continue;
            }
            let (name, changed) = platform::portable_name(&e.name);
            let r = if rel.is_empty() { name } else { format!("{rel}/{name}") };
            if changed {
                w.renamed.push((r.clone(), e.name.clone()));
            }
            match e.dtype {
                DT_DIR => match fs.inode(e.id) {
                    Ok(ino) => {
                        w.dirs.push((r.clone(), ino));
                        stack.push((e.id, r));
                    }
                    Err(err) => w.problems.push((r, format!("{err:#}"))),
                },
                DT_REG => match fs.inode(e.id) {
                    Ok(ino) => w.files.push(FileJob { ino, rel: r }),
                    Err(err) => w.problems.push((r, format!("{err:#}"))),
                },
                DT_LNK => w.links.push((r, e.id)),
                t => w.problems.push((r, format!("special file (type {t}), skipped"))),
            }
        }
        if last.elapsed() > Duration::from_secs(10) {
            eprintln!(
                "  listing... {} folders, {} files ({:.0}s)",
                w.dirs.len(),
                w.files.len(),
                t0.elapsed().as_secs_f64()
            );
            last = Instant::now();
        }
    }
    Ok(w)
}

fn ns_to_filetime(ns: u64) -> filetime::FileTime {
    filetime::FileTime::from_unix_time((ns / 1_000_000_000) as i64, (ns % 1_000_000_000) as u32)
}

pub fn fmt_ranges(r: &[(u64, u64)]) -> String {
    r.iter().map(|(s, e)| format!("{s}-{e}")).collect::<Vec<_>>().join(",")
}

/// Refuses to write onto the same file system as the images.
pub fn check_output_location(out: &Path, images: &[PathBuf]) -> Result<()> {
    let mut probe = out.to_path_buf();
    while !probe.exists() {
        if !probe.pop() {
            break;
        }
    }
    for img in images {
        if platform::same_disk(&probe, img).with_context(|| format!("checking {}", probe.display()))? {
            bail!(
                "output folder {} is on the same disk as {}; choose another disk",
                out.display(),
                img.display()
            );
        }
    }
    Ok(())
}

#[derive(Default)]
pub struct Summary {
    pub ok: u64,
    pub damaged: u64,
    pub errors: u64,
    pub skipped: u64,
    pub bytes: u64,
    pub repaired_blocks: u64,
}

pub fn extract(fs: &Fs, start: u64, out: &Path, threads: usize) -> Result<Summary> {
    fs::create_dir_all(out)?;
    let report_path = out.join("_apfs-rescue-report.tsv");
    eprintln!("Listing files...");
    let w = walk(fs, start)?;
    let total_bytes: u64 = w.files.iter().map(|f| f.ino.size).sum();
    eprintln!(
        "{} files, {} folders, {} in total",
        w.files.len(),
        w.dirs.len(),
        human(total_bytes)
    );

    let report = Mutex::new(BufWriter::new(File::create(&report_path)?));
    writeln!(report.lock().unwrap(), "status\tpath\tsize\tdetail")?;
    let log = |status: &str, rel: &str, size: u64, detail: &str| {
        let mut r = report.lock().unwrap();
        let _ = writeln!(r, "{status}\t{rel}\t{size}\t{detail}");
    };

    for (rel, p) in &w.problems {
        log("ERROR", rel, 0, p);
    }
    for (rel, orig) in &w.renamed {
        log("RENAMED", rel, 0, &format!("original name: {orig}"));
    }
    for (rel, _) in &w.dirs {
        fs::create_dir_all(out.join(rel))?;
    }
    for (rel, id) in &w.links {
        let dst = out.join(rel);
        match fs.symlink_target(*id) {
            Ok(t) => {
                if fs::symlink_metadata(&dst).is_err()
                    && let Err(e) = platform::make_symlink(&t, &dst)
                {
                    log("ERROR", rel, 0, &format!("symlink: {e}"));
                }
            }
            Err(e) => log("ERROR", rel, 0, &format!("{e:#}")),
        }
    }

    let done_bytes = AtomicU64::new(0);
    let done_files = AtomicU64::new(0);
    let sum = Mutex::new(Summary::default());
    let finished = std::sync::atomic::AtomicBool::new(false);
    let pool = rayon::ThreadPoolBuilder::new().num_threads(threads).build()?;
    let t0 = Instant::now();

    std::thread::scope(|s| {
        s.spawn(|| {
            while !finished.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(500));
                if t0.elapsed().as_millis() % 15000 < 500 {
                    let b = done_bytes.load(Ordering::Relaxed);
                    let rate = b as f64 / t0.elapsed().as_secs_f64().max(1.0);
                    eprintln!(
                        "  {}/{} files, {} / {} ({}/s)",
                        done_files.load(Ordering::Relaxed),
                        w.files.len(),
                        human(b),
                        human(total_bytes),
                        human(rate as u64)
                    );
                }
            }
        });
        pool.install(|| {
            use rayon::prelude::*;
            w.files.par_iter().for_each(|job| {
                let dst = out.join(&job.rel);
                let res = copy_file(fs, &job.ino, &dst);
                let mut sm = sum.lock().unwrap();
                match res {
                    Ok(CopyResult::Skipped) => {
                        sm.skipped += 1;
                        log("SKIPPED", &job.rel, job.ino.size, "already copied");
                    }
                    Ok(CopyResult::Copied(n, rep)) => {
                        sm.bytes += n;
                        sm.repaired_blocks += rep.repaired_blocks;
                        if rep.bad.is_empty() {
                            sm.ok += 1;
                            if rep.repaired_blocks > 0 {
                                log(
                                    "REPAIRED",
                                    &job.rel,
                                    n,
                                    &format!("{} blocks taken from SSD cache", rep.repaired_blocks),
                                );
                            }
                        } else {
                            sm.damaged += 1;
                            log(
                                "DAMAGED",
                                &job.rel,
                                n,
                                &format!("zero-filled byte ranges {}", fmt_ranges(&rep.bad)),
                            );
                        }
                    }
                    Err(e) => {
                        sm.errors += 1;
                        log("ERROR", &job.rel, job.ino.size, &format!("{e:#}"));
                    }
                }
                drop(sm);
                done_bytes.fetch_add(job.ino.size, Ordering::Relaxed);
                done_files.fetch_add(1, Ordering::Relaxed);
            });
        });
        finished.store(true, Ordering::Relaxed);
    });

    // Directory times last, deepest first, so file writes don't change them.
    for (rel, ino) in w.dirs.iter().rev() {
        let t = ns_to_filetime(ino.mod_time);
        let _ = filetime::set_file_times(out.join(rel), ns_to_filetime(ino.access_time), t);
    }
    report.lock().unwrap().flush()?;
    let mut s = sum.into_inner().unwrap();
    s.errors += w.problems.len() as u64;
    eprintln!("Report: {}", report_path.display());
    Ok(s)
}

enum CopyResult {
    Skipped,
    Copied(u64, ReadReport),
}

fn copy_file(fs: &Fs, ino: &Inode, dst: &Path) -> Result<CopyResult> {
    let mtime = ns_to_filetime(ino.mod_time);
    if !ino.is_compressed()
        && let Ok(m) = fs::metadata(dst)
        && m.len() == ino.size
        && filetime::FileTime::from_last_modification_time(&m) == mtime
    {
        return Ok(CopyResult::Skipped);
    }
    let tmp = dst.with_file_name(format!(
        ".{}.apfs-rescue-part",
        dst.file_name().map(|n| n.to_string_lossy()).unwrap_or_default()
    ));
    let mut rep = ReadReport::default();
    let n = {
        let mut f = BufWriter::with_capacity(1 << 20, File::create(&tmp)?);
        let n = fs.read_file(ino, &mut f, &mut rep)?;
        f.flush()?;
        n
    };
    platform::set_mode(&tmp, ino.mode, false)?;
    filetime::set_file_times(&tmp, ns_to_filetime(ino.access_time), mtime)?;
    fs::rename(&tmp, dst)?;
    Ok(CopyResult::Copied(n, rep))
}

/// Finds files whose data overlaps unreadable areas, without copying anything.
pub fn scan_damage(fs: &Fs, start: u64) -> Result<Vec<(String, u64, ReadReport)>> {
    let w = walk(fs, start)?;
    let bs = fs.store.bs;
    let mut out = Vec::new();
    for (rel, p) in &w.problems {
        eprintln!("ERROR {rel}: {p}");
    }
    for job in &w.files {
        let mut rep = ReadReport::default();
        let mut streams = vec![(job.ino.private_id, job.ino.size)];
        if job.ino.is_compressed()
            && let Ok(xs) = fs.xattrs(job.ino.id)
        {
            for x in xs {
                if let crate::fs::XattrData::Stream { obj_id, size } = x.data {
                    streams.push((obj_id, size));
                }
            }
        }
        for (sid, size) in streams {
            match fs.extents(sid) {
                Ok(exts) => {
                    for e in exts {
                        if e.phys == 0 || e.logical >= size {
                            continue;
                        }
                        let (bad, rp) = fs.store.damage_report(e.phys, e.len.div_ceil(bs));
                        rep.repaired_blocks += rp;
                        for (s, x) in bad {
                            rep.bad
                                .push((e.logical + (s - e.phys) * bs, e.logical + (x - e.phys) * bs));
                        }
                    }
                }
                Err(err) => eprintln!("ERROR {}: {err:#}", job.rel),
            }
        }
        if !rep.bad.is_empty() || rep.repaired_blocks > 0 {
            out.push((job.rel.clone(), job.ino.size, rep));
        }
    }
    Ok(out)
}

pub fn fmt_time(ns: u64) -> String {
    // days-from-civil inverse (H. Hinnant's algorithm), UTC
    let secs = (ns / 1_000_000_000) as i64;
    let days = secs.div_euclid(86400);
    let sod = secs.rem_euclid(86400);
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}", sod / 3600, sod % 3600 / 60)
}
