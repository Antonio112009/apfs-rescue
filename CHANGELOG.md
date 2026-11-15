# Changelog

All notable changes are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- `repair` fixes a damaged photo or video after it has been copied out; the
  damaged file is never changed.
  - Photos (JPEG, HEIC, PNG, TIFF, WebP) with an intact smaller copy of the
    same shot (`--reference`, e.g. the preview a Photos or iPhoto library
    keeps): the result is full size, the original's own pixels wherever they
    were read, the reference only in damaged places. In a JPEG, the part after
    a lost piece is put back in place and its colour corrected, so it stays at
    full resolution (on a test photo: 61% of the picture broken before, 5%
    taken from the preview after). A damaged PNG is read exactly up to the
    damage. Colour profile kept; date, camera and place copied with ExifTool.
  - Videos with the unreadable byte ranges (`--bad`, as printed by
    `damaged`): frames spoiled by the damage are removed and the last whole
    frame holds there; sound, date, place and HDR are kept, and the result is
    checked to decode without errors.
  - Uses ImageMagick 7 for pictures and FFmpeg for videos.

## [0.3.0] - 2026-11-15

### Added

- `damaged --find-copies` looks across the whole volume for an intact copy of
  each damaged file (same size, no unreadable parts, identical in every byte
  that could be read). On the test disk it found copies for 38 of 147 damaged
  files in under a minute.
- `copy -p` also accepts a single file.

## [0.2.0] - 2026-11-14

First published release.

### Added

- Read-only access to raw disk and partition images (GPT detection).
- APFS containers, checkpoints, object maps, volumes and file-system trees.
- Fusion Drive: assembles the SSD and HDD images, and reads HDD blocks from the
  SSD cache when the cache copy is newer or the HDD block is unreadable.
- GNU ddrescue maps, found automatically next to the images; per-file reports
  of unreadable byte ranges (`damaged` command and copy report).
- `info`, `ls`, `copy` and `damaged` commands; the data volume is chosen
  automatically.
- Transparent compression: zlib, LZVN and LZFSE.
- Copying keeps modification times and permissions, resumes interrupted runs,
  and refuses to write onto the disk that holds the images.
- Windows support: file names that Windows does not allow are renamed and
  listed in the report.
- On macOS, `copy` restores creation dates and extended attributes (Finder
  tags, comments, resource forks...) of files and folders. `--no-xattrs`
  turns attributes off. Problems are listed in the report as `NOTE`.
- `mount` shows a volume, folder or snapshot as a read-only disk. On macOS it
  needs nothing installed: a local NFS server (`nfsserve`) mounted with the
  system's `mount_nfs`. On Linux it uses FUSE.
- `size` (`du`) shows how much each item in a folder takes, without copying.
- Snapshots: `info` lists them, and `--snapshot` (`-s`) makes `ls`, `copy`
  and `damaged` read a volume as it was when the snapshot was taken.

### Changed

- Release builds for Linux are static (musl) and run on any distribution.

### Fixed

- `mount` also removes the empty `~/apfs-rescue-mounts` folder after unmounting.
- Clearer message for macOS compression types that are not supported yet.
