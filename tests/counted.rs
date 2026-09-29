//! Counting what a volume asks its device to do.
//!
//! The counts are only worth having if they are exact, since what they are
//! read for is the ratio between calls and blocks — so these check the
//! numbers themselves against transfers whose shape is known.

mod support;

use resident_fat::FileSystem;
use resident_fat::counted::{Counted, Counters, Counts};
use support::*;

/// `BIG.BIN` in this fixture: 1,600 KiB, laid out contiguously.
const BIG_BLOCKS: u32 = 1600 * 1024 / 512;

/// A contiguous file read whole is one call covering every block — the
/// figure a block-at-a-time implementation would have as 3,200 calls.
#[test]
fn a_whole_file_read_is_counted_exactly() {
    let counters = Counters::new();
    let device = Counted::new(
        FileImage::open(fixture("fat32-4k.img")).expect("open"),
        &counters,
    );
    let mut volume = FileSystem::mount(device).expect("mount");
    let file = volume.open("/BIG.BIN").expect("open");

    let before = counters.now();
    volume.read_all(&file).expect("read");
    let read = counters.since(before);

    assert_eq!(
        read,
        Counts {
            read_calls: 1,
            read_blocks: BIG_BLOCKS,
            write_calls: 0,
            write_blocks: 0,
        }
    );
    assert_eq!(read.blocks_per_call_x10(), BIG_BLOCKS * 10);

    // Mounting read the device too, and that is in the totals: `now` is
    // everything, `since` is the part a caller asked about.
    assert!(counters.now().read_calls > 1);
}

/// The device's transfer limit is passed through, so a counted device with
/// a limit is still split to that limit — and the calls say so.
///
/// `Capped` panics on a transfer longer than its cap, so a wrapper that let
/// the default of "no limit" through would fail here rather than miscount.
#[test]
fn the_transfer_limit_is_forwarded_and_the_split_is_counted() {
    const CAP: u32 = 64;
    let counters = Counters::new();
    let device = Counted::new(
        Capped::new(
            FileImage::open(fixture("fat32-4k.img")).expect("open"),
            u64::from(CAP),
        ),
        &counters,
    );
    let mut volume = FileSystem::mount(device).expect("mount");
    let file = volume.open("/BIG.BIN").expect("open");

    let before = counters.now();
    volume.read_all(&file).expect("read");
    let read = counters.since(before);

    assert_eq!(read.read_calls, BIG_BLOCKS / CAP);
    assert_eq!(read.read_blocks, BIG_BLOCKS);
    assert_eq!(read.blocks_per_call_x10(), CAP * 10);
}

/// Writes are counted on their own side, and the unwrapped device is the
/// one that was wrapped.
#[test]
fn writes_are_counted_separately() {
    let counters = Counters::new();
    // A fresh volume in the scratch directory, since the fixtures are shared
    // and the small one is deliberately full.
    let image = FileImage::open_rw(mkfs_image("counted-write.img", 272, 4)).expect("open");
    let mut volume = FileSystem::mount(Counted::new(image, &counters)).expect("mount");

    let before = counters.now();
    volume
        .write_file("/NEW.BIN", &[0x5A; 3 * 512])
        .expect("write");
    volume.sync().expect("sync");
    let wrote = counters.since(before);

    assert!(wrote.write_calls > 0, "{wrote}");
    assert!(
        wrote.write_blocks >= 3,
        "three blocks of data at least: {wrote}"
    );
    let _image: FileImage = volume.into_device().into_inner();
}
