//! Proving the test harness before anything trusts it.
//!
//! No filesystem code is exercised here. These tests establish that the
//! oracles are wired up and that the recording device reports what actually
//! reached the image — so that when a later phase says "`fsck` is happy" or
//! "that read cost one device call", the statement means something.
//!
//! The negative controls are the point. An oracle that has only ever been
//! seen to pass is indistinguishable from one that is not running.

mod support;

use resident_fat::blockdev::{BLOCK_SIZE, BlockDevice};
use support::*;

/// `fsck` accepts a volume nothing in this repository has touched.
///
/// The baseline: if this failed, every later "fsck is clean" would be
/// meaningless, because it would be reporting on the harness rather than on
/// what we wrote.
#[test]
fn fsck_accepts_an_untouched_image() {
    let image = mkfs_image("untouched.img", 272, 4);
    assert_fsck_clean(&image);
}

/// `fsck` rejects a volume with a broken cluster chain.
///
/// The other half of the baseline, and the more important half. This
/// deliberately corrupts a copy and requires a complaint: without it, a
/// silently-not-running `fsck` would look exactly like a passing one.
#[test]
fn fsck_reports_a_corrupted_image() {
    let image = mkfs_image("corrupt.img", 272, 4);
    mcopy_in(
        &image,
        host_file("victim.txt", b"data that needs a cluster\n"),
        "/VICTIM.TXT",
    );
    assert_fsck_clean(&image);

    // Point the file's directory entry at a cluster number far past the end
    // of the volume. `fsck` reports a bad start cluster; the filesystem
    // under test will eventually have to reject the same thing, which is
    // why this is the corruption chosen.
    let mut device = FileImage::open_rw(&image).expect("could not open the scratch image");
    let root = root_directory_block(&mut device);
    let mut block = [0u8; BLOCK_SIZE];
    device
        .read(root, &mut block)
        .expect("could not read the root directory");

    let entry =
        find_entry(&block, b"VICTIM  TXT").expect("VICTIM.TXT is not in the root directory");
    block[entry + 0x14..entry + 0x16].copy_from_slice(&0x0FFFu16.to_le_bytes());
    block[entry + 0x1A..entry + 0x1C].copy_from_slice(&0xFFFFu16.to_le_bytes());
    device
        .write(root, &block)
        .expect("could not write the root directory");
    drop(device);

    let report = fsck(&image);
    assert!(
        !report.clean,
        "fsck accepted an image with a start cluster past the end of the volume, \
         so it cannot be trusted to report real corruption:\n{}",
        report.output
    );
}

/// A file survives a round trip through `mtools` unchanged.
///
/// Establishes the other oracle: `mdir` and `mcopy` are used later to check
/// names and contents against an implementation that is not ours, so they
/// have to be known to move bytes faithfully first.
#[test]
fn mtools_round_trips_a_file() {
    let image = mkfs_image("roundtrip.img", 272, 4);

    // Longer than one cluster, so this crosses the allocator rather than
    // fitting in a single block.
    let original: Vec<u8> = (0..40_000u32).map(|n| (n % 251) as u8).collect();
    let source = host_file("roundtrip.bin", &original);
    mcopy_in(&image, &source, "/ROUND.BIN");
    assert_fsck_clean(&image);

    let listing = mdir(&image, "/");
    assert!(
        listing.contains("ROUND"),
        "mdir does not list the file that was just copied in:\n{listing}"
    );

    let returned = scratch_dir().join("roundtrip.out");
    let _ = std::fs::remove_file(&returned);
    mcopy_out(&image, "/ROUND.BIN", &returned);

    let actual = std::fs::read(&returned).expect("could not read the file mtools returned");
    assert_eq!(
        actual.len(),
        original.len(),
        "round trip changed the file's length"
    );
    assert_eq!(actual, original, "round trip changed the file's contents");
}

/// The recorder reports exactly the calls that were made, with their
/// lengths — the measurement every later phase's claims rest on.
#[test]
fn the_recorder_reports_calls_and_their_lengths() {
    let mut device = Recorder::new(FileImage::open(fixture("fat32-4k.img")).expect("open"));

    let mut one = [0u8; BLOCK_SIZE];
    device.read(0, &mut one).expect("read the boot sector");

    let mut many = vec![0u8; BLOCK_SIZE * 64];
    device.read(2048, &mut many).expect("read a 64-block run");

    let accesses = device.accesses();
    assert_eq!(accesses.len(), 2, "expected two calls, got {accesses:?}");
    assert_eq!(
        accesses[0],
        Access {
            direction: Direction::Read,
            start_block: 0,
            blocks: 1
        }
    );
    assert_eq!(
        accesses[1],
        Access {
            direction: Direction::Read,
            start_block: 2048,
            blocks: 64
        }
    );

    // The pair that distinguishes one long transfer from many short ones,
    // which is the distinction this crate exists to make.
    assert_eq!(device.call_count(), 2);
    assert_eq!(device.blocks_moved(), 65);

    device.reset();
    assert_eq!(device.call_count(), 0, "reset did not clear the recording");
}

/// A read past the end of a device is an error, not a short read.
#[test]
fn reading_past_the_end_is_an_error() {
    let mut device = MemoryImage::load(fixture("fat32-frag.img")).expect("load the small fixture");
    let blocks = device
        .block_count()
        .expect("block count")
        .expect("a memory image knows its own size");

    let mut block = [0u8; BLOCK_SIZE];
    device
        .read(blocks - 1, &mut block)
        .expect("the last block is readable");
    assert!(
        device.read(blocks, &mut block).is_err(),
        "reading one block past the end should fail"
    );
}

/// Every fixture the later phases name is present and consistent.
///
/// Cheap, and it turns "the fixtures were never built" into one clear
/// failure rather than a scattering of confusing ones.
#[test]
fn every_fixture_is_present_and_consistent() {
    for name in [
        "fat32-4k.img",
        "fat32-16k.img",
        "fat32-32k.img",
        "fat32-frag.img",
    ] {
        assert_fsck_clean(fixture(name));
    }
    assert!(
        fixtures_dir().join("manifest.json").exists(),
        "the fixture manifest is missing; run `make fixtures`"
    );
}

// ---------------------------------------------------------------------------
// Helpers local to this file
// ---------------------------------------------------------------------------

/// Writes a file into the scratch directory and returns its path.
fn host_file(name: &str, contents: &[u8]) -> std::path::PathBuf {
    let path = scratch_dir().join(name);
    std::fs::write(&path, contents).expect("could not write a scratch file");
    path
}

/// The device block holding the start of the root directory.
///
/// Reads only the fields it needs straight out of the boot sector. This is
/// harness code deliberately kept separate from the crate: a test that
/// located the root directory by calling the code under test would agree
/// with that code's bugs.
fn root_directory_block(device: &mut impl BlockDevice) -> u64 {
    let mut boot = [0u8; BLOCK_SIZE];
    device
        .read(0, &mut boot)
        .expect("could not read the boot sector");

    let sectors_per_cluster = boot[0x0D] as u64;
    let reserved = u16::from_le_bytes([boot[0x0E], boot[0x0F]]) as u64;
    let fats = boot[0x10] as u64;
    let sectors_per_fat =
        u32::from_le_bytes([boot[0x24], boot[0x25], boot[0x26], boot[0x27]]) as u64;
    let root_cluster = u32::from_le_bytes([boot[0x2C], boot[0x2D], boot[0x2E], boot[0x2F]]) as u64;

    reserved + fats * sectors_per_fat + (root_cluster - 2) * sectors_per_cluster
}

/// Offset of the directory entry with this 8.3 name, if it is in `block`.
fn find_entry(block: &[u8], name: &[u8; 11]) -> Option<usize> {
    block
        .chunks_exact(32)
        .position(|entry| &entry[..11] == name.as_slice())
        .map(|index| index * 32)
}
