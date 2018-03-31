//! [u8] -> [u64] mapping. Insertion only.
//!
//! The index could be backed by a combination of an on-disk file, and in-memory content. Changes
//! to the index will be buffered in memory forever until an explicit flush. Internally, the index
//! uses base16 radix tree for keys and linked list of values, though it's possible to extend the
//! format to support other kinds of trees and values.
//!
//! File format:
//!
//! ```ignore
//! INDEX       := HEADER + ENTRY_LIST
//! HEADER      := '\0'  (takes offset 0, so 0 is not a valid offset for ENTRY)
//! ENTRY_LIST  := RADIX | ENTRY_LIST + ENTRY
//! ENTRY       := RADIX | LEAF | LINK | KEY | ROOT
//! RADIX       := '\2' + JUMP_TABLE (16 bytes) + PTR(LINK) + PTR(RADIX | LEAF) * N
//! LEAF        := '\3' + PTR(KEY) + PTR(LINK)
//! LINK        := '\4' + VLQ(VALUE) + PTR(NEXT_LINK | NULL)
//! KEY         := '\5' + VLQ(KEY_LEN) + KEY_BYTES
//! ROOT        := '\1' + PTR(RADIX) + ROOT_LEN (1 byte)
//!
//! PTR(ENTRY)  := VLQ(the offset of ENTRY)
//! ```
//!
//! Some notes about the format:
//!
//! - A "RADIX" entry has 16 children. This is mainly for source control hex hashes. The "N"
//!   in a radix entry could be less than 16 if some of the children are missing (ex. offset = 0).
//!   The corresponding jump table bytes of missing children are 0s. If child i exists, then
//!   `jumptable[i]` is the relative (to the beginning of radix entry) offset of PTR(child offset).
//! - A "ROOT" entry its length recorded as the last byte. Normally the root entry is written
//!   at the end. This makes it easier for the caller - it does not have to record the position
//!   of the root entry. The caller could optionally provide a root location.
//! - An entry has a 1 byte "type". This makes it possible to do a linear scan from the
//!   beginning of the file, instead of having to go through a root. Potentially useful for
//!   recovery purpose, or adding new entry types (ex. tree entries other than the 16-children
//!   radix entry, value entries that are not u64 linked list, key entries that refers external
//!   buffer).
//! - The "JUMP_TABLE" in "RADIX" entry stores relative offsets to the actual value of
//!   RADIX/LEAF offsets. It has redundant information. The more compact form is a 2-byte
//!   (16-bit) bitmask but that hurts lookup performance.

use std::collections::HashMap;
use std::fmt::{self, Debug, Formatter};
use std::fs::{File, OpenOptions};
use std::io::{self, Seek, SeekFrom, Write};
use std::ops::Deref;
use std::path::Path;

use std::io::ErrorKind::InvalidData;

use base16::Base16Iter;
use lock::ScopedFileLock;
use utils::mmap_readonly;

use fs2::FileExt;
use memmap::Mmap;
use vlqencoding::{VLQDecodeAt, VLQEncode};

//// Structures related to file format

#[derive(Clone, PartialEq, Default)]
struct MemRadix {
    pub offsets: [Offset; 16],
    pub link_offset: LinkOffset,
}

#[derive(Clone, PartialEq)]
struct MemLeaf {
    pub key_offset: KeyOffset,
    pub link_offset: LinkOffset,
}

#[derive(Clone, PartialEq)]
struct MemKey {
    pub key: Vec<u8>, // base256
}

#[derive(Clone, PartialEq)]
struct MemLink {
    pub value: u64,
    pub next_link_offset: LinkOffset,
}

#[derive(Clone, PartialEq)]
struct MemRoot {
    pub radix_offset: RadixOffset,
}

//// Serialization

// Offsets that are >= DIRTY_OFFSET refer to in-memory entries that haven't been
// written to disk. Offsets < DIRTY_OFFSET are on-disk offsets.
const DIRTY_OFFSET: u64 = 1u64 << 63;

const TYPE_HEAD: u8 = 0;
const TYPE_ROOT: u8 = 1;
const TYPE_RADIX: u8 = 2;
const TYPE_LEAF: u8 = 3;
const TYPE_LINK: u8 = 4;
const TYPE_KEY: u8 = 5;

// Bits needed to represent the above type integers.
const TYPE_BITS: usize = 3;

// Size constants. Do not change.
const TYPE_BYTES: usize = 1;
const JUMPTABLE_BYTES: usize = 16;

// Raw offset that has an unknown type.
#[derive(Copy, Clone, PartialEq, PartialOrd, Default)]
pub struct Offset(u64);

// Typed offsets. Constructed after verifying types.
// `LinkOffset` is public since it's exposed by some APIs.

#[derive(Copy, Clone, PartialEq, PartialOrd, Default)]
struct RadixOffset(Offset);
#[derive(Copy, Clone, PartialEq, PartialOrd, Default)]
struct LeafOffset(Offset);
#[derive(Copy, Clone, PartialEq, PartialOrd, Default)]
pub struct LinkOffset(Offset);
#[derive(Copy, Clone, PartialEq, PartialOrd, Default)]
struct KeyOffset(Offset);

#[derive(Copy, Clone)]
enum TypedOffset {
    Radix(RadixOffset),
    Leaf(LeafOffset),
    Link(LinkOffset),
    Key(KeyOffset),
}

impl Offset {
    /// Convert `io::Result<u64>` read from disk to a non-dirty `Offset`.
    /// Return `InvalidData` error if the offset is dirty.
    #[inline]
    fn from_disk(value: u64) -> io::Result<Self> {
        if value >= DIRTY_OFFSET {
            Err(InvalidData.into())
        } else {
            Ok(Offset(value))
        }
    }

    /// Convert a possibly "dirty" offset to a non-dirty offset.
    /// Useful when writing offsets to disk.
    #[inline]
    fn to_disk(self, offset_map: &HashMap<u64, u64>) -> u64 {
        if self.is_dirty() {
            // Should always find a value. Otherwise it's a programming error about write order.
            *offset_map.get(&self.0).unwrap()
        } else {
            self.0
        }
    }

    /// Convert to `TypedOffset`.
    #[inline]
    fn to_typed(self, buf: &[u8]) -> io::Result<TypedOffset> {
        let type_int = self.type_int(buf)?;
        match type_int {
            TYPE_RADIX => Ok(TypedOffset::Radix(RadixOffset(self))),
            TYPE_LEAF => Ok(TypedOffset::Leaf(LeafOffset(self))),
            TYPE_LINK => Ok(TypedOffset::Link(LinkOffset(self))),
            TYPE_KEY => Ok(TypedOffset::Key(KeyOffset(self))),
            _ => Err(InvalidData.into()),
        }
    }

    /// Read the `type_int` value.
    #[inline]
    fn type_int(self, buf: &[u8]) -> io::Result<u8> {
        if self.is_null() {
            Err(InvalidData.into())
        } else if self.is_dirty() {
            Ok(((self.0 - DIRTY_OFFSET) & ((1 << TYPE_BITS) - 1)) as u8)
        } else {
            match buf.get(self.0 as usize) {
                Some(x) => Ok(*x as u8),
                _ => return Err(InvalidData.into()),
            }
        }
    }

    /// Test whether the offset is null (0).
    #[inline]
    fn is_null(self) -> bool {
        self.0 == 0
    }

    /// Test whether the offset points to an in-memory entry.
    #[inline]
    fn is_dirty(self) -> bool {
        self.0 >= DIRTY_OFFSET
    }
}

// Common methods shared by typed offset structs.
trait TypedOffsetMethods: Sized {
    #[inline]
    fn dirty_index(self) -> usize {
        debug_assert!(self.to_offset().is_dirty());
        ((self.to_offset().0 - DIRTY_OFFSET) >> TYPE_BITS) as usize
    }

    #[inline]
    fn from_offset(offset: Offset, buf: &[u8]) -> io::Result<Self> {
        if offset.is_null() {
            Ok(Self::from_offset_unchecked(offset))
        } else {
            let type_int = offset.type_int(buf)?;
            if type_int == Self::type_int() {
                Ok(Self::from_offset_unchecked(offset))
            } else {
                Err(InvalidData.into())
            }
        }
    }

    #[inline]
    fn from_dirty_index(index: usize) -> Self {
        Self::from_offset_unchecked(Offset(
            (((index as u64) << TYPE_BITS) | Self::type_int() as u64) + DIRTY_OFFSET,
        ))
    }

    #[inline]
    fn type_int() -> u8;

    #[inline]
    fn from_offset_unchecked(offset: Offset) -> Self;

    #[inline]
    fn to_offset(&self) -> Offset;
}

// Implement traits for typed offset structs.
macro_rules! impl_offset {
    ($type: ident, $type_int: expr, $name: expr) => {
        impl TypedOffsetMethods for $type {
            #[inline]
            fn type_int() -> u8 {
                $type_int
            }

            #[inline]
            fn from_offset_unchecked(offset: Offset) -> Self {
                $type(offset)
            }

            #[inline]
            fn to_offset(&self) -> Offset {
                self.0
            }
        }

        impl Deref for $type {
            type Target = Offset;

            #[inline]
            fn deref(&self) -> &Offset {
                &self.0
            }
        }

        impl Debug for $type {
            fn fmt(&self, f: &mut Formatter) -> Result<(), fmt::Error> {
                if self.is_null() {
                    write!(f, "None")
                } else {
                    if self.is_dirty() {
                        write!(f, "{}[{}]", $name, self.dirty_index())
                    } else {
                        // `Offset` will print "Disk[{}]".
                        self.0.fmt(f)
                    }
                }
            }
        }

        impl From<$type> for Offset {
            #[inline]
            fn from(x: $type) -> Offset {
                x.0
            }
        }

        impl From<$type> for u64 {
            #[inline]
            fn from(x: $type) -> u64 {
                (x.0).0
            }
        }

        impl From<$type> for usize {
            #[inline]
            fn from(x: $type) -> usize {
                (x.0).0 as usize
            }
        }
    };
}

impl_offset!(RadixOffset, TYPE_RADIX, "Radix");
impl_offset!(LeafOffset, TYPE_LEAF, "Leaf");
impl_offset!(LinkOffset, TYPE_LINK, "Link");
impl_offset!(KeyOffset, TYPE_KEY, "Key");

impl RadixOffset {
    /// Link offset of a radix entry.
    #[inline]
    fn link_offset(self, index: &Index) -> io::Result<LinkOffset> {
        if self.is_dirty() {
            Ok(index.dirty_radixes[self.dirty_index()].link_offset)
        } else {
            let (v, _) = index
                .buf
                .read_vlq_at(TYPE_BYTES + JUMPTABLE_BYTES + usize::from(self))?;
            LinkOffset::from_offset(Offset::from_disk(v)?, &index.buf)
        }
    }

    /// Lookup the `i`-th child inside a radix entry.
    /// Return stored offset, or `Offset(0)` if that child does not exist.
    #[inline]
    fn child(self, index: &Index, i: u8) -> io::Result<Offset> {
        debug_assert!(i < 16);
        if self.is_dirty() {
            Ok(index.dirty_radixes[self.dirty_index()].offsets[i as usize])
        } else {
            // Read from jump table
            match index.buf.get(usize::from(self) + TYPE_BYTES + i as usize) {
                None => Err(InvalidData.into()),
                Some(&jump) => {
                    let (v, _) = index.buf.read_vlq_at(usize::from(self) + jump as usize)?;
                    Offset::from_disk(v)
                }
            }
        }
    }

    /// Copy an on-disk entry to memory so it can be modified. Return new offset.
    /// If the offset is already in-memory, return it as-is.
    #[inline]
    fn copy(self, index: &mut Index) -> io::Result<RadixOffset> {
        if self.is_dirty() {
            Ok(self)
        } else {
            let entry = MemRadix::read_from(&index.buf, u64::from(self))?;
            let len = index.dirty_radixes.len();
            index.dirty_radixes.push(entry);
            Ok(RadixOffset::from_dirty_index(len))
        }
    }

    /// Change a child of `MemRadix`. Panic if the offset points to an on-disk entry.
    #[inline]
    fn set_child(self, index: &mut Index, i: u8, value: Offset) {
        assert!(i < 16);
        if self.is_dirty() {
            index.dirty_radixes[self.dirty_index()].offsets[i as usize] = value;
        } else {
            panic!("bug: set_child called on immutable radix entry");
        }
    }

    /// Change link offset of `MemRadix`. Panic if the offset points to an on-disk entry.
    #[inline]
    fn set_link(self, index: &mut Index, value: LinkOffset) {
        if self.is_dirty() {
            index.dirty_radixes[self.dirty_index()].link_offset = value.into();
        } else {
            panic!("bug: set_link called on immutable radix entry");
        }
    }

    /// Create a new in-memory radix entry.
    #[inline]
    fn create(index: &mut Index, radix: MemRadix) -> RadixOffset {
        let len = index.dirty_radixes.len();
        index.dirty_radixes.push(radix);
        RadixOffset::from_dirty_index(len)
    }
}

impl LeafOffset {
    /// Key and link offsets of a leaf entry.
    #[inline]
    fn key_and_link_offset(self, index: &Index) -> io::Result<(KeyOffset, LinkOffset)> {
        if self.is_dirty() {
            let e = &index.dirty_leafs[self.dirty_index()];
            Ok((e.key_offset, e.link_offset))
        } else {
            let (key_offset, vlq_len): (u64, _) =
                index.buf.read_vlq_at(usize::from(self) + TYPE_BYTES)?;
            let key_offset = KeyOffset::from_offset(Offset::from_disk(key_offset)?, &index.buf)?;
            let (link_offset, _) = index
                .buf
                .read_vlq_at(usize::from(self) + TYPE_BYTES + vlq_len)?;
            let link_offset = LinkOffset::from_offset(Offset::from_disk(link_offset)?, &index.buf)?;
            Ok((key_offset, link_offset))
        }
    }

    /// Create a new in-memory leaf entry.
    #[inline]
    fn create(index: &mut Index, link_offset: LinkOffset, key_offset: KeyOffset) -> LeafOffset {
        let len = index.dirty_leafs.len();
        index.dirty_leafs.push(MemLeaf {
            link_offset,
            key_offset,
        });
        LeafOffset::from_dirty_index(len)
    }

    /// Update link_offset of a leaf entry in-place. Copy on write. Return the new leaf_offset
    /// if it's copied from disk.
    ///
    /// Note: the old leaf is expected to be no longer needed. If that's not true, don't call
    /// this function.
    #[inline]
    fn set_link(self, index: &mut Index, link_offset: LinkOffset) -> io::Result<LeafOffset> {
        if self.is_dirty() {
            index.dirty_leafs[self.dirty_index()].link_offset = link_offset;
            Ok(self)
        } else {
            let entry = MemLeaf::read_from(&index.buf, u64::from(self))?;
            Ok(Self::create(index, link_offset, entry.key_offset))
        }
    }
}

impl LinkOffset {
    /// Get value.
    #[inline]
    pub fn value(self, index: &Index) -> io::Result<u64> {
        if self.is_dirty() {
            Ok(index.dirty_links[self.dirty_index()].value)
        } else {
            let (value, _) = index.buf.read_vlq_at(usize::from(self) + TYPE_BYTES)?;
            Ok(value)
        }
    }

    /// Create a new link entry that chains this entry.
    /// Return new `LinkOffset`
    fn create(self, index: &mut Index, value: u64) -> LinkOffset {
        let new_link = MemLink {
            value,
            next_link_offset: self.into(),
        };
        let len = index.dirty_links.len();
        index.dirty_links.push(new_link);
        LinkOffset::from_dirty_index(len)
    }
}

impl KeyOffset {
    /// Key content of a key entry.
    #[inline]
    fn key_content(self, index: &Index) -> io::Result<&[u8]> {
        if self.is_dirty() {
            Ok(&index.dirty_keys[self.dirty_index()].key[..])
        } else {
            let (key_len, vlq_len): (usize, _) =
                index.buf.read_vlq_at(usize::from(self) + TYPE_BYTES)?;
            let start = usize::from(self) + TYPE_BYTES + vlq_len;
            let end = start + key_len;
            if end > index.buf.len() {
                Err(InvalidData.into())
            } else {
                Ok(&index.buf[start..end])
            }
        }
    }

    /// Create a new in-memory key entry.
    #[inline]
    fn create(index: &mut Index, key: &[u8]) -> KeyOffset {
        let len = index.dirty_keys.len();
        index.dirty_keys.push(MemKey {
            key: Vec::from(key),
        });
        KeyOffset::from_dirty_index(len)
    }
}

/// Check type for an on-disk entry
fn check_type(buf: &[u8], offset: usize, expected: u8) -> io::Result<()> {
    let typeint = *(buf.get(offset).ok_or(InvalidData)?);
    if typeint != expected {
        Err(InvalidData.into())
    } else {
        Ok(())
    }
}

impl MemRadix {
    fn read_from<B: AsRef<[u8]>>(buf: B, offset: u64) -> io::Result<Self> {
        let buf = buf.as_ref();
        let offset = offset as usize;
        let mut pos = 0;

        check_type(buf, offset, TYPE_RADIX)?;
        pos += TYPE_BYTES;

        let jumptable = buf.get(offset + pos..offset + pos + JUMPTABLE_BYTES)
            .ok_or(InvalidData)?;
        pos += JUMPTABLE_BYTES;

        let (link_offset, len) = buf.read_vlq_at(offset + pos)?;
        let link_offset = LinkOffset::from_offset(Offset::from_disk(link_offset)?, buf)?;
        pos += len;

        let mut offsets = [Offset::default(); 16];
        for i in 0..16 {
            if jumptable[i] != 0 {
                if jumptable[i] as usize != pos {
                    return Err(InvalidData.into());
                }
                let (v, len) = buf.read_vlq_at(offset + pos)?;
                offsets[i] = Offset::from_disk(v)?;
                pos += len;
            }
        }

        Ok(MemRadix {
            offsets,
            link_offset,
        })
    }

    fn write_to<W: Write>(&self, writer: &mut W, offset_map: &HashMap<u64, u64>) -> io::Result<()> {
        // Approximate size good enough for an average radix entry
        let mut buf = Vec::with_capacity(1 + 16 + 5 * 17);

        buf.write_all(&[TYPE_RADIX])?;
        buf.write_all(&[0u8; 16])?;
        buf.write_vlq(self.link_offset.to_disk(offset_map))?;

        for i in 0..16 {
            let v = self.offsets[i];
            if !v.is_null() {
                let v = v.to_disk(offset_map);
                buf[1 + i] = buf.len() as u8; // update jump table
                buf.write_vlq(v)?;
            }
        }

        writer.write_all(&buf)
    }
}

impl MemLeaf {
    fn read_from<B: AsRef<[u8]>>(buf: B, offset: u64) -> io::Result<Self> {
        let buf = buf.as_ref();
        let offset = offset as usize;
        check_type(buf, offset, TYPE_LEAF)?;
        let (key_offset, len) = buf.read_vlq_at(offset + 1)?;
        let key_offset = KeyOffset::from_offset(Offset::from_disk(key_offset)?, buf)?;
        let (link_offset, _) = buf.read_vlq_at(offset + len + 1)?;
        let link_offset = LinkOffset::from_offset(Offset::from_disk(link_offset)?, buf)?;
        Ok(MemLeaf {
            key_offset,
            link_offset,
        })
    }

    fn write_to<W: Write>(&self, writer: &mut W, offset_map: &HashMap<u64, u64>) -> io::Result<()> {
        writer.write_all(&[TYPE_LEAF])?;
        writer.write_vlq(self.key_offset.to_disk(offset_map))?;
        writer.write_vlq(self.link_offset.to_disk(offset_map))?;
        Ok(())
    }
}

impl MemLink {
    fn read_from<B: AsRef<[u8]>>(buf: B, offset: u64) -> io::Result<Self> {
        let buf = buf.as_ref();
        let offset = offset as usize;
        check_type(buf, offset, TYPE_LINK)?;
        let (value, len) = buf.read_vlq_at(offset + 1)?;
        let (next_link_offset, _) = buf.read_vlq_at(offset + len + 1)?;
        let next_link_offset = LinkOffset::from_offset(Offset::from_disk(next_link_offset)?, buf)?;
        Ok(MemLink {
            value,
            next_link_offset,
        })
    }

    fn write_to<W: Write>(&self, writer: &mut W, offset_map: &HashMap<u64, u64>) -> io::Result<()> {
        writer.write_all(&[TYPE_LINK])?;
        writer.write_vlq(self.value)?;
        writer.write_vlq(self.next_link_offset.to_disk(offset_map))?;
        Ok(())
    }
}

impl MemKey {
    fn read_from<B: AsRef<[u8]>>(buf: B, offset: u64) -> io::Result<Self> {
        let buf = buf.as_ref();
        let offset = offset as usize;
        check_type(buf, offset, TYPE_KEY)?;
        let (key_len, len): (usize, _) = buf.read_vlq_at(offset + 1)?;
        let key = Vec::from(buf.get(offset + 1 + len..offset + 1 + len + key_len)
            .ok_or(InvalidData)?);
        Ok(MemKey { key })
    }

    fn write_to<W: Write>(&self, writer: &mut W, _: &HashMap<u64, u64>) -> io::Result<()> {
        writer.write_all(&[TYPE_KEY])?;
        writer.write_vlq(self.key.len())?;
        writer.write_all(&self.key)?;
        Ok(())
    }
}

impl MemRoot {
    fn read_from<B: AsRef<[u8]>>(buf: B, offset: u64) -> io::Result<Self> {
        let buf = buf.as_ref();
        let offset = offset as usize;
        check_type(buf, offset, TYPE_ROOT)?;
        let (radix_offset, len1) = buf.read_vlq_at(offset + 1)?;
        let radix_offset = RadixOffset::from_offset(Offset::from_disk(radix_offset)?, buf)?;
        let (len, _): (usize, _) = buf.read_vlq_at(offset + 1 + len1)?;
        if len == 1 + len1 + 1 {
            Ok(MemRoot { radix_offset })
        } else {
            Err(InvalidData.into())
        }
    }

    fn read_from_end<B: AsRef<[u8]>>(buf: B, end: u64) -> io::Result<Self> {
        if end > 1 {
            let (size, _): (u64, _) = buf.as_ref().read_vlq_at(end as usize - 1)?;
            Self::read_from(buf, end - size)
        } else {
            Err(InvalidData.into())
        }
    }

    fn write_to<W: Write>(&self, writer: &mut W, offset_map: &HashMap<u64, u64>) -> io::Result<()> {
        let mut buf = Vec::with_capacity(16);
        buf.write_all(&[TYPE_ROOT])?;
        buf.write_vlq(self.radix_offset.to_disk(offset_map))?;
        let len = buf.len() + 1;
        buf.write_vlq(len)?;
        writer.write_all(&buf)
    }
}

//// Main Index

pub struct Index {
    // For locking and low-level access.
    file: File,

    // For efficient and shared random reading.
    buf: Mmap,

    // Whether "file" was opened as read-only.
    // Only affects "flush". Do not affect in-memory writes.
    read_only: bool,

    // In-memory entries. The root entry is always in-memory.
    root: MemRoot,
    dirty_radixes: Vec<MemRadix>,
    dirty_leafs: Vec<MemLeaf>,
    dirty_links: Vec<MemLink>,
    dirty_keys: Vec<MemKey>,
}

impl Index {
    /// Open the index file as read-write. Fallback to read-only.
    ///
    /// The index is always writable because it buffers all writes in-memory.
    /// read-only will only cause "flush" to fail.
    ///
    /// If `root_offset` is not 0, read the root entry from the given offset.
    /// Otherwise, read the root entry from the end of the file.
    pub fn open<P: AsRef<Path>>(path: P, root_offset: u64) -> io::Result<Self> {
        let open_result = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .append(true)
            .open(path.as_ref());

        // Fallback to open the file as read-only.
        let (read_only, mut file) = if open_result.is_err() {
            (true, OpenOptions::new().read(true).open(path)?)
        } else {
            (false, open_result.unwrap())
        };

        let (mmap, len) = {
            if root_offset == 0 {
                // Take the lock to read file length, since that decides root entry location.
                let mut lock = ScopedFileLock::new(&mut file, false)?;
                mmap_readonly(lock.as_ref())?
            } else {
                // It's okay to mmap a larger buffer, without locking.
                mmap_readonly(&file)?
            }
        };

        let (dirty_radixes, root) = if root_offset == 0 {
            // Automatically locate the root entry
            if len == 0 {
                // Empty file. Create root radix entry as an dirty entry
                let radix_offset = RadixOffset::from_dirty_index(0);
                (vec![MemRadix::default()], MemRoot { radix_offset })
            } else {
                // Load root entry from the end of file.
                (vec![], MemRoot::read_from_end(&mmap, len)?)
            }
        } else {
            // Load root entry from given offset.
            (vec![], MemRoot::read_from(&mmap, root_offset)?)
        };

        Ok(Index {
            file,
            buf: mmap,
            read_only,
            root,
            dirty_radixes,
            dirty_links: vec![],
            dirty_leafs: vec![],
            dirty_keys: vec![],
        })
    }

    /// Clone the index.
    pub fn clone(&self) -> io::Result<Index> {
        let file = self.file.duplicate()?;
        let mmap = mmap_readonly(&file)?.0;
        if mmap.len() < self.buf.len() {
            // Break the append-only property
            return Err(InvalidData.into());
        }
        Ok(Index {
            file,
            buf: mmap,
            read_only: self.read_only,
            root: self.root.clone(),
            dirty_keys: self.dirty_keys.clone(),
            dirty_leafs: self.dirty_leafs.clone(),
            dirty_links: self.dirty_links.clone(),
            dirty_radixes: self.dirty_radixes.clone(),
        })
    }

    /// Flush dirty parts to disk.
    ///
    /// Return 0 if nothing needs to be written. Otherwise return the
    /// new offset to the root entry.
    ///
    /// Return `PermissionDenied` if the file is read-only.
    pub fn flush(&mut self) -> io::Result<u64> {
        if self.read_only {
            return Err(io::ErrorKind::PermissionDenied.into());
        }

        let mut root_offset = 0;
        if !self.root.radix_offset.is_dirty() {
            // Nothing changed
            return Ok(root_offset);
        }

        // Critical section: need write lock
        {
            let estimated_dirty_bytes = self.dirty_links.len() * 50;
            let estimated_dirty_offsets = self.dirty_links.len() + self.dirty_keys.len()
                + self.dirty_leafs.len()
                + self.dirty_radixes.len();

            let mut lock = ScopedFileLock::new(&mut self.file, true)?;
            let len = lock.as_mut().seek(SeekFrom::End(0))?;
            let mut buf = Vec::with_capacity(estimated_dirty_bytes);
            let mut offset_map = HashMap::with_capacity(estimated_dirty_offsets);

            // Write in the following order:
            // header, keys, links, leafs, radixes, root.
            // Latter entries depend on former entries.

            if len == 0 {
                buf.write_all(&[TYPE_HEAD])?;
            }

            for (i, entry) in self.dirty_keys.iter().enumerate() {
                let offset = buf.len() as u64 + len;
                entry.write_to(&mut buf, &offset_map)?;
                offset_map.insert(KeyOffset::from_dirty_index(i).into(), offset);
            }

            for (i, entry) in self.dirty_links.iter().enumerate() {
                let offset = buf.len() as u64 + len;
                entry.write_to(&mut buf, &offset_map)?;
                offset_map.insert(LinkOffset::from_dirty_index(i).into(), offset);
            }

            for (i, entry) in self.dirty_leafs.iter().enumerate() {
                let offset = buf.len() as u64 + len;
                entry.write_to(&mut buf, &offset_map)?;
                offset_map.insert(LeafOffset::from_dirty_index(i).into(), offset);
            }

            // Write Radix entries in reversed order since former ones might refer to latter ones.
            for (i, entry) in self.dirty_radixes.iter().enumerate().rev() {
                let offset = buf.len() as u64 + len;
                entry.write_to(&mut buf, &offset_map)?;
                offset_map.insert(RadixOffset::from_dirty_index(i).into(), offset);
            }

            root_offset = buf.len() as u64 + len;
            self.root.write_to(&mut buf, &offset_map)?;
            lock.as_mut().write_all(&buf)?;

            // Remap and update root since length has changed
            let (mmap, new_len) = mmap_readonly(lock.as_ref())?;
            self.buf = mmap;

            // Sanity check - the length should be expected. Otherwise, the lock
            // is somehow ineffective.
            if new_len != buf.len() as u64 + len {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }

            self.root = MemRoot::read_from_end(&self.buf, new_len)?;
        }

        // Outside critical section
        self.dirty_radixes.clear();
        self.dirty_leafs.clear();
        self.dirty_links.clear();
        self.dirty_keys.clear();

        Ok(root_offset)
    }

    /// Lookup by key. Return the link offset (the head of the linked list), or 0
    /// if the key does not exist. This is a low-level API.
    pub fn get<K: AsRef<[u8]>>(&self, key: &K) -> io::Result<LinkOffset> {
        let mut offset: Offset = self.root.radix_offset.into();
        let mut iter = Base16Iter::from_base256(key);

        while !offset.is_null() {
            // Read the entry at "offset"
            match offset.to_typed(&self.buf)? {
                TypedOffset::Radix(radix) => {
                    match iter.next() {
                        None => {
                            // The key ends at this Radix entry.
                            return radix.link_offset(self);
                        }
                        Some(x) => {
                            // Follow the `x`-th child in the Radix entry.
                            offset = radix.child(self, x)?;
                        }
                    }
                }
                TypedOffset::Leaf(leaf) => {
                    // Meet a leaf. If key matches, return the link offset.
                    let (key_offset, link_offset) = leaf.key_and_link_offset(self)?;
                    let stored_key = key_offset.key_content(self)?;
                    if stored_key == key.as_ref() {
                        return Ok(link_offset);
                    } else {
                        return Ok(LinkOffset::default());
                    }
                }
                _ => return Err(InvalidData.into()),
            }
        }

        // Not found
        Ok(LinkOffset::default())
    }

    /// Insert a new value as a head of the linked list associated with `key`.
    pub fn insert<K: AsRef<[u8]>>(&mut self, key: &K, value: u64) -> io::Result<()> {
        self.insert_advanced(key, value.into(), None)
    }

    /// Update the linked list for a given key.
    ///
    /// - If `value` is not None, `link` is None, a new link entry with
    ///   `value` will be created, and connect to the existing linked
    ///   list pointed by `key`. `key` will point to the new link entry.
    /// - If `value` is None, `link` is not None, `key` will point
    ///   to `link` directly.  This can be used to make multiple
    ///   keys share (part of) a linked list.
    /// - If `value` is not None, and `link` is not None, a new link entry
    ///   with `value` will be created, and connect to `link`. `key` will
    ///   point to the new link entry.
    /// - If `value` and `link` are None. Everything related to `key` is
    ///   marked "dirty" without changing their actual logic value.
    ///
    /// This is a low-level API.
    pub fn insert_advanced<K: AsRef<[u8]>>(
        &mut self,
        key: &K,
        value: Option<u64>,
        link: Option<LinkOffset>,
    ) -> io::Result<()> {
        let mut offset: Offset = self.root.radix_offset.into();
        let mut iter = Base16Iter::from_base256(key);
        let mut step = 0;
        let key = key.as_ref();

        let mut last_radix = RadixOffset::default();
        let mut last_child = 0u8;

        loop {
            match offset.to_typed(&self.buf)? {
                TypedOffset::Radix(radix) => {
                    // Copy radix entry since we must modify it.
                    let radix = radix.copy(self)?;
                    offset = radix.into();

                    if step == 0 {
                        self.root.radix_offset = radix;
                    } else {
                        last_radix.set_child(self, last_child, offset);
                    }

                    last_radix = radix;

                    match iter.next() {
                        None => {
                            let old_link_offset = radix.link_offset(self)?;
                            let new_link_offset =
                                self.maybe_create_link_entry(old_link_offset, value, link);
                            radix.set_link(self, new_link_offset);
                            return Ok(());
                        }
                        Some(x) => {
                            let next_offset = radix.child(self, x)?;
                            if next_offset.is_null() {
                                // "key" is longer than existing ones. Create key and leaf entries.
                                let link_offset = self.maybe_create_link_entry(
                                    LinkOffset::default(),
                                    value,
                                    link,
                                );
                                let key_offset = KeyOffset::create(self, key);
                                let leaf_offset = LeafOffset::create(self, link_offset, key_offset);
                                radix.set_child(self, x, leaf_offset.into());
                                return Ok(());
                            } else {
                                offset = next_offset;
                                last_child = x;
                            }
                        }
                    }
                }
                TypedOffset::Leaf(leaf) => {
                    let (key_offset, link_offset) = leaf.key_and_link_offset(self)?;
                    if key_offset.key_content(self)? == key.as_ref() {
                        // Key matched. Need to copy leaf entry.
                        let new_link_offset =
                            self.maybe_create_link_entry(link_offset, value, link);
                        let new_leaf_offset = leaf.set_link(self, new_link_offset)?;
                        last_radix.set_child(self, last_child, new_leaf_offset.into());
                    } else {
                        // Key mismatch. Do a leaf split.
                        let new_link_offset =
                            self.maybe_create_link_entry(LinkOffset::default(), value, link);
                        self.split_leaf(
                            leaf,
                            key_offset,
                            key.as_ref(),
                            step,
                            last_radix,
                            last_child,
                            link_offset,
                            new_link_offset,
                        )?;
                    }
                    return Ok(());
                }
                _ => return Err(InvalidData.into()),
            }

            step += 1;
        }
    }

    /// Split a leaf entry. Separated from `insert_advanced` to make `insert_advanced`
    /// shorter.  The parameters are internal states inside `insert_advanced`. Calling this
    /// from other functions makes less sense.
    #[inline]
    fn split_leaf(
        &mut self,
        old_leaf_offset: LeafOffset,
        old_key_offset: KeyOffset,
        new_key: &[u8],
        step: usize,
        radix_offset: RadixOffset,
        child: u8,
        old_link_offset: LinkOffset,
        new_link_offset: LinkOffset,
    ) -> io::Result<()> {
        // This is probably the most complex part. Here are some explanation about input parameters
        // and what this function is supposed to do for some cases:
        //
        // Input parameters are marked using `*`:
        //
        //      Offset            | Content
        //      root_radix        | Radix(child1: radix1, ...)         \
        //      radix1            | Radix(child2: radix2, ...)         |> steps
        //      ...               | ...                                | (for skipping check
        //      *radix_offset*    | Radix(*child*: *leaf_offset*, ...) /  of prefix in keys)
        //      *old_leaf_offset* | Leaf(link_offset: *old_link_offset*, ...)
        //      *new_link_offset* | Link(...)
        //
        //      old_* are redundant, but they are pre-calculated by the caller. So just reuse them.
        //
        // Here are 3 kinds of examples (Keys are embed in Leaf for simplicity):
        //
        // Example 1. old_key = "1234"; new_key = "1278".
        //
        //      Offset | Before                | After
        //           A | Radix(1: B)           | Radix(1: C)
        //           B | Leaf("1234", Link: X) | Leaf("1234", Link: X)
        //           C |                       | Radix(2: E)
        //           D |                       | Leaf("1278")
        //           E |                       | Radix(3: B, 7: D)
        //
        // Example 2. old_key = "1234", new_key = "12". No need for a new leaf entry:
        //
        //      Offset | Before                | After
        //           A | Radix(1: B)           | Radix(1: C)
        //           B | Leaf("1234", Link: X) | Leaf("1234", Link: X)
        //           C |                       | Radix(2: B, Link: Y)
        //
        // Example 3. old_key = "12", new_key = "1234". Need new leaf. Old leaf is not needed.
        //
        //      Offset | Before              | After
        //           A | Radix(1: B)         | Radix(1: C)
        //           B | Leaf("12", Link: X) | Leaf("12", Link: X) # not used
        //           C |                     | Radix(2: E, Link: X)
        //           D |                     | Leaf("1234", Link: Y)
        //           E |                     | Radix(3: D)

        let old_key = Vec::from(old_key_offset.key_content(self)?);
        let mut old_iter = Base16Iter::from_base256(&old_key).skip(step);
        let mut new_iter = Base16Iter::from_base256(&new_key).skip(step);

        let mut last_radix_offset = radix_offset;
        let mut last_radix_child = child;

        let mut completed = false;

        loop {
            let b1 = old_iter.next();
            let b2 = new_iter.next();

            let mut radix = MemRadix::default();

            if let Some(b1) = b1 {
                // Initial value for the b1-th child. Could be rewritten by
                // "set_radix_entry_child" in the next loop iteration.
                radix.offsets[b1 as usize] = old_leaf_offset.into();
            } else {
                // Example 3. old_key is a prefix of new_key. A leaf is still needed.
                // The new leaf will be created by the next "if" block.
                radix.link_offset = old_link_offset;
            }

            if b2.is_none() {
                // Example 2. new_key is a prefix of old_key. A new leaf is not needed.
                radix.link_offset = new_link_offset;
                completed = true;
            } else if b1 != b2 {
                // Example 1 and Example 3. A new leaf is needed.
                let new_key_offset = KeyOffset::create(self, new_key);
                let new_leaf_offset = LeafOffset::create(self, new_link_offset, new_key_offset);
                radix.offsets[b2.unwrap() as usize] = new_leaf_offset.into();
                completed = true;
            }

            // Create the Radix entry, and connect it to the parent entry.
            let offset = RadixOffset::create(self, radix);
            last_radix_offset.set_child(self, last_radix_child, offset.into());

            if completed {
                break;
            }

            debug_assert!(b1 == b2);
            last_radix_offset = offset;
            last_radix_child = b2.unwrap();
        }

        Ok(())
    }

    /// See `insert_advanced`. Create a new link entry if necessary and return its offset.
    fn maybe_create_link_entry(
        &mut self,
        link_offset: LinkOffset,
        value: Option<u64>,
        link: Option<LinkOffset>,
    ) -> LinkOffset {
        let link = link.or(Some(link_offset)).unwrap();
        if let Some(value) = value {
            link.create(self, value)
        } else {
            link
        }
    }
}

//// Debug Formatter

impl Debug for Offset {
    fn fmt(&self, f: &mut Formatter) -> Result<(), fmt::Error> {
        if self.is_null() {
            write!(f, "None")
        } else if self.is_dirty() {
            match self.to_typed(&b""[..]).unwrap() {
                TypedOffset::Radix(x) => x.fmt(f),
                TypedOffset::Leaf(x) => x.fmt(f),
                TypedOffset::Link(x) => x.fmt(f),
                TypedOffset::Key(x) => x.fmt(f),
            }
        } else {
            write!(f, "Disk[{}]", self.0)
        }
    }
}

impl Debug for MemRadix {
    fn fmt(&self, f: &mut Formatter) -> Result<(), fmt::Error> {
        write!(f, "Radix {{ link: {:?}", self.link_offset)?;
        for (i, v) in self.offsets.iter().cloned().enumerate() {
            if !v.is_null() {
                write!(f, ", {}: {:?}", i, v)?;
            }
        }
        write!(f, " }}")
    }
}

impl Debug for MemLeaf {
    fn fmt(&self, f: &mut Formatter) -> Result<(), fmt::Error> {
        write!(
            f,
            "Leaf {{ key: {:?}, link: {:?} }}",
            self.key_offset, self.link_offset
        )
    }
}

impl Debug for MemLink {
    fn fmt(&self, f: &mut Formatter) -> Result<(), fmt::Error> {
        write!(
            f,
            "Link {{ value: {}, next: {:?} }}",
            self.value, self.next_link_offset
        )
    }
}

impl Debug for MemKey {
    fn fmt(&self, f: &mut Formatter) -> Result<(), fmt::Error> {
        write!(f, "Key {{ key:")?;
        for byte in self.key.iter() {
            write!(f, " {:X}", byte)?;
        }
        write!(f, " }}")
    }
}

impl Debug for MemRoot {
    fn fmt(&self, f: &mut Formatter) -> Result<(), fmt::Error> {
        write!(f, "Root {{ radix: {:?} }}", self.radix_offset)
    }
}

impl Debug for Index {
    fn fmt(&self, f: &mut Formatter) -> Result<(), fmt::Error> {
        write!(
            f,
            "Index {{ len: {}, root: {:?} }}\n",
            self.buf.len(),
            self.root.radix_offset
        )?;

        // On-disk entries
        let offset_map = HashMap::new();
        let mut buf = Vec::with_capacity(self.buf.len());
        buf.push(TYPE_HEAD);
        loop {
            let i = buf.len();
            if i >= self.buf.len() {
                break;
            }
            write!(f, "Disk[{}]: ", i)?;
            let type_int = self.buf[i];
            let i = i as u64;
            match type_int {
                TYPE_RADIX => {
                    let e = MemRadix::read_from(&self.buf, i).expect("read");
                    e.write_to(&mut buf, &offset_map).expect("write");
                    write!(f, "{:?}\n", e)?;
                }
                TYPE_LEAF => {
                    let e = MemLeaf::read_from(&self.buf, i).expect("read");
                    e.write_to(&mut buf, &offset_map).expect("write");
                    write!(f, "{:?}\n", e)?;
                }
                TYPE_LINK => {
                    let e = MemLink::read_from(&self.buf, i).expect("read");
                    e.write_to(&mut buf, &offset_map).expect("write");
                    write!(f, "{:?}\n", e)?;
                }
                TYPE_KEY => {
                    let e = MemKey::read_from(&self.buf, i).expect("read");
                    e.write_to(&mut buf, &offset_map).expect("write");
                    write!(f, "{:?}\n", e)?;
                }
                TYPE_ROOT => {
                    let e = MemRoot::read_from(&self.buf, i).expect("read");
                    e.write_to(&mut buf, &offset_map).expect("write");
                    write!(f, "{:?}\n", e)?;
                }
                _ => {
                    write!(f, "Broken Data!\n")?;
                    break;
                }
            }
        }

        if buf.len() > 1 && self.buf[..] != buf[..] {
            return write!(f, "Inconsistent Data!\n");
        }

        // In-memory entries
        for (i, e) in self.dirty_radixes.iter().enumerate() {
            write!(f, "Radix[{}]: ", i)?;
            write!(f, "{:?}\n", e)?;
        }

        for (i, e) in self.dirty_leafs.iter().enumerate() {
            write!(f, "Leaf[{}]: ", i)?;
            write!(f, "{:?}\n", e)?;
        }

        for (i, e) in self.dirty_links.iter().enumerate() {
            write!(f, "Link[{}]: ", i)?;
            write!(f, "{:?}\n", e)?;
        }

        for (i, e) in self.dirty_keys.iter().enumerate() {
            write!(f, "Key[{}]: ", i)?;
            write!(f, "{:?}\n", e)?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempdir::TempDir;

    #[test]
    fn test_distinct_one_byte_keys() {
        let dir = TempDir::new("index").expect("tempdir");
        let mut index = Index::open(dir.path().join("a"), 0).expect("open");
        assert_eq!(
            format!("{:?}", index),
            "Index { len: 1, root: Radix[0] }\n\
             Radix[0]: Radix { link: None }\n"
        );

        index.insert(&[], 55).expect("update");
        assert_eq!(
            format!("{:?}", index),
            "Index { len: 1, root: Radix[0] }\n\
             Radix[0]: Radix { link: Link[0] }\n\
             Link[0]: Link { value: 55, next: None }\n"
        );

        index.insert(&[0x12], 77).expect("update");
        assert_eq!(
            format!("{:?}", index),
            "Index { len: 1, root: Radix[0] }\n\
             Radix[0]: Radix { link: Link[0], 1: Leaf[0] }\n\
             Leaf[0]: Leaf { key: Key[0], link: Link[1] }\n\
             Link[0]: Link { value: 55, next: None }\n\
             Link[1]: Link { value: 77, next: None }\n\
             Key[0]: Key { key: 12 }\n"
        );

        let link = index.get(&[0x12]).expect("get");
        index
            .insert_advanced(&[0x34], 99.into(), link.into())
            .expect("update");
        assert_eq!(
            format!("{:?}", index),
            "Index { len: 1, root: Radix[0] }\n\
             Radix[0]: Radix { link: Link[0], 1: Leaf[0], 3: Leaf[1] }\n\
             Leaf[0]: Leaf { key: Key[0], link: Link[1] }\n\
             Leaf[1]: Leaf { key: Key[1], link: Link[2] }\n\
             Link[0]: Link { value: 55, next: None }\n\
             Link[1]: Link { value: 77, next: None }\n\
             Link[2]: Link { value: 99, next: Link[1] }\n\
             Key[0]: Key { key: 12 }\n\
             Key[1]: Key { key: 34 }\n"
        );
    }

    #[test]
    fn test_distinct_one_byte_keys_flush() {
        let dir = TempDir::new("index").expect("tempdir");
        let mut index = Index::open(dir.path().join("a"), 0).expect("open");

        // 1st flush.
        assert_eq!(index.flush().expect("flush"), 19);
        assert_eq!(
            format!("{:?}", index),
            "Index { len: 22, root: Disk[1] }\n\
             Disk[1]: Radix { link: None }\n\
             Disk[19]: Root { radix: Disk[1] }\n"
        );

        // Mixed on-disk and in-memory state.
        index.insert(&[], 55).expect("update");
        index.insert(&[0x12], 77).expect("update");
        assert_eq!(
            format!("{:?}", index),
            "Index { len: 22, root: Radix[0] }\n\
             Disk[1]: Radix { link: None }\n\
             Disk[19]: Root { radix: Disk[1] }\n\
             Radix[0]: Radix { link: Link[0], 1: Leaf[0] }\n\
             Leaf[0]: Leaf { key: Key[0], link: Link[1] }\n\
             Link[0]: Link { value: 55, next: None }\n\
             Link[1]: Link { value: 77, next: None }\n\
             Key[0]: Key { key: 12 }\n"
        );

        // After 2nd flush. There are 2 roots.
        let link = index.get(&[0x12]).expect("get");
        index
            .insert_advanced(&[0x34], 99.into(), link.into())
            .expect("update");
        index.flush().expect("flush");
        assert_eq!(
            format!("{:?}", index),
            "Index { len: 66, root: Disk[43] }\n\
             Disk[1]: Radix { link: None }\n\
             Disk[19]: Root { radix: Disk[1] }\n\
             Disk[22]: Key { key: 12 }\n\
             Disk[25]: Key { key: 34 }\n\
             Disk[28]: Link { value: 55, next: None }\n\
             Disk[31]: Link { value: 77, next: None }\n\
             Disk[34]: Link { value: 99, next: Disk[31] }\n\
             Disk[37]: Leaf { key: Disk[22], link: Disk[31] }\n\
             Disk[40]: Leaf { key: Disk[25], link: Disk[34] }\n\
             Disk[43]: Radix { link: Disk[28], 1: Disk[37], 3: Disk[40] }\n\
             Disk[63]: Root { radix: Disk[43] }\n"
        );
    }

    #[test]
    fn test_leaf_split() {
        let dir = TempDir::new("index").expect("tempdir");
        let mut index = Index::open(dir.path().join("a"), 0).expect("open");

        // Example 1: two keys are not prefixes of each other
        index.insert(&[0x12, 0x34], 5).expect("insert");
        assert_eq!(
            format!("{:?}", index),
            "Index { len: 1, root: Radix[0] }\n\
             Radix[0]: Radix { link: None, 1: Leaf[0] }\n\
             Leaf[0]: Leaf { key: Key[0], link: Link[0] }\n\
             Link[0]: Link { value: 5, next: None }\n\
             Key[0]: Key { key: 12 34 }\n"
        );
        index.insert(&[0x12, 0x78], 7).expect("insert");
        assert_eq!(
            format!("{:?}", index),
            "Index { len: 1, root: Radix[0] }\n\
             Radix[0]: Radix { link: None, 1: Radix[1] }\n\
             Radix[1]: Radix { link: None, 2: Radix[2] }\n\
             Radix[2]: Radix { link: None, 3: Leaf[0], 7: Leaf[1] }\n\
             Leaf[0]: Leaf { key: Key[0], link: Link[0] }\n\
             Leaf[1]: Leaf { key: Key[1], link: Link[1] }\n\
             Link[0]: Link { value: 5, next: None }\n\
             Link[1]: Link { value: 7, next: None }\n\
             Key[0]: Key { key: 12 34 }\n\
             Key[1]: Key { key: 12 78 }\n"
        );

        // Example 2: new key is a prefix of the old key
        let mut index = Index::open(dir.path().join("a"), 0).expect("open");
        index.insert(&[0x12, 0x34], 5).expect("insert");
        index.insert(&[0x12], 7).expect("insert");
        assert_eq!(
            format!("{:?}", index),
            "Index { len: 1, root: Radix[0] }\n\
             Radix[0]: Radix { link: None, 1: Radix[1] }\n\
             Radix[1]: Radix { link: None, 2: Radix[2] }\n\
             Radix[2]: Radix { link: Link[1], 3: Leaf[0] }\n\
             Leaf[0]: Leaf { key: Key[0], link: Link[0] }\n\
             Link[0]: Link { value: 5, next: None }\n\
             Link[1]: Link { value: 7, next: None }\n\
             Key[0]: Key { key: 12 34 }\n"
        );

        // Example 3: old key is a prefix of the new key
        let mut index = Index::open(dir.path().join("a"), 0).expect("open");
        index.insert(&[0x12], 5).expect("insert");
        index.insert(&[0x12, 0x78], 7).expect("insert");
        assert_eq!(
            format!("{:?}", index),
            "Index { len: 1, root: Radix[0] }\n\
             Radix[0]: Radix { link: None, 1: Radix[1] }\n\
             Radix[1]: Radix { link: None, 2: Radix[2] }\n\
             Radix[2]: Radix { link: Link[0], 7: Leaf[1] }\n\
             Leaf[0]: Leaf { key: Key[0], link: Link[0] }\n\
             Leaf[1]: Leaf { key: Key[1], link: Link[1] }\n\
             Link[0]: Link { value: 5, next: None }\n\
             Link[1]: Link { value: 7, next: None }\n\
             Key[0]: Key { key: 12 }\n\
             Key[1]: Key { key: 12 78 }\n"
        );

        // Same key. Multiple values.
        let mut index = Index::open(dir.path().join("a"), 0).expect("open");
        index.insert(&[0x12], 5).expect("insert");
        index.insert(&[0x12], 7).expect("insert");
        assert_eq!(
            format!("{:?}", index),
            "Index { len: 1, root: Radix[0] }\n\
             Radix[0]: Radix { link: None, 1: Leaf[0] }\n\
             Leaf[0]: Leaf { key: Key[0], link: Link[1] }\n\
             Link[0]: Link { value: 5, next: None }\n\
             Link[1]: Link { value: 7, next: Link[0] }\n\
             Key[0]: Key { key: 12 }\n"
        );
    }

    #[test]
    fn test_clone() {
        let dir = TempDir::new("index").expect("tempdir");
        let mut index = Index::open(dir.path().join("a"), 0).expect("open");

        index.insert(&[], 55).expect("insert");
        index.insert(&[0x12], 77).expect("insert");
        index.flush().expect("flush");
        index.insert(&[0x15], 99).expect("insert");

        let index2 = index.clone().expect("clone");
        assert_eq!(format!("{:?}", index), format!("{:?}", index2));
    }

    quickcheck! {
        fn test_single_value(map: HashMap<Vec<u8>, u64>, flush: bool) -> bool {
            let dir = TempDir::new("index").expect("tempdir");
            let mut index = Index::open(dir.path().join("a"), 0).expect("open");

            for (key, value) in &map {
                index.insert(key, *value).expect("insert");
            }

            if flush {
                let root_offset = index.flush().expect("flush");
                index = Index::open(dir.path().join("a"), root_offset).expect("open");
            }

            map.iter().all(|(key, value)| {
                let link_offset = index.get(key).expect("lookup");
                assert!(!link_offset.is_null());
                link_offset.value(&index).unwrap() == *value
            })
        }
    }
}
