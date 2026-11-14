# Changelog

All notable changes are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- On macOS, `copy` restores creation dates and extended attributes (Finder
  tags, comments, resource forks...) of files and folders. `--no-xattrs`
  turns attributes off. Problems are listed in the report as `NOTE`.
- Snapshots: `info` lists them, and `--snapshot` (`-s`) makes `ls`, `copy`
  and `damaged` read a volume as it was when the snapshot was taken.

## [0.1.0] - 2026-11-14

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
