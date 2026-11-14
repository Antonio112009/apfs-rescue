//! Everything that differs between macOS/Linux and Windows lives here.

use anyhow::Result;
use std::fs::File;
use std::path::Path;

/// Reads exactly `buf.len()` bytes at absolute offset `pos` without moving a
/// shared file cursor (safe to call from many threads at once).
#[cfg(unix)]
pub fn read_exact_at(file: &File, buf: &mut [u8], pos: u64) -> std::io::Result<()> {
    use std::os::unix::fs::FileExt;
    file.read_exact_at(buf, pos)
}

#[cfg(windows)]
pub fn read_exact_at(file: &File, mut buf: &mut [u8], mut pos: u64) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        match file.seek_read(buf, pos) {
            Ok(0) => return Err(std::io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => {
                buf = &mut buf[n..];
                pos += n as u64;
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// True when both existing paths are on the same disk / file system.
#[cfg(unix)]
pub fn same_disk(a: &Path, b: &Path) -> Result<bool> {
    use std::os::unix::fs::MetadataExt;
    Ok(std::fs::metadata(a)?.dev() == std::fs::metadata(b)?.dev())
}

#[cfg(windows)]
pub fn same_disk(a: &Path, b: &Path) -> Result<bool> {
    // Compare the drive or share prefix (C:, \\server\share).
    fn prefix(p: &Path) -> Result<String> {
        let c = std::fs::canonicalize(p)?;
        Ok(match c.components().next() {
            Some(std::path::Component::Prefix(px)) => px.as_os_str().to_string_lossy().to_uppercase(),
            _ => String::new(),
        })
    }
    Ok(prefix(a)? == prefix(b)?)
}

/// Applies Unix permission bits (always keeping the owner able to read and write).
#[cfg(unix)]
pub fn set_mode(path: &Path, mode: u16, is_dir: bool) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let owner = if is_dir { 0o700 } else { 0o600 };
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(((mode & 0o777) | owner) as u32))
}

#[cfg(windows)]
pub fn set_mode(_path: &Path, _mode: u16, _is_dir: bool) -> std::io::Result<()> {
    Ok(()) // Windows has no Unix permission bits; files stay writable.
}

#[cfg(unix)]
pub fn make_symlink(target: &str, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
pub fn make_symlink(target: &str, link: &Path) -> std::io::Result<()> {
    // Needs Developer Mode or admin rights; failures are reported, not fatal.
    std::os::windows::fs::symlink_file(target.replace('/', "\\"), link)
}

/// Extended attributes and creation dates are restored on macOS only: other
/// systems have no place for Apple attributes such as Finder tags.
pub const MAC_METADATA: bool = cfg!(target_os = "macos");

#[cfg(target_os = "macos")]
fn c_path(path: &Path) -> std::io::Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;
    Ok(std::ffi::CString::new(path.as_os_str().as_bytes())?)
}

/// Sets one extended attribute on `path` (not following symbolic links).
#[cfg(target_os = "macos")]
pub fn set_xattr(path: &Path, name: &str, value: &[u8]) -> std::io::Result<()> {
    let p = c_path(path)?;
    let n = std::ffi::CString::new(name)?;
    // SAFETY: valid NUL-terminated strings and a buffer of the given length.
    let r = unsafe {
        libc::setxattr(
            p.as_ptr(),
            n.as_ptr(),
            value.as_ptr().cast(),
            value.len(),
            0,
            libc::XATTR_NOFOLLOW,
        )
    };
    if r == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(not(target_os = "macos"))]
pub fn set_xattr(_path: &Path, _name: &str, _value: &[u8]) -> std::io::Result<()> {
    Err(std::io::ErrorKind::Unsupported.into())
}

/// Sets the creation ("birth") time, in nanoseconds since 1970.
#[cfg(target_os = "macos")]
pub fn set_creation_time(path: &Path, ns: u64) -> std::io::Result<()> {
    let p = c_path(path)?;
    let mut list = libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: libc::ATTR_CMN_CRTIME,
        volattr: 0,
        dirattr: 0,
        fileattr: 0,
        forkattr: 0,
    };
    let mut ts = libc::timespec {
        tv_sec: (ns / 1_000_000_000) as _,
        tv_nsec: (ns % 1_000_000_000) as _,
    };
    // SAFETY: the attribute buffer holds exactly the one timespec requested.
    let r = unsafe {
        libc::setattrlist(
            p.as_ptr(),
            (&mut list as *mut libc::attrlist).cast(),
            (&mut ts as *mut libc::timespec).cast(),
            std::mem::size_of::<libc::timespec>(),
            libc::FSOPT_NOFOLLOW,
        )
    };
    if r == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(not(target_os = "macos"))]
pub fn set_creation_time(_path: &Path, _ns: u64) -> std::io::Result<()> {
    Err(std::io::ErrorKind::Unsupported.into())
}

/// Makes one file-name component valid on this system. Returns the name and
/// whether it had to be changed.
pub fn portable_name(name: &str) -> (String, bool) {
    portable_name_for(name, cfg!(windows))
}

fn portable_name_for(name: &str, windows: bool) -> (String, bool) {
    if !windows {
        return (name.to_string(), false);
    }
    const BAD: &[char] = &['<', '>', ':', '"', '\\', '|', '?', '*'];
    let mut s: String = name
        .chars()
        .map(|c| if BAD.contains(&c) || (c as u32) < 32 { '_' } else { c })
        .collect();
    // Windows drops trailing dots and spaces; replace them so names stay distinct.
    let keep = s.trim_end_matches(['.', ' ']).len();
    if keep < s.len() {
        let n = s.len() - keep;
        s.truncate(keep);
        s.push_str(&"_".repeat(n));
    }
    const RESERVED: &[&str] = &[
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9", "LPT1",
        "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    let stem = s.split('.').next().unwrap_or("").to_ascii_uppercase();
    if RESERVED.contains(&stem.as_str()) {
        s.insert(0, '_');
    }
    let changed = s != name;
    (s, changed)
}

#[cfg(test)]
mod tests {
    use super::portable_name_for as p;

    #[test]
    fn windows_names() {
        assert_eq!(p("Фото 1.jpg", true), ("Фото 1.jpg".into(), false));
        assert_eq!(p("10:30 notes?.txt", true), ("10_30 notes_.txt".into(), true));
        assert_eq!(p("ends with dot.", true), ("ends with dot_".into(), true));
        assert_eq!(p("con.txt", true), ("_con.txt".into(), true));
        assert_eq!(p("10:30", false), ("10:30".into(), false));
    }
}
