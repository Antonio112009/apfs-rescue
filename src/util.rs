//! Small helpers for reading little-endian fields with bounds checks.

use anyhow::{Result, bail};

pub fn le16(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes(b[off..off + 2].try_into().unwrap())
}
pub fn le32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(b[off..off + 4].try_into().unwrap())
}
pub fn le64(b: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(b[off..off + 8].try_into().unwrap())
}
pub fn be32(b: &[u8], off: usize) -> u32 {
    u32::from_be_bytes(b[off..off + 4].try_into().unwrap())
}

/// Returns `b[off..off+len]` or an error instead of panicking on corrupt data.
pub fn slice(b: &[u8], off: usize, len: usize) -> Result<&[u8]> {
    match off.checked_add(len) {
        Some(end) if end <= b.len() => Ok(&b[off..end]),
        _ => bail!("corrupt structure: range {off}+{len} outside {} bytes", b.len()),
    }
}

/// Name stored on disk: UTF-8 with a trailing NUL.
pub fn cstr(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).into_owned()
}

pub fn human(bytes: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1000.0 && i < U.len() - 1 {
        v /= 1000.0;
        i += 1;
    }
    if i == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.2} {}", U[i])
    }
}
