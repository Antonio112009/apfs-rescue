//! APFS B-trees (`btree_node_phys_t`): lookup and in-order iteration.

use crate::device::Store;
use crate::obj::{OBJECT_TYPE_BTREE, OBJECT_TYPE_BTREE_NODE, ObjHeader};
use crate::util::{le16, le32, le64, slice};
use anyhow::{Result, bail};
use std::cmp::Ordering;
use std::sync::Arc;

pub const BTNODE_ROOT: u16 = 0x1;
pub const BTNODE_LEAF: u16 = 0x2;
pub const BTNODE_FIXED_KV_SIZE: u16 = 0x4;
const BTREE_INFO_SIZE: usize = 40;
const NODE_HEADER: usize = 56;

#[allow(dead_code)]
#[derive(Clone)]
pub struct Node {
    pub blk: Arc<Vec<u8>>,
    pub paddr: u64,
    pub flags: u16,
    pub level: u16,
    pub nkeys: usize,
    toc: usize,
    kstart: usize,
    vend: usize,
    ksz: usize,
    vsz: usize,
}

impl Node {
    fn parse(blk: Arc<Vec<u8>>, loc: Loc, ksz: usize, vsz: usize) -> Result<Node> {
        let paddr = loc.paddr;
        let h = ObjHeader::parse(&blk);
        if !loc.noheader && h.otype != OBJECT_TYPE_BTREE && h.otype != OBJECT_TYPE_BTREE_NODE {
            bail!("block {paddr:#x} is not a B-tree node (type {:#x})", h.otype);
        }
        let flags = le16(&blk, 32);
        let toff = le16(&blk, 40) as usize;
        let tlen = le16(&blk, 42) as usize;
        let toc = NODE_HEADER + toff;
        let kstart = toc + tlen;
        let vend = if flags & BTNODE_ROOT != 0 {
            blk.len() - BTREE_INFO_SIZE
        } else {
            blk.len()
        };
        if kstart > vend {
            bail!("corrupt B-tree node {paddr:#x}");
        }
        Ok(Node {
            paddr,
            flags,
            level: le16(&blk, 34),
            nkeys: le32(&blk, 36) as usize,
            toc,
            kstart,
            vend,
            ksz,
            vsz,
            blk,
        })
    }

    pub fn is_leaf(&self) -> bool {
        self.flags & BTNODE_LEAF != 0
    }

    pub fn key(&self, i: usize) -> Result<&[u8]> {
        Ok(self.entry(i)?.0)
    }

    pub fn entry(&self, i: usize) -> Result<(&[u8], &[u8])> {
        if i >= self.nkeys {
            bail!("entry index {i} out of range in node {:#x}", self.paddr);
        }
        let b = &self.blk[..];
        let (k, kl, v, vl) = if self.flags & BTNODE_FIXED_KV_SIZE != 0 {
            let e = self.toc + i * 4;
            let vl = if self.is_leaf() { self.vsz } else { 8 };
            (le16(b, e) as usize, self.ksz, le16(b, e + 2) as usize, vl)
        } else {
            let e = self.toc + i * 8;
            (
                le16(b, e) as usize,
                le16(b, e + 2) as usize,
                le16(b, e + 4) as usize,
                le16(b, e + 6) as usize,
            )
        };
        let key = slice(b, self.kstart + k, kl)?;
        if v == 0xffff {
            return Ok((key, &[])); // no value
        }
        if v > self.vend {
            bail!("corrupt value offset in node {:#x}", self.paddr);
        }
        let val = slice(b, self.vend - v, vl)?;
        Ok((key, val))
    }

    /// Child object id stored in an index node entry.
    fn child(&self, i: usize) -> Result<u64> {
        let v = self.entry(i)?.1;
        if v.len() < 8 {
            bail!("corrupt index entry in node {:#x}", self.paddr);
        }
        Ok(le64(v, 0))
    }
}

/// Where an object lives. `noheader` objects (e.g. nodes of a sealed
/// volume's file-system tree) carry no `obj_phys_t` checksum.
#[derive(Debug, Clone, Copy)]
pub struct Loc {
    pub paddr: u64,
    pub noheader: bool,
}

impl Loc {
    pub fn phys(paddr: u64) -> Loc {
        Loc { paddr, noheader: false }
    }
}

/// How child pointers are translated into block addresses.
pub trait Resolve {
    fn resolve(&self, oid: u64) -> Result<Loc>;
}

pub struct Physical;
impl Resolve for Physical {
    fn resolve(&self, oid: u64) -> Result<Loc> {
        Ok(Loc::phys(oid))
    }
}

pub struct Tree<'a, R: Resolve> {
    pub store: &'a Store,
    pub root: Node,
    res: R,
    /// `bt_flags` from the root's `btree_info_t`.
    pub bt_flags: u32,
}

const BTREE_PHYSICAL: u32 = 0x10;
const BTREE_NOHEADER: u32 = 0x100;

impl<'a, R: Resolve> Tree<'a, R> {
    pub fn open(store: &'a Store, root: Loc, res: R) -> Result<Self> {
        let blk = store.read_object(root.paddr, !root.noheader)?;
        let flags = le16(&blk, 32);
        if flags & BTNODE_ROOT == 0 {
            bail!("block {:#x} is not a B-tree root", root.paddr);
        }
        let info = &blk[blk.len() - BTREE_INFO_SIZE..];
        let (bt_flags, ksz, vsz) = (le32(info, 0), le32(info, 8) as usize, le32(info, 12) as usize);
        let root = Node::parse(blk, root, ksz, vsz)?;
        Ok(Tree {
            store,
            root,
            res,
            bt_flags,
        })
    }

    fn load(&self, oid: u64) -> Result<Node> {
        // Sealed volumes keep their file-system tree physical and header-less,
        // even though the tree itself is reached through the object map.
        let mut loc = if self.bt_flags & BTREE_PHYSICAL != 0 {
            Loc::phys(oid)
        } else {
            self.res.resolve(oid)?
        };
        loc.noheader |= self.bt_flags & BTREE_NOHEADER != 0;
        let blk = self.store.read_object(loc.paddr, !loc.noheader)?;
        Node::parse(blk, loc, self.root.ksz, self.root.vsz)
    }

    /// Last entry whose key is <= the target (`cmp(key)` compares a key to the target).
    pub fn find_le(&self, cmp: impl Fn(&[u8]) -> Ordering) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        let mut node = self.root.clone();
        loop {
            let mut found = None;
            for i in 0..node.nkeys {
                if cmp(node.key(i)?) != Ordering::Greater {
                    found = Some(i);
                } else {
                    break;
                }
            }
            let Some(i) = found else { return Ok(None) };
            if node.is_leaf() {
                let (k, v) = node.entry(i)?;
                return Ok(Some((k.to_vec(), v.to_vec())));
            }
            node = self.load(node.child(i)?)?;
        }
    }

    /// Iterates in key order starting at the first entry not less than the
    /// target. Callers stop when keys go past what they need.
    pub fn iter_from(&self, cmp: impl Fn(&[u8]) -> Ordering) -> Result<TreeIter<'_, 'a, R>> {
        let mut stack = Vec::new();
        let mut node = self.root.clone();
        loop {
            if node.is_leaf() {
                let mut i = node.nkeys;
                for j in 0..node.nkeys {
                    if cmp(node.key(j)?) != Ordering::Less {
                        i = j;
                        break;
                    }
                }
                stack.push((node, i));
                break;
            }
            // Last child whose first key is strictly less than the target:
            // entries equal to the target may continue from that child.
            let mut c = 0;
            for j in 0..node.nkeys {
                if cmp(node.key(j)?) == Ordering::Less {
                    c = j;
                } else {
                    break;
                }
            }
            let child = self.load(node.child(c)?)?;
            stack.push((node, c + 1));
            node = child;
        }
        Ok(TreeIter { tree: self, stack })
    }

    pub fn iter_all(&self) -> Result<TreeIter<'_, 'a, R>> {
        self.iter_from(|_| Ordering::Greater)
    }
}

pub struct TreeIter<'t, 'a, R: Resolve> {
    tree: &'t Tree<'a, R>,
    stack: Vec<(Node, usize)>,
}

impl<R: Resolve> Iterator for TreeIter<'_, '_, R> {
    type Item = Result<(Vec<u8>, Vec<u8>)>;
    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let top = self.stack.last_mut()?;
            let i = top.1;
            if i >= top.0.nkeys {
                self.stack.pop();
                continue;
            }
            top.1 += 1;
            let node = top.0.clone();
            if node.is_leaf() {
                return Some(node.entry(i).map(|(k, v)| (k.to_vec(), v.to_vec())));
            }
            match node.child(i).and_then(|oid| self.tree.load(oid)) {
                Ok(child) => self.stack.push((child, 0)),
                Err(e) => return Some(Err(e)),
            }
        }
    }
}
