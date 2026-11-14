# apfs-rescue: step-by-step guide

This guide is for people who are not data-recovery specialists. It walks
through getting files back from a Mac disk that is failing or comes out of a
Mac that no longer starts. [A Russian version is here](GUIDE.ru.md).

- [0. The rules that keep your data safe](#0-the-rules-that-keep-your-data-safe)
- [1. Make images of the disks](#1-make-images-of-the-disks)
- [2. Get apfs-rescue](#2-get-apfs-rescue)
- [3. Look inside: `info`](#3-look-inside-info)
- [4. Find your folder: `ls`](#4-find-your-folder-ls)
- [5. Which files are damaged: `damaged`](#5-which-files-are-damaged-damaged)
- [6. Copy: `copy`](#6-copy-copy)
- [7. Check the copy](#7-check-the-copy)
- [7a. Older versions and deleted files: snapshots](#7a-older-versions-and-deleted-files-snapshots)
- [8. Photos and iPhoto libraries](#8-photos-and-iphoto-libraries)
- [9. Error messages](#9-error-messages)
- [10. Questions](#10-questions)

## 0. The rules that keep your data safe

1. **Never work on a failing disk directly.** Every read can make it worse.
   Make an image once (step 1), then work only with the image.
2. **Keep the image and the original disk** until you have checked the copy.
3. **Do not let anything "repair" the image or the disk.** If macOS offers to
   *Initialize*, *Repair* or run *First Aid*, say no. apfs-rescue never writes
   to images, but other programs might.
4. **Copy to a different disk** than the one holding the images. apfs-rescue
   refuses to write to the same disk.
5. **Keep the ddrescue `.map` files.** They are the only record of which
   parts of the disk could not be read.

## 1. Make images of the disks

apfs-rescue reads *images*: files that contain a whole disk byte for byte.
The best tool to make them is [GNU ddrescue](https://www.gnu.org/software/ddrescue/),
because it reads the good parts first, retries the bad parts later, and
writes a **map file** that lists what could not be read.

You need a computer that can see the disk (for example the dead Mac's disk in a
USB adapter, or a Linux USB stick booted on any PC) and an external disk with
more free space than the size of the disk you are imaging.

On Linux (the disk is `/dev/sdX`; check with `lsblk` and be very careful to
pick the right one):

```bash
sudo ddrescue -d -n /dev/sdX /media/backup/disk.img /media/backup/disk.map
sudo ddrescue -d -r3 /dev/sdX /media/backup/disk.img /media/backup/disk.map
```

The first command copies everything that reads easily, the second retries the
bad areas three times. Both write to the same map, so you can stop and resume.

**Fusion Drive** (iMac and Mac mini 2012–2019 with "Fusion Drive" in About
This Mac): it is two disks, a small SSD and a large hard disk, that macOS joins
into one. Image **both** disks into separate files, for example `ssd.img` +
`ssd.map` and `hdd.img` + `hdd.map`. You need both images; one alone is not
enough.

The file extension does not matter (`.img`, `.dmg`, `.bin`). apfs-rescue
reads raw images, not compressed Apple disk images made by Disk Utility.

## 2. Get apfs-rescue

### Download

Open the project's **Releases** page on GitHub and download the file for your
computer:

| Computer | File |
|---|---|
| Mac with Apple chip (M1 and later) | `…-aarch64-apple-darwin.tar.gz` |
| Mac with Intel chip | `…-x86_64-apple-darwin.tar.gz` |
| Windows (most PCs) | `…-x86_64-pc-windows-msvc.zip` |
| Windows on ARM | `…-aarch64-pc-windows-msvc.zip` |
| Linux | `…-x86_64-unknown-linux-musl.tar.gz` or `…-aarch64-…` |

Unpack it. Inside is the program `apfs-rescue` (`apfs-rescue.exe` on Windows),
this guide and the license.

**Check that the download is genuine** (optional). Every release lists a
`SHA256SUMS.txt` file and a signed build record. With the
[GitHub CLI](https://cli.github.com/):

```bash
gh attestation verify apfs-rescue-v0.1.0-aarch64-apple-darwin.tar.gz --repo Antonio112009/apfs-rescue
```

**First start on a Mac.** If the release is not signed with an Apple
certificate, macOS says the program "cannot be opened". Remove the download
mark once:

```bash
xattr -d com.apple.quarantine ./apfs-rescue
```

**First start on Windows.** SmartScreen may warn about an unknown publisher:
click *More info → Run anyway*.

### Or build it yourself

Install Rust from [rustup.rs](https://rustup.rs), then:

```bash
git clone https://github.com/Antonio112009/apfs-rescue
cd apfs-rescue
cargo build --release
```

The program is then `target/release/apfs-rescue`.

### Running it

apfs-rescue is a command-line program. Open **Terminal** (Mac: Applications →
Utilities), **PowerShell** (Windows) or a terminal (Linux), go to the folder
where you unpacked it, and run it as `./apfs-rescue` (Windows:
`.\apfs-rescue.exe`). In the examples below, replace the image paths with
yours. Tip: drag a file from Finder or Explorer into the terminal window to
paste its full path.

`./apfs-rescue --help` lists all commands; `./apfs-rescue copy --help` shows
the options of one command.

## 3. Look inside: `info`

```bash
./apfs-rescue info /Volumes/Backup/ssd.img /Volumes/Backup/hdd.img
```

For a normal (non-Fusion) disk give one image. Example output:

```
Using ddrescue map /Volumes/Backup/ssd.map for /Volumes/Backup/ssd.img
Using ddrescue map /Volumes/Backup/hdd.map for /Volumes/Backup/hdd.img
Fusion Drive    : yes
Checkpoint      : 44192259 (newest of 150 valid)
Volumes:
  0  "Macintosh HD - Data"      Data        1623754 files
  4  "Macintosh HD"             System       426897 files
  ...
Fusion SSD cache: 610512 ranges, 9.94 GB of HDD data has a copy on the SSD
Damaged areas   : 759 areas, 4.28 MB (from ddrescue map)
```

What it means:

- **Using ddrescue map**: the map next to each image was found automatically
  (it must have the same name as the image, with `.map`). Otherwise pass
  `--map path/to/file.map`.
- **Volumes**: on macOS 10.15 and later your files are in
  **"Macintosh HD - Data"**; the system volume "Macintosh HD" holds only macOS
  itself. On older Macs there is a single "Macintosh HD". apfs-rescue picks the
  data volume automatically; use `-v NUMBER` to choose another one.
- **Damaged areas**: how much of the disk could not be read. Small numbers
  (a few MB) usually mean only a handful of files are affected.

## 4. Find your folder: `ls`

```bash
./apfs-rescue ls ssd.img hdd.img -p Users
```

lists the user accounts. Then look inside one:

```bash
./apfs-rescue ls ssd.img hdd.img -p Users/NAME
```

Where things usually are:

| What | Path |
|---|---|
| Photos library | `Users/NAME/Pictures/Photos Library.photoslibrary` (may be named in your language) |
| Old iPhoto library | `Users/NAME/Pictures/iPhoto Library.photolibrary` |
| Desktop, Documents, Downloads | `Users/NAME/Desktop`, `Documents`, `Downloads` |
| Music and videos | `Users/NAME/Music`, `Users/NAME/Movies` |
| Mail | `Users/NAME/Library/Mail` |
| Everything of one user | `Users/NAME` |

Paths use `/` on every system, also on Windows. Put a path with spaces in
quotes: `-p "Users/NAME/Pictures/Photos Library.photoslibrary"`.

## 5. Which files are damaged: `damaged`

```bash
./apfs-rescue damaged ssd.img hdd.img -p Users/NAME > damaged.tsv
```

This reads only the file system records, copies nothing and is fast (about
20 seconds for 600,000 files). Each line in `damaged.tsv` is:

```
DAMAGED  <path>  <size>  <byte ranges that are zeros>
```

for files with data in unreadable areas, or `REPAIRED` for files whose bad
blocks were recovered from the Fusion SSD cache. Open the file in a
spreadsheet program to sort it.

A damaged photo usually still opens, with a grey or broken band. Videos
usually play with a short glitch. Look for a second copy of such files
elsewhere (old iPhoto library, phone, cloud).

## 6. Copy: `copy`

```bash
./apfs-rescue copy ssd.img hdd.img -p Users/NAME/Pictures -o /Volumes/NewDisk/Pictures
```

- `-p`: the folder to copy (with everything inside).
- `-o`: where to put the copy. It must be on another disk than the images.

It shows progress every 15 seconds and at the end:

```
Done: 636152 copied fine, 140 damaged, 0 errors, 0 already there. 1.22 TB written.
```

The output folder contains `_apfs-rescue-report.tsv` with one line per file
that needs attention:

| Status | Meaning |
|---|---|
| `DAMAGED` | Copied, but some bytes were unreadable on the disk and are zeros. |
| `REPAIRED` | Unreadable blocks were recovered from the Fusion SSD cache. |
| `ERROR` | Could not be copied; the reason is in the last column. |
| `RENAMED` | The name is not allowed on this system (Windows), so it was changed. |
| `SKIPPED` | Already copied by an earlier run. |
| `NOTE` | Copied, but an attribute or date could not be restored (details in the last column). |

**Interrupted?** Run the same command again. Files that are already complete
are skipped.

Modification dates and (on Mac and Linux) permissions are kept. When you copy
on a Mac, creation dates, Finder tags and colours, comments, resource forks and
other extended attributes are kept as well (`--no-xattrs` turns attributes
off). Windows and Linux have no place for these Mac attributes. The "downloaded
from the internet" mark is deliberately not copied. Anything that could not be
restored is listed in the report as `NOTE`.

## 7. Check the copy

- Compare the numbers: `info` shows the file count of the whole volume, the
  `copy` summary shows how many files were copied from your folder.
- Open a few photos, documents and videos from the copy.
- Read `_apfs-rescue-report.tsv`; it is empty except for the header line
  when everything went well.

Only after this, consider what to do with the old disk. Keep the images.

## 7a. Older versions and deleted files: snapshots

macOS keeps *snapshots*: frozen views of the disk from a moment in the past.
When Time Machine is turned on, it makes one about every hour and keeps them
for about a day; macOS also makes snapshots for system updates. A snapshot can hold a file you deleted
or an older version of a file you changed.

`info` lists them at the end:

```
Snapshots of "Macintosh HD - Data" (use -s NUMBER):
    44191693  2026-11-09 20:06 UTC  com.apple.TimeMachine.2026-11-09-162407.local
```

Add `-s NUMBER` (or the snapshot's name) to `ls`, `copy` or `damaged` to see
the disk as it was then:

```bash
./apfs-rescue ls   ssd.img hdd.img -s 44191693 -p Users/NAME/Desktop
./apfs-rescue copy ssd.img hdd.img -s 44191693 -p Users/NAME/Desktop -o /Volumes/NewDisk/Desktop-snapshot
```

Copy a snapshot into its own folder so it does not mix with the current files.
Snapshots share data with the current files, so damaged areas affect both.

## 8. Photos and iPhoto libraries

A `.photoslibrary` is a folder that macOS shows as one file.

- **To open it in Photos**, copy it to a Mac disk formatted as APFS or Mac OS
  Extended, then hold the **Option** key while starting Photos and choose
  *Other Library…*. Photos may spend a while updating it.
- **On Windows or an exFAT disk** Photos cannot open the library, but your
  original pictures and videos are ordinary files in the `originals` folder
  (older libraries: `Masters`). Copy that folder alone if that is all you need:

  ```bash
  ./apfs-rescue copy ssd.img hdd.img -p "Users/NAME/Pictures/Photos Library.photoslibrary/originals" -o D:\Photos
  ```

- Damaged files in `resources` are previews that Photos recreates; damaged
  files in `originals` or `Masters` are the ones that matter.

## 9. Error messages

| Message | What to do |
|---|---|
| `this is a Fusion container: pass both images` | Give both the SSD and the HDD image. |
| `output folder … is on the same disk as …` | Choose an output folder on another disk. |
| `several volumes found; choose one with -v NUMBER` | Run `info` and pass the number of the volume you want. |
| `volume … is encrypted` | FileVault volumes are not supported yet. |
| `volume … is a sealed system volume` | That volume contains only macOS; your files are in "… - Data". |
| `… not found in path …` | Check the spelling with `ls`, one level at a time. |
| `metadata block … lies in an unreadable area` | A folder record is damaged; the rest is still copied and the folder is listed as `ERROR`. Try `--xid` with an older checkpoint from `info` (advanced). |
| `no APFS container found` | The image is not an APFS disk (older Macs use HFS+), or it is a compressed Disk Utility image. |

## 10. Questions

**Does it work with two SSDs, two hard disks, or a single disk?** Yes. For a
Fusion Drive it only matters which disk is the main one and which is the
second; apfs-rescue reads that from the disks. A normal Mac disk is one image.

**Can it read a disk directly instead of an image?** Not yet: it reads image
files. For a failing disk you should make an image first anyway.

**Can it damage my images?** No. It opens them read-only and has no code that
writes to them.

**HFS+ disks?** Not supported; apfs-rescue reads APFS (macOS 10.13 and later).
