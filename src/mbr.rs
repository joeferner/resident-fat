//! Master boot records, so that "mount the card" does not need a second
//! crate.
//!
//! A card formatted as one bare volume — what `mkfs.vfat` on a raw device
//! produces — starts with a boot sector, and
//! [`FileSystem::mount`](crate::FileSystem::mount) is right for it. A card
//! written by an imaging tool starts with a *partition table*, and the
//! volume begins some way in. That is the common case for removable media
//! and the one this module exists for.
//!
//! Partition tables are arguably not filesystem logic, and other projects do
//! put them in a crate of their own. This is about a hundred lines, and
//! making the ordinary case of mounting a card require a second dependency
//! is a worse trade than the loss of purity. The `mbr` feature keeps it off
//! the compile for anyone who does not need it.
//!
//! # Telling a table from a boot sector
//!
//! Harder than it looks, and worth doing properly: **both structures end
//! with the same `0x55AA` signature**, so the obvious check answers yes to
//! either. Reading a boot sector's tail as four partition entries yields
//! plausible-looking offsets, and mounting at one puts every subsequent
//! access somewhere arbitrary on the device.
//!
//! Three things have to hold before this reports a table, and a boot sector
//! fails all three:
//!
//! * the signature, which is necessary and nowhere near sufficient;
//! * every one of the four status bytes being `0x00` or `0x80`, since that
//!   field has only those two meanings; and
//! * at least one entry describing a real partition — a nonzero type, a
//!   nonzero length, and a first block that is not zero, because a partition
//!   containing the table that describes it is nonsense.
//!
//! The region those entries occupy is zero-filled on a FAT boot sector,
//! which fails the third check outright. Where it is not zero it is boot
//! code, which fails the second.
//!
//! # GPT
//!
//! Not supported, and not planned. It appears on removable media almost
//! nowhere, and a second parser that nothing exercises is a liability rather
//! than a feature. A GPT disk carries a protective MBR whose single entry
//! has type `0xEE`; [`Partition::is_protective_gpt`] recognises it so that
//! the failure is a clear one rather than a mount of the wrong thing.

use crate::blockdev::{BLOCK_SIZE, BlockDevice};
use crate::error::{Error, Result};

/// Where the first partition entry starts.
const TABLE: usize = 0x1BE;
/// Bytes in one partition entry.
const ENTRY_SIZE: usize = 16;
/// Entries a master boot record holds.
pub const MAX_PARTITIONS: usize = 4;

/// Offsets within a partition entry.
mod field {
    /// `0x80` for the bootable partition, `0x00` otherwise.
    pub const STATUS: usize = 0x0;
    /// What the partition is meant to hold.
    pub const KIND: usize = 0x4;
    /// First block of the partition.
    pub const FIRST_BLOCK: usize = 0x8;
    /// How many blocks it spans.
    pub const BLOCKS: usize = 0xC;
}

/// The two values a status byte is allowed to take.
const STATUS_INACTIVE: u8 = 0x00;
/// Marks the partition the BIOS should boot.
const STATUS_BOOTABLE: u8 = 0x80;

/// One partition, as the table describes it.
///
/// Nothing here is checked against the partition's contents: the type byte
/// is what the table *claims*, and a volume is only known to be FAT once its
/// boot sector has been read. That is why mounting takes the block number
/// from here and validates the volume separately.
///
/// `#[non_exhaustive]`, like the other parsed descriptions this crate hands
/// out: an entry comes from a table, never from a caller, so the attribute
/// costs nothing now and leaves room for a field later.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Partition {
    /// Which of the four table slots this is.
    pub index: usize,
    /// Whether the table marks this partition bootable.
    pub bootable: bool,
    /// The partition type byte.
    pub kind: u8,
    /// First block of the partition, counting from the start of the device.
    pub first_block: u64,
    /// How many blocks the partition spans.
    pub blocks: u64,
}

impl Partition {
    /// Whether the type byte is one used for a FAT volume.
    ///
    /// A hint for choosing between partitions, not a guarantee: the byte is
    /// metadata a tool wrote and nothing enforces, and a FAT volume under an
    /// unexpected type still mounts. Deciding by this rather than by the
    /// boot sector would refuse working cards.
    pub fn is_fat(&self) -> bool {
        matches!(
            self.kind,
            0x01 | 0x04 | 0x06 | 0x0B | 0x0C | 0x0E | 0x11 | 0x14 | 0x16 | 0x1B | 0x1C | 0x1E
        )
    }

    /// Whether this is the placeholder entry a GPT disk carries.
    ///
    /// A GPT disk keeps a "protective" master boot record with one entry of
    /// type `0xEE` spanning the device, so that a tool which understands only
    /// partition tables sees the disk as full rather than as empty and
    /// available. Mounting it would mean treating the GPT header as a volume.
    pub fn is_protective_gpt(&self) -> bool {
        self.kind == 0xEE
    }
}

/// A device's partition table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PartitionTable {
    entries: [Option<Partition>; MAX_PARTITIONS],
}

impl PartitionTable {
    /// Reads block 0 and parses it, or reports that it is not a table.
    ///
    /// `Ok(None)` means the device has no partition table — which is a fact
    /// about the device rather than a failure, and is exactly what a card
    /// formatted as one bare volume looks like. A caller that wants to
    /// handle both shapes falls back to mounting at block 0.
    pub fn read<D: BlockDevice>(device: &mut D) -> Result<Option<Self>, D::Error> {
        let mut block = [0u8; BLOCK_SIZE];
        device.read(0, &mut block).map_err(Error::device)?;
        Ok(Self::parse(&block))
    }

    /// Parses a block that may or may not be a partition table.
    ///
    /// See the module documentation for what has to hold before this says
    /// yes. Getting it wrong in the permissive direction is the expensive
    /// mistake: a boot sector read as a table gives block numbers that are
    /// wrong but plausible, and every access afterwards lands somewhere
    /// arbitrary.
    ///
    /// A block shorter than a sector is not a table, and comes back as
    /// `None` rather than panicking on the signature it cannot reach.
    pub fn parse(block: &[u8]) -> Option<Self> {
        // Checked in release too, not asserted: the offsets below are fixed,
        // so a short slice would panic rather than answer.
        if block.len() < BLOCK_SIZE {
            return None;
        }

        if block[0x1FE] != 0x55 || block[0x1FF] != 0xAA {
            return None;
        }

        let mut entries = [None; MAX_PARTITIONS];
        let mut any = false;
        for (index, slot) in entries.iter_mut().enumerate() {
            let at = TABLE + index * ENTRY_SIZE;
            let status = block[at + field::STATUS];
            if status != STATUS_INACTIVE && status != STATUS_BOOTABLE {
                return None;
            }

            let kind = block[at + field::KIND];
            let first_block = u64::from(read_u32(block, at + field::FIRST_BLOCK));
            let blocks = u64::from(read_u32(block, at + field::BLOCKS));

            // A partition of nothing, or one starting at the block that
            // holds the table describing it, is not a partition.
            if kind == 0 || blocks == 0 || first_block == 0 {
                continue;
            }

            any = true;
            *slot = Some(Partition {
                index,
                bootable: status == STATUS_BOOTABLE,
                kind,
                first_block,
                blocks,
            });
        }

        any.then_some(PartitionTable { entries })
    }

    /// The partition in table slot `index`, if that slot is in use.
    pub fn get(&self, index: usize) -> Option<&Partition> {
        self.entries.get(index)?.as_ref()
    }

    /// Every partition the table describes, in table order.
    ///
    /// Unused slots are skipped, so the first item is not necessarily slot
    /// zero — which is why [`Partition::index`] is carried rather than left
    /// to the caller to count.
    pub fn iter(&self) -> impl Iterator<Item = &Partition> {
        self.entries.iter().flatten()
    }

    /// How many of the four slots are in use.
    pub fn len(&self) -> usize {
        self.iter().count()
    }

    /// Whether the table describes no partitions.
    ///
    /// Never true for a table this crate produced: a block with no usable
    /// entries is reported as not being a table at all.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The first partition whose type byte says FAT.
    ///
    /// The usual way to find the volume on a card: imaging tools put it
    /// first, but not always in slot zero, and a card with a separate boot
    /// and data partition has more than one.
    pub fn first_fat(&self) -> Option<&Partition> {
        self.iter().find(|partition| partition.is_fat())
    }
}

fn read_u32(block: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([block[at], block[at + 1], block[at + 2], block[at + 3]])
}
