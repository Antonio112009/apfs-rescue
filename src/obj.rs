//! APFS object header (`obj_phys_t`) and its Fletcher-64 checksum.

use crate::util::{le32, le64};

pub const OBJECT_TYPE_NX_SUPERBLOCK: u32 = 0x01;
pub const OBJECT_TYPE_BTREE: u32 = 0x02;
pub const OBJECT_TYPE_BTREE_NODE: u32 = 0x03;
pub const OBJECT_TYPE_OMAP: u32 = 0x0b;

pub fn fletcher64(block: &[u8]) -> u64 {
    const M: u64 = 0xffff_ffff;
    let (mut s1, mut s2) = (0u64, 0u64);
    let (words, _) = block[8..].as_chunks::<4>();
    for w in words {
        s1 = (s1 + u32::from_le_bytes(*w) as u64) % M;
        s2 = (s2 + s1) % M;
    }
    let c1 = M - ((s1 + s2) % M);
    let c2 = M - ((s1 + c1) % M);
    (c2 << 32) | c1
}

/// True when the stored checksum matches. An all-zero block never matches,
/// which matters because ddrescue fills unread sectors with zeros.
pub fn checksum_ok(block: &[u8]) -> bool {
    let stored = le64(block, 0);
    stored != 0 && stored == fletcher64(block)
}

#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
pub struct ObjHeader {
    pub oid: u64,
    pub xid: u64,
    pub otype: u32,
    pub subtype: u32,
}

impl ObjHeader {
    pub fn parse(b: &[u8]) -> Self {
        ObjHeader {
            oid: le64(b, 8),
            xid: le64(b, 16),
            otype: le32(b, 24) & 0xffff,
            subtype: le32(b, 28),
        }
    }
}
