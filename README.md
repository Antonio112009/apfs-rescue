# apfs-rescue

Copy your files out of a failing or dead Mac disk, safely.

apfs-rescue reads disk images of APFS disks (macOS 10.13 and later), including
**Fusion Drives** imaged as two separate disks, and copies folders to another
disk. It **never writes to the images**, and it tells you exactly which files
were affected by unreadable sectors.

Runs on macOS, Windows and Linux. Written from scratch in Rust from Apple's
public *Apple File System Reference*.

**New to this? Read the step-by-step guide:
[English](docs/GUIDE.md) · [Русский](docs/GUIDE.ru.md)**

## Quick start

```bash
apfs-rescue info  ssd.img hdd.img                       # what is inside
apfs-rescue ls    ssd.img hdd.img -p Users/NAME         # browse
apfs-rescue damaged ssd.img hdd.img -p Users/NAME       # which files are damaged
apfs-rescue copy  ssd.img hdd.img -p Users/NAME/Pictures -o /Volumes/Backup/Pictures
apfs-rescue mount ssd.img hdd.img                       # browse it in Finder, read-only
```

For a normal (non-Fusion) disk give one image. ddrescue map files named like
the images (`hdd.img` + `hdd.map`) are used automatically.

## Download

Programs for macOS, Windows and Linux are on the **Releases** page, together
with checksums, signed build records and the source code.
To build from source: install [Rust](https://rustup.rs) and run
`cargo build --release`.

## What makes it different

- **Fusion Drive aware.** Joins the SSD and HDD images itself and uses the
  Fusion SSD cache: HDD blocks that could not be read are recovered from the
  SSD copy when one exists.
- **Honest about damage.** Reads GNU ddrescue maps and reports, per file, the
  byte ranges that came from unreadable sectors, instead of copying zeros
  silently.
- **Safe.** Images are opened read-only; output onto the disk holding the
  images is refused.
- **Resumable.** Run the same `copy` again after an interruption; finished
  files are skipped.
- **Browse in Finder.** `mount` shows the old disk read-only in Finder, with
  nothing to install: on a Mac it uses the system's own NFS on 127.0.0.1; on
  Linux it uses FUSE.
- **Snapshots.** Lists Time Machine and other APFS snapshots and reads the
  volume as it was then: older versions of files, and files deleted since.
- Keeps modification dates everywhere; on a Mac also creation dates, Finder
  tags, comments, resource forks and other extended attributes.
- Transparent macOS compression (zlib, LZVN, LZFSE), Unicode file names,
  files larger than 4 GB, sparse files, symbolic links.

## Not supported yet

- Encrypted volumes (FileVault).
- Sealed system volumes ("Macintosh HD" on macOS 11+). Your files are in
  "Macintosh HD - Data", which works.
- Reading a disk device directly (make an image first).
- HFS+ disks.

## How it was tested

On a real iMac Fusion Drive (128 GB SSD + 1 TB HDD imaged with ddrescue, 759
unreadable areas, 1.6 million files): every sampled Fusion cache entry matched
the HDD data byte for byte, and the SHA-256 checksums of thousands of extracted
files (photos, applications with compressed files) matched an independent APFS
reader. A synthetic single-disk image covers Unicode names, symbolic links and
a 4.8 GB sparse file.

## Contributing and releases

See [CONTRIBUTING.md](CONTRIBUTING.md) (trunk-based workflow, clean-room rule)
and [RELEASING.md](RELEASING.md) (versioning, release steps, code signing).

## License

[MIT](LICENSE)
