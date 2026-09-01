//! Reading partition tables.
//!
//! Half of what a partition needs is in `partition.rs`, which checks that a
//! volume mounted at an offset behaves like one that starts at block 0.
//! This file is the other half: finding that offset, and — the part that
//! takes care — deciding whether block 0 is a partition table at all.
//!
//! A boot sector and a partition table end with the same `0x55AA`, so the
//! obvious check answers yes to either. Reading one as the other is the
//! expensive mistake, because the block numbers it invents are wrong but
//! plausible, so each of the checks that tell them apart is asserted on its
//! own.

#![cfg(feature = "mbr")]

mod support;

use resident_fat::mbr::PartitionTable;
use resident_fat::{BootError, Error, FileSystem, Geometry};
use support::*;

/// Where `scripts/mkfixtures.sh` puts the partition.
const PARTITION_START: u64 = 8192;

/// The table is read, and describes the partition the fixture built.
#[test]
fn the_partition_table_is_read() {
    let mut device = FileImage::open(fixture("fat32-mbr.img")).expect("open");
    let table = PartitionTable::read(&mut device)
        .expect("read")
        .expect("the fixture has a partition table");

    assert_eq!(table.len(), 1);
    let partition = table.get(0).expect("slot 0 is in use");
    assert_eq!(partition.index, 0);
    assert_eq!(partition.first_block, PARTITION_START);
    assert_eq!(partition.blocks, 1_048_576);
    assert_eq!(partition.kind, 0x0C, "FAT32 with LBA addressing");
    assert!(partition.bootable);
    assert!(partition.is_fat());
    assert!(!partition.is_protective_gpt());

    // The convenience path, for a caller that does not know the slot.
    assert_eq!(table.first_fat(), Some(partition));
    assert_eq!(table.get(1), None);
    assert_eq!(table.get(9), None, "an out-of-range slot is not a panic");
}

/// Mounting by partition index gives the same volume as mounting at the
/// offset by hand.
#[test]
fn a_partition_mounts_by_index() {
    let mut by_index =
        FileSystem::mount_partition(FileImage::open(fixture("fat32-mbr.img")).expect("open"), 0)
            .expect("mount partition 0");
    let mut by_offset = FileSystem::mount_at(
        FileImage::open(fixture("fat32-mbr.img")).expect("open"),
        PARTITION_START,
    )
    .expect("mount at the offset");

    assert_eq!(by_index.boot_sector(), by_offset.boot_sector());
    let a = by_index.open("/BIG.BIN").expect("open");
    let b = by_offset.open("/BIG.BIN").expect("open");
    assert_eq!(
        by_index.read_all(&a).expect("read"),
        by_offset.read_all(&b).expect("read")
    );

    // An empty slot is refused by name rather than mounted as block 0,
    // which is what makes the raw-slot numbering safe to rely on.
    match FileSystem::mount_partition(FileImage::open(fixture("fat32-mbr.img")).expect("open"), 1) {
        Err(Error::NoSuchPartition { index: 1 }) => {}
        other => panic!("expected NoSuchPartition, got {other:?}"),
    }
}

/// A device with no partition table says so, rather than reading a boot
/// sector's tail as four partition entries.
///
/// The trap this guards is that a boot sector ends with the same `0x55AA` a
/// partition table does, so the obvious check answers yes to both. Getting
/// it wrong the permissive way is the expensive direction: the offsets it
/// invents are wrong but plausible, and every access afterwards lands
/// somewhere arbitrary on the device.
#[test]
fn a_bare_volume_is_not_mistaken_for_a_partition_table() {
    for name in ["fat32-4k.img", "fat32-frag.img", "fat16.img", "fat12.img"] {
        let mut device = FileImage::open(fixture(name)).expect("open");
        assert_eq!(
            PartitionTable::read(&mut device).expect("read"),
            None,
            "{name}: a boot sector was read as a partition table"
        );

        match FileSystem::mount_partition(FileImage::open(fixture(name)).expect("open"), 0) {
            Err(Error::NoPartitionTable) => {}
            other => panic!("{name}: expected NoPartitionTable, got {other:?}"),
        }
    }
}

/// The checks that make that distinction, one at a time.
///
/// Asserted individually because each is load-bearing on its own: a table
/// that passed on the signature alone would accept every boot sector, and
/// one that skipped the status check would accept boot code that happened
/// to contain a plausible block number.
#[test]
fn the_table_checks_are_each_load_bearing() {
    let good = std::fs::read(fixture("fat32-mbr.img")).expect("read");
    let good = &good[..512];
    assert!(PartitionTable::parse(good).is_some(), "the fixture parses");

    let mut no_signature = good.to_vec();
    no_signature[0x1FE] = 0;
    assert_eq!(PartitionTable::parse(&no_signature), None, "signature");

    // A status byte is `0x00` or `0x80` and nothing else, so anything else
    // there means these bytes are not a partition table.
    let mut bad_status = good.to_vec();
    bad_status[0x1BE] = 0x42;
    assert_eq!(PartitionTable::parse(&bad_status), None, "status byte");

    // A partition of type zero is an unused slot; with no other slot in use
    // there is no table worth reporting.
    let mut no_kind = good.to_vec();
    no_kind[0x1BE + 4] = 0;
    assert_eq!(PartitionTable::parse(&no_kind), None, "partition type");

    // A partition starting at block 0 would contain the table describing
    // it, which cannot be right.
    let mut at_zero = good.to_vec();
    at_zero[0x1BE + 8..0x1BE + 12].copy_from_slice(&0u32.to_le_bytes());
    assert_eq!(PartitionTable::parse(&at_zero), None, "first block");

    // A partition of no length is not a partition.
    let mut empty = good.to_vec();
    empty[0x1BE + 12..0x1BE + 16].copy_from_slice(&0u32.to_le_bytes());
    assert_eq!(PartitionTable::parse(&empty), None, "length");
}

/// The placeholder table on a GPT disk is recognised and declined.
///
/// A GPT disk carries a master boot record with one entry of type `0xEE`
/// spanning the device, so that a tool understanding only partition tables
/// sees it as full rather than as empty and available. Mounting through it
/// would treat the GPT header as a volume.
#[test]
fn a_protective_gpt_table_is_declined() {
    let mut protective = std::fs::read(fixture("fat32-mbr.img")).expect("read")[..512].to_vec();
    protective[0x1BE + 4] = 0xEE;
    protective[0x1BE + 8..0x1BE + 12].copy_from_slice(&1u32.to_le_bytes());

    let table = PartitionTable::parse(&protective).expect("it is still a table");
    let entry = table.get(0).expect("slot 0");
    assert!(entry.is_protective_gpt());
    assert!(!entry.is_fat());

    // And mounting through it is refused rather than attempted.
    let mut image = std::fs::read(fixture("fat32-mbr.img")).expect("read");
    image[..512].copy_from_slice(&protective);
    let scratch = scratch_dir().join("protective-gpt.img");
    std::fs::write(&scratch, &image).expect("write");
    match FileSystem::mount_partition(FileImage::open(&scratch).expect("open"), 0) {
        Err(Error::NoPartitionTable) => {}
        other => panic!("expected NoPartitionTable, got {other:?}"),
    }
}

/// A volume claiming more sectors than its partition holds is refused, even
/// on a device that will not say how big it is.
///
/// The partition table already knows the answer, so the bound does not have
/// to come from the device. That matters because the drivers most likely to
/// decline the question are the ones on removable media, which is exactly
/// where partition tables are found — leaving this to `block_count` alone
/// would skip the check on the hardware that needs it most.
#[test]
fn a_volume_overflowing_its_partition_is_refused() {
    let mut image = MemoryImage::load(fixture("fat32-mbr.img")).expect("load");

    // Claim the volume runs to 0x00FF_FFFF sectors, far past the partition
    // and, since the fixture is not that large, past the device too.
    let boot = PARTITION_START as usize * 512;
    image.bytes_mut()[boot + 0x13..boot + 0x15].copy_from_slice(&0u16.to_le_bytes());
    image.bytes_mut()[boot + 0x20..boot + 0x24].copy_from_slice(&0x00FF_FFFFu32.to_le_bytes());

    // Through the partition table, with the device declining to report its
    // size — so the partition's length is the only bound available.
    match FileSystem::mount_partition(Reticent(image), 0).map(|_| ()) {
        Err(Error::Boot(BootError::BadGeometry(Geometry::VolumeTooLarge { .. }))) => {}
        other => panic!("expected VolumeTooLarge, got {other:?}"),
    }
}
