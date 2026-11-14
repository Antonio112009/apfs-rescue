//! apfs-rescue: read-only APFS (including Fusion Drive) recovery from raw disk images.

mod btree;
mod container;
mod damage;
mod device;
mod extract;
mod fs;
#[cfg(unix)]
mod mount;
mod obj;
mod platform;
mod util;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use container::{Container, VolumeInfo};
use std::path::{Path, PathBuf};
use util::human;

#[derive(Parser)]
#[command(
    version,
    about = "Copy files out of APFS disk images (including Fusion Drives). Never writes to the images.",
    after_help = "Typical use:\n  \
        apfs-rescue info  ssd.dmg hdd.dmg\n  \
        apfs-rescue ls    ssd.dmg hdd.dmg -p Users\n  \
        apfs-rescue copy  ssd.dmg hdd.dmg -p Users/NAME/Pictures -o /Volumes/Backup/Pictures\n\n\
        Full guide: docs/GUIDE.md"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(clap::Args)]
struct Source {
    /// Disk images. For a Fusion Drive give both the SSD and the HDD image (any order).
    #[arg(required = true, num_args = 1..=2, value_name = "IMAGE")]
    images: Vec<PathBuf>,
    /// GNU ddrescue map file (repeatable). A map named like its image
    /// (hdd.dmg + hdd.map) is found automatically.
    #[arg(long, value_name = "MAP")]
    map: Vec<PathBuf>,
    /// Do not look for ddrescue maps next to the images.
    #[arg(long)]
    no_map: bool,
    /// Volume name or number from `info`. Default: the volume with your data.
    #[arg(long, short, value_name = "VOLUME")]
    volume: Option<String>,
    /// Read the volume as it was in this snapshot (name or number from `info`).
    /// Snapshots can hold older versions of files and files deleted since.
    #[arg(long, short, value_name = "SNAPSHOT")]
    snapshot: Option<String>,
    /// Advanced: read an older checkpoint (transaction id) instead of the newest.
    #[arg(long, hide_short_help = true)]
    xid: Option<u64>,
    /// Advanced: how to use the Fusion SSD cache when reading HDD blocks.
    /// `blocks` was verified on a real Fusion pair: sampled clean entries
    /// matched the HDD data at their first and last block.
    #[arg(long, value_enum, default_value_t = CacheMode::Blocks, hide_short_help = true)]
    fusion_cache: CacheMode,
}

#[derive(Clone, Copy, ValueEnum, PartialEq)]
enum CacheMode {
    Off,
    /// Middle-tree lengths are in blocks.
    Blocks,
    /// Middle-tree lengths are in bytes.
    Bytes,
}

#[derive(Subcommand)]
enum Cmd {
    /// Show what is inside the images: volumes, Fusion cache, damaged areas.
    Info {
        #[command(flatten)]
        src: Source,
    },
    /// List one folder.
    Ls {
        #[command(flatten)]
        src: Source,
        /// Folder inside the volume, e.g. Users/NAME/Pictures (default: top level).
        #[arg(long, short, default_value = "")]
        path: String,
    },
    /// Copy a folder (with everything in it) to another disk.
    #[command(alias = "extract")]
    Copy {
        #[command(flatten)]
        src: Source,
        /// Folder inside the volume, e.g. Users/NAME/Pictures (empty = whole volume).
        #[arg(long, short)]
        path: String,
        /// Where to put the copy. Must be on a different disk than the images.
        #[arg(long, short, value_name = "FOLDER")]
        out: PathBuf,
        /// Files copied in parallel.
        #[arg(long, default_value_t = 8, hide_short_help = true)]
        threads: usize,
        /// Do not restore extended attributes (Finder tags, comments...). They
        /// are restored on macOS only.
        #[arg(long)]
        no_xattrs: bool,
        /// Allow writing to the disk that holds the images (tests only).
        #[arg(long, hide = true)]
        allow_same_disk: bool,
    },
    /// List files whose data lies in unreadable areas (needs a ddrescue map). Copies nothing.
    Damaged {
        #[command(flatten)]
        src: Source,
        /// Folder inside the volume (default: whole volume).
        #[arg(long, short, default_value = "")]
        path: String,
    },
    /// Show the volume (or one folder) as a read-only disk you can browse in
    /// Finder or a file manager. Needs macFUSE on a Mac, fuse3 on Linux.
    #[cfg(unix)]
    #[command(after_help = mount::INSTALL_HELP)]
    Mount {
        #[command(flatten)]
        src: Source,
        /// Folder inside the volume to show (default: the whole volume).
        #[arg(long, short, default_value = "")]
        path: String,
        /// Empty folder to mount on (default: a new folder in ~/apfs-rescue-mounts).
        #[arg(long = "at", value_name = "FOLDER")]
        mountpoint: Option<PathBuf>,
    },
    /// Advanced: compare Fusion cache entries with HDD data.
    #[command(hide = true)]
    FusionCheck {
        #[command(flatten)]
        src: Source,
        #[arg(long, default_value_t = 300)]
        samples: usize,
    },
}

/// Pairs each map with an image: explicit `--map` by file name stem, otherwise
/// a `.map` file next to the image.
fn find_maps(src: &Source) -> Result<Vec<(PathBuf, PathBuf)>> {
    let stem = |p: &Path| {
        p.file_stem()
            .map(|s| s.to_string_lossy().to_lowercase())
            .unwrap_or_default()
    };
    let mut out = Vec::new();
    for m in &src.map {
        let img = if src.images.len() == 1 {
            src.images[0].clone()
        } else {
            src.images
                .iter()
                .find(|i| stem(i) == stem(m))
                .with_context(|| {
                    format!(
                        "cannot tell which image {} belongs to; name it like the image (e.g. hdd.dmg + hdd.map)",
                        m.display()
                    )
                })?
                .clone()
        };
        out.push((img, m.clone()));
    }
    if src.no_map {
        return Ok(out);
    }
    for img in &src.images {
        if out.iter().any(|(i, _)| i == img) {
            continue;
        }
        let mut with_suffix = img.clone().into_os_string();
        with_suffix.push(".map");
        for cand in [img.with_extension("map"), PathBuf::from(with_suffix)] {
            if cand.is_file() {
                out.push((img.clone(), cand));
                break;
            }
        }
    }
    Ok(out)
}

fn open(src: &Source) -> Result<Container> {
    let mut c = Container::open(&src.images, src.xid)?;
    let bs = c.store.bs;
    for (img, map) in find_maps(src)? {
        let bad = damage::read_bad_byte_ranges(&map)?;
        let s = &c.store;
        let (off, len, base) = if s.main.path == img {
            (s.main.offset, s.main.len, 0)
        } else if let Some(t2) = s.tier2.as_ref().filter(|t| t.path == img) {
            (t2.offset, t2.len, s.tier2_base)
        } else {
            bail!("internal error: image {} not found", img.display());
        };
        eprintln!("Using ddrescue map {} for {}", map.display(), img.display());
        c.store.damage.add_map(&bad, off, len, bs, base);
    }
    if c.fusion && src.fusion_cache != CacheMode::Off {
        c.enable_fusion_cache(src.fusion_cache == CacheMode::Bytes)?;
    }
    Ok(c)
}

/// The volume asked for, or the one holding user data.
fn pick_volume<'c>(c: &'c Container, sel: &Option<String>) -> Result<&'c VolumeInfo> {
    if let Some(s) = sel {
        return c.volume(s);
    }
    if let Some(v) = c.volumes.iter().find(|v| v.role == 0x40) {
        return Ok(v);
    }
    if c.volumes.len() == 1 {
        return Ok(&c.volumes[0]);
    }
    let names: Vec<String> = c
        .volumes
        .iter()
        .map(|v| format!("  {}  {:?}", v.index, v.name))
        .collect();
    bail!(
        "several volumes found; choose one with -v NUMBER:\n{}",
        names.join("\n")
    )
}

fn open_fs<'c>(c: &'c Container, src: &Source) -> Result<fs::Fs<'c>> {
    let v = pick_volume(c, &src.volume)?;
    eprintln!("Volume: {:?}", v.name);
    let Some(sel) = &src.snapshot else {
        return fs::Fs::open(c, v);
    };
    let snaps = fs::snapshots(c, v)?;
    let snap = snaps
        .iter()
        .find(|s| s.name == *sel || s.xid.to_string() == *sel)
        .with_context(|| format!("no snapshot {sel:?} in volume {:?} (see `info`)", v.name))?;
    eprintln!(
        "Snapshot: {:?} from {} UTC",
        snap.name,
        extract::fmt_time(snap.create_time)
    );
    fs::Fs::open_snapshot(c, v, snap)
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Info { src } => {
            let c = open(&src)?;
            let s = &c.store;
            println!("Block size      : {}", s.bs);
            println!(
                "Image           : {} (APFS at byte {})",
                s.main.path.display(),
                s.main.offset
            );
            if let Some(t2) = &s.tier2 {
                println!("Fusion HDD image: {} (APFS at byte {})", t2.path.display(), t2.offset);
            }
            println!("Fusion Drive    : {}", if c.fusion { "yes" } else { "no" });
            println!("Checkpoint      : {} (newest of {} valid)", c.xid, c.checkpoints.len());
            println!("Volumes:");
            for v in &c.volumes {
                println!(
                    "  {}  {:<26} {:<9} {:>9} files {}",
                    v.index,
                    format!("{:?}", v.name),
                    v.role_name(),
                    v.num_files,
                    if v.encrypted() {
                        " ENCRYPTED (not supported)"
                    } else {
                        ""
                    }
                );
            }
            for v in &c.volumes {
                let snaps = match fs::snapshots(&c, v) {
                    Ok(s) => s,
                    Err(e) => {
                        println!("Snapshots of {:?}: cannot read ({e:#})", v.name);
                        continue;
                    }
                };
                if snaps.is_empty() {
                    continue;
                }
                println!("Snapshots of {:?} (use -s NUMBER):", v.name);
                for s in &snaps {
                    println!("  {:>10}  {} UTC  {}", s.xid, extract::fmt_time(s.create_time), s.name);
                }
            }
            if c.fusion {
                let mt = c.fusion_middle_tree()?;
                let blocks: u64 = mt.iter().map(|e| e.2 as u64).sum();
                println!(
                    "Fusion SSD cache: {} ranges, {} of HDD data has a copy on the SSD",
                    mt.len(),
                    human(blocks * s.bs)
                );
            }
            if s.damage.ranges.is_empty() {
                println!("Damaged areas   : none known (no ddrescue map, or no errors)");
            } else {
                let blocks = s.damage.total_blocks();
                println!(
                    "Damaged areas   : {} areas, {} (from ddrescue map)",
                    s.damage.ranges.len(),
                    human(blocks * s.bs)
                );
            }
        }
        Cmd::Ls { src, path } => {
            let c = open(&src)?;
            let f = open_fs(&c, &src)?;
            let id = f.lookup(&path)?;
            let mut entries = f.readdir(id)?;
            entries.sort_by(|a, b| a.name.cmp(&b.name));
            for e in entries {
                match f.inode(e.id) {
                    Ok(i) => println!(
                        "{} {:>14} {}  {}{}",
                        if i.is_dir() {
                            'd'
                        } else if i.mode & fs::S_IFMT == fs::S_IFLNK {
                            'l'
                        } else {
                            '-'
                        },
                        if i.is_dir() { String::new() } else { i.size.to_string() },
                        extract::fmt_time(i.mod_time),
                        e.name,
                        if i.is_compressed() { "  [compressed]" } else { "" }
                    ),
                    Err(err) => println!("? {:>14} {:16}  {}  ({err:#})", "", "", e.name),
                }
            }
        }
        Cmd::Copy {
            src,
            path,
            out,
            threads,
            no_xattrs,
            allow_same_disk,
        } => {
            if !allow_same_disk {
                extract::check_output_location(&out, &src.images)?;
            }
            let c = open(&src)?;
            let f = open_fs(&c, &src)?;
            let id = f.lookup(&path)?;
            let opts = extract::CopyOptions {
                threads,
                xattrs: !no_xattrs,
            };
            let s = extract::extract(&f, id, &out, &opts)?;
            println!(
                "Done: {} copied fine, {} damaged, {} errors, {} already there. {} written.",
                s.ok,
                s.damaged,
                s.errors,
                s.skipped,
                human(s.bytes)
            );
            if s.repaired_blocks > 0 {
                println!(
                    "{} damaged blocks were recovered from the Fusion SSD cache.",
                    s.repaired_blocks
                );
            }
            if s.damaged + s.errors > 0 {
                println!(
                    "See the report for details: {}",
                    out.join("_apfs-rescue-report.tsv").display()
                );
            }
        }
        Cmd::Damaged { src, path } => {
            let c = open(&src)?;
            if c.store.damage.ranges.is_empty() {
                bail!("no ddrescue map found; put hdd.map next to hdd.dmg or pass --map FILE");
            }
            let f = open_fs(&c, &src)?;
            let id = f.lookup(&path)?;
            let hits = extract::scan_damage(&f, id)?;
            let mut damaged = 0;
            for (rel, size, rep) in &hits {
                if rep.bad.is_empty() {
                    println!("REPAIRED\t{rel}\t{size}\t{} blocks from SSD cache", rep.repaired_blocks);
                } else {
                    damaged += 1;
                    println!("DAMAGED\t{rel}\t{size}\t{}", extract::fmt_ranges(&rep.bad));
                }
            }
            eprintln!(
                "{damaged} damaged files, {} repaired from the Fusion SSD cache",
                hits.len() - damaged
            );
        }
        #[cfg(unix)]
        Cmd::Mount { src, path, mountpoint } => {
            mount::check_fuse()?;
            // The mount lives until the process ends, so the container may too.
            let c: &'static Container = Box::leak(Box::new(open(&src)?));
            let f = open_fs(c, &src)?;
            let id = f.lookup(&path)?;
            let base = path
                .rsplit('/')
                .find(|s| !s.is_empty())
                .unwrap_or(&f.info.name)
                .to_string();
            let volname = match &src.snapshot {
                Some(s) => format!("{base} (snapshot {s})"),
                None => base,
            };
            let mp = match mountpoint {
                Some(p) => p,
                None => {
                    let home = std::env::var_os("HOME").context("HOME is not set; give a mount folder")?;
                    PathBuf::from(home).join("apfs-rescue-mounts").join(&volname)
                }
            };
            let s = &c.store;
            let blocks = (s.main.len + s.tier2.as_ref().map_or(0, |t| t.len)) / s.bs;
            mount::run(mount::RescueFs::new(f, id, blocks), &mp, &volname)?;
        }
        Cmd::FusionCheck { src, samples } => {
            let c = open(&src)?;
            let s = &c.store;
            let mt = c.fusion_middle_tree()?;
            if mt.is_empty() {
                bail!("no Fusion middle tree");
            }
            let bs = s.bs;
            let lens_multiple = mt.iter().all(|e| (e.2 as u64).is_multiple_of(bs));
            let max_len = mt.iter().map(|e| e.2).max().unwrap_or(0);
            println!(
                "{} entries; all lengths multiples of block size: {lens_multiple}; max length {max_len}",
                mt.len()
            );
            let (mut same, mut diff, mut unreadable) = (0, 0, 0);
            let step = (mt.len() / samples.max(1)).max(1);
            let (mut a, mut b) = (vec![0u8; bs as usize], vec![0u8; bs as usize]);
            for e in mt.iter().step_by(step).filter(|e| e.3 & 1 == 0) {
                if s.read_device(e.0, &mut a).is_err() || s.read_device(e.1, &mut b).is_err() {
                    unreadable += 1;
                    continue;
                }
                if a == b { same += 1 } else { diff += 1 }
            }
            println!("clean entries, first block: {same} identical, {diff} different, {unreadable} unreadable");
            // The last block of multi-block entries tells whether `length` counts blocks.
            let (mut same, mut diff) = (0, 0);
            for e in mt.iter().filter(|e| e.2 > 1 && e.3 & 1 == 0).step_by(step) {
                let last = e.2 as u64 - 1;
                if s.read_device(e.0 + last, &mut a).is_ok() && s.read_device(e.1 + last, &mut b).is_ok() {
                    if a == b { same += 1 } else { diff += 1 }
                }
            }
            println!("clean entries, last block if length is in blocks: {same} identical, {diff} different");
        }
    }
    Ok(())
}
