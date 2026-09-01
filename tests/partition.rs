//! Mounting a volume that does not start at block 0.
//!
//! Every other fixture is a bare volume, which is the convenient shape and
//! not the common one. A card written by an imaging tool has a partition
//! table in block 0 and the filesystem some way after it, so mounting one
//! means treating every block number in the volume as relative to an offset.
//!
//! That is a different code path from mounting at zero, it is the path a
//! real consumer boots through, and until these tests it was not exercised
//! at all: `mount_at` was public, documented, and called by nothing.

mod support;

use resident_fat::{BootError, Error, FileSystem};
use support::*;

/// Where `scripts/mkfixtures.sh` puts the partition.
const PARTITION_START: u64 = 8192;

/// A volume inside a partition mounts, and reads the same bytes as one that
/// starts at block 0.
#[test]
fn a_partitioned_volume_mounts_at_its_offset() {
    let mut volume = FileSystem::mount_at(
        FileImage::open(fixture("fat32-mbr.img")).expect("open"),
        PARTITION_START,
    )
    .expect("mount at the partition offset");

    let file = volume.open("/BIG.BIN").expect("open BIG.BIN");
    let expected = expected_content(1600 * 1024);
    assert_eq!(file.len() as usize, expected.len());
    assert_eq!(
        volume.read_all(&file).expect("read"),
        expected,
        "reading through an offset gave different bytes"
    );

    let hello = volume.open("/HELLO.TXT").expect("open HELLO.TXT");
    assert_eq!(
        volume.read_all(&hello).expect("read"),
        b"resident-fat fixture\n",
    );

    // The offset has to reach the directory and the allocation table too,
    // not just file data — a volume that got only the data right would read
    // one file correctly and enumerate nothing.
    let names: Vec<String> = volume
        .root_dir()
        .expect("root")
        .iter()
        .map(|entry| entry.name().to_owned())
        .collect();
    assert!(
        names.iter().any(|name| name == "BIG.BIN"),
        "the root directory did not list its files: {names:?}"
    );
}

/// Mounting a partitioned card at block 0 fails rather than reading the
/// partition table as a volume.
///
/// This is the mistake a consumer makes first, and the failure has to be
/// loud. A partition table ends with the same `0x55AA` signature a boot
/// sector does, so the cheapest check passes and the geometry fields are
/// read out of boot code — which is how a mount "succeeds" and then reads
/// from arbitrary places on the card.
#[test]
fn mounting_a_partitioned_card_at_zero_is_refused() {
    let result = FileSystem::mount(FileImage::open(fixture("fat32-mbr.img")).expect("open"));
    match result {
        Err(Error::Boot(BootError::NotFat | BootError::BadGeometry(_))) => {}
        Err(other) => panic!("expected a refusal naming the format, got {other:?}"),
        Ok(_) => panic!("mounted a partition table as if it were a volume"),
    }
}

/// The same volume read at its offset and read on its own give identical
/// answers.
///
/// The offset arithmetic is the whole risk here, and an error in it that was
/// consistent between reads would pass every check above. Comparing against
/// the partition extracted to its own file is what makes that impossible:
/// the second volume genuinely starts at block 0, so nothing it reports
/// depends on the arithmetic being tested.
#[test]
fn an_offset_volume_agrees_with_the_same_volume_extracted() {
    let extracted = scratch_dir().join("partition-extracted.img");
    let whole = std::fs::read(fixture("fat32-mbr.img")).expect("read the fixture");
    std::fs::write(&extracted, &whole[PARTITION_START as usize * 512..])
        .expect("write the extracted partition");

    let mut offset = FileSystem::mount_at(
        FileImage::open(fixture("fat32-mbr.img")).expect("open"),
        PARTITION_START,
    )
    .expect("mount at offset");
    let mut bare = FileSystem::mount(FileImage::open(&extracted).expect("open")).expect("mount");

    assert_eq!(
        offset.boot_sector(),
        bare.boot_sector(),
        "the two mounts disagree about the volume's geometry"
    );
    assert_eq!(offset.fat().cluster_count(), bare.fat().cluster_count());

    for path in ["/BIG.BIN", "/HELLO.TXT"] {
        let a = offset.open(path).expect("open through the offset");
        let b = bare.open(path).expect("open bare");
        assert_eq!(a.first_cluster(), b.first_cluster(), "{path} start cluster");
        assert_eq!(
            offset.read_all(&a).expect("read"),
            bare.read_all(&b).expect("read"),
            "{path} read back differently"
        );
    }

    // And the run detection agrees, which is what the offset would break in
    // a way file content might not show.
    let big = offset.open("/BIG.BIN").expect("open");
    assert_eq!(
        offset.runs(big.first_cluster()).expect("runs"),
        bare.runs(big.first_cluster()).expect("runs")
    );
}

/// Writing through an offset lands inside the partition, and `fsck` accepts
/// the result.
///
/// The failure this rules out is the one that matters most: an offset
/// applied on read but not on write puts data outside the partition, where
/// it corrupts whatever else is on the card while the volume still reads
/// back correctly from memory.
#[test]
fn writing_through_an_offset_stays_inside_the_partition() {
    let image = scratch_dir().join("partition-write.img");
    std::fs::copy(fixture("fat32-mbr.img"), &image).expect("copy the fixture");

    let payload = expected_content(300_000);
    let mut volume =
        FileSystem::mount_at(FileImage::open_rw(&image).expect("open"), PARTITION_START)
            .expect("mount");
    volume.write_file("/NEW.BIN", &payload).expect("write");
    volume.unmount().expect("unmount");

    // Nothing before the partition may have been touched: the boot code and
    // the partition table are exactly where they were.
    let before = std::fs::read(fixture("fat32-mbr.img")).expect("read the fixture");
    let after = std::fs::read(&image).expect("read the written image");
    let table = PARTITION_START as usize * 512;
    assert_eq!(
        before[..table],
        after[..table],
        "the write reached outside the partition"
    );

    // And an independent reader, pointed at the partition, sees the file.
    let extracted = scratch_dir().join("partition-write-extracted.img");
    std::fs::write(&extracted, &after[table..]).expect("extract");
    assert_fsck_clean(&extracted);
    let out = scratch_dir().join("partition-write-out.bin");
    let _ = std::fs::remove_file(&out);
    mcopy_out(&extracted, "/NEW.BIN", &out);
    assert_eq!(std::fs::read(&out).expect("read back"), payload);
}
