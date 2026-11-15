<div align="center">

# 🛟 apfs-rescue

**Copy your files out of a failing or dead Mac disk, safely.**

[![CI](https://img.shields.io/github/actions/workflow/status/Antonio112009/apfs-rescue/ci.yml?branch=main&label=CI&logo=githubactions&logoColor=white)](https://github.com/Antonio112009/apfs-rescue/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/Antonio112009/apfs-rescue?logo=github&color=blue)](https://github.com/Antonio112009/apfs-rescue/releases/latest)
[![Downloads](https://img.shields.io/github/downloads/Antonio112009/apfs-rescue/total?color=blue)](https://github.com/Antonio112009/apfs-rescue/releases)
[![License: MIT](https://img.shields.io/github/license/Antonio112009/apfs-rescue?color=green)](LICENSE)
<br>
[![Rust 1.88+](https://img.shields.io/badge/rust-1.88%2B-orange?logo=rust)](https://www.rust-lang.org)
[![Platforms](https://img.shields.io/badge/platform-macOS%20%7C%20Windows%20%7C%20Linux-lightgrey)](#-download)
[![Read-only](https://img.shields.io/badge/disk%20images-read--only-success)](#-what-makes-it-different)
[![Fusion Drive](https://img.shields.io/badge/Fusion%20Drive-supported-success)](#-what-makes-it-different)

📖 **Step-by-step guide:** [English](docs/GUIDE.md) · [Русский](docs/GUIDE.ru.md)

</div>

---

apfs-rescue reads disk images of APFS disks (macOS 10.13 and later), including
**Fusion Drives** imaged as two separate disks, and copies folders to another
disk. It **never writes to the images**, and it tells you exactly which files
were affected by unreadable sectors.

Runs on macOS, Windows and Linux. Written from scratch in Rust from Apple's
public *Apple File System Reference*.

## Contents

- [Quick start](#-quick-start)
- [Commands](#-commands)
- [Download](#-download)
- [What makes it different](#-what-makes-it-different)
- [Not supported yet](#-not-supported-yet)
- [How it was tested](#-how-it-was-tested)
- [Contributing and releases](#-contributing-and-releases)

## 🚀 Quick start

```bash
apfs-rescue info    ssd.img hdd.img                      # what is inside
apfs-rescue ls      ssd.img hdd.img -p Users/NAME        # browse
apfs-rescue damaged ssd.img hdd.img -p Users/NAME        # which files are damaged
apfs-rescue copy    ssd.img hdd.img -p Users/NAME/Pictures -o /Volumes/Backup/Pictures
apfs-rescue mount   ssd.img hdd.img                      # browse it in Finder, read-only
```

> [!TIP]
> For a normal (non-Fusion) disk give one image. GNU ddrescue map files named
> like the images (`hdd.img` + `hdd.map`) are picked up automatically.

## 🧰 Commands

| Command   | What it does |
| --------- | ------------ |
| `info`    | Shows what is inside the images: volumes, Fusion cache, damaged areas, snapshots |
| `ls`      | Lists one folder |
| `size`    | Shows how much each item in a folder takes, without copying anything |
| `damaged` | Lists files whose data lies in unreadable areas (needs a ddrescue map); `--find-copies` looks for intact duplicates |
| `copy`    | Copies a folder, with everything in it, to another disk |
| `mount`   | Shows the volume (or one folder) as a read-only disk in Finder or a file manager |
| `repair`  | Repairs a damaged photo (with an intact preview of it) or video (frames spoiled by the damage are removed) |

Add `-s NUMBER` (a snapshot from `info`) to `ls`, `copy`, `damaged` or `mount`
to see the disk as it was at that moment. Run `apfs-rescue help <command>` for
every option.

## 📦 Download

Ready-to-run programs are on the
[**Releases page**](https://github.com/Antonio112009/apfs-rescue/releases/latest),
together with checksums (`SHA256SUMS.txt`), signed build records and the source code.

| System  | Apple silicon / ARM                     | Intel / x86-64                        |
| ------- | --------------------------------------- | ------------------------------------- |
| macOS   | `aarch64-apple-darwin.tar.gz`           | `x86_64-apple-darwin.tar.gz`          |
| Windows | `aarch64-pc-windows-msvc.zip`           | `x86_64-pc-windows-msvc.zip`          |
| Linux   | `aarch64-unknown-linux-musl.tar.gz`     | `x86_64-unknown-linux-musl.tar.gz`    |

Linux builds are static and run on any distribution.

<details>
<summary><b>Build from source</b></summary>

Install [Rust](https://rustup.rs) (1.88 or newer), then:

```bash
cargo build --release
```

The program is written to `target/release/apfs-rescue`.

</details>

## ✨ What makes it different

- 🔗 **Fusion Drive aware.** Joins the SSD and HDD images itself and uses the
  Fusion SSD cache: HDD blocks that could not be read are recovered from the
  SSD copy when one exists.
- 🩺 **Honest about damage.** Reads GNU ddrescue maps and reports, per file, the
  byte ranges that came from unreadable sectors, instead of copying zeros
  silently.
- 🔍 **Finds intact copies.** `damaged --find-copies` looks across the whole disk
  for an undamaged file with the same contents (photos are often stored twice)
  so a damaged file can be restored exactly.
- 🩹 **Repairs photos and videos.** `repair` rebuilds a damaged photo at full
  size from what was read plus the library's intact preview — in a JPEG it
  puts the shifted rest of the picture back in place — and removes the frames
  a damaged video cannot show (uses ImageMagick and FFmpeg).
- 🔒 **Safe.** Images are opened read-only; output onto the disk holding the
  images is refused.
- ⏯️ **Resumable.** Run the same `copy` again after an interruption; finished
  files are skipped.
- 🗂️ **Browse in Finder.** `mount` shows the old disk read-only in Finder, with
  nothing to install: on a Mac it uses the system's own NFS on 127.0.0.1; on
  Linux it uses FUSE.
- 🕰️ **Snapshots.** Lists Time Machine and other APFS snapshots and reads the
  volume as it was then: older versions of files, and files deleted since.
- 🏷️ **Keeps metadata.** Modification dates everywhere; on a Mac also creation
  dates, Finder tags, comments, resource forks and other extended attributes.
- 🗜️ **Handles the details.** Transparent macOS compression (zlib, LZVN, LZFSE),
  Unicode file names, files larger than 4 GB, sparse files, symbolic links.

## 🚧 Not supported yet

| Not supported | Workaround |
| ------------- | ---------- |
| Encrypted volumes (FileVault) | — |
| Sealed system volumes ("Macintosh HD" on macOS 11+) | Your files are in **"Macintosh HD - Data"**, which works |
| Reading a disk device directly | Make an image first (see [guide, step 1](docs/GUIDE.md#1-make-images-of-the-disks)) |
| HFS+ disks | — |

## 🧪 How it was tested

On a real iMac Fusion Drive (128 GB SSD + 1 TB HDD imaged with ddrescue, 759
unreadable areas, 1.6 million files):

- every sampled Fusion cache entry matched the HDD data byte for byte;
- the SHA-256 checksums of thousands of extracted files (photos, applications
  with compressed files) matched an independent APFS reader.

A synthetic single-disk image covers Unicode names, symbolic links and a 4.8 GB
sparse file.

## 🤝 Contributing and releases

See [CONTRIBUTING.md](CONTRIBUTING.md) (trunk-based workflow, clean-room rule)
and [RELEASING.md](RELEASING.md) (versioning, release steps, code signing).
Changes are listed in [CHANGELOG.md](CHANGELOG.md).

## 📄 License

[MIT](LICENSE)
