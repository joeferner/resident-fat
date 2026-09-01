//! The dirty-volume flag.
//!
//! One bit in the boot sector, and the only record a FAT volume keeps that
//! a writer did not finish. Nothing else on the volume says so: the write
//! ordering makes an interruption leak clusters rather than corrupt them,
//! but a leak is invisible until something goes looking, and "invisible"
//! is the wrong answer to "did the last update complete?".
//!
//! The flag has to be set *before* the change it warns about and cleared
//! *after* everything has landed, or it says nothing at all — a flag
//! written after the write it covers is set only when it was not needed.
//! Both orderings are asserted here from the device's own record of what
//! happened, rather than inferred.

mod support;

use resident_fat::FileSystem;
use support::*;

/// Whether the volume's boot sector has the dirty bit set, read straight
/// out of the image.
fn dirty_bit(image: &std::path::Path) -> bool {
    let boot = std::fs::read(image).expect("read the image");
    boot[0x41] & 0x01 != 0
}

/// A volume dropped without a sync comes back dirty, and says so at mount.
#[test]
fn an_unsynced_volume_comes_back_dirty() {
    let image = mkfs_image("dirty-unsynced.img", 16, 4);
    assert!(!dirty_bit(&image), "a fresh volume is not dirty");

    {
        let mut volume =
            FileSystem::mount(FileImage::open_rw(&image).expect("open")).expect("mount");
        assert!(
            !volume.is_dirty(),
            "nothing has happened to this volume yet"
        );
        volume
            .write_file("/LOST.BIN", &expected_content(4000))
            .expect("write");
        // No sync. This stands in for the power going out, which is the
        // case the flag exists for.
    }

    assert!(dirty_bit(&image), "an interrupted write left no warning");
    let volume = FileSystem::mount(FileImage::open(&image).expect("open")).expect("remount");
    assert!(
        volume.is_dirty(),
        "the crate should report the volume as unclean"
    );

    // And the oracle agrees that this is what it is looking at.
    let report = fsck(&image);
    assert!(
        report.output.contains("Dirty bit is set"),
        "fsck did not see the flag:\n{}",
        report.output
    );
    // A leak at worst: the ordering rules still hold, so nothing is shared
    // or dangling even though the volume was abandoned mid-update.
    assert_eq!(report.indicates_corruption(), None, "{}", report.output);
}

/// A volume that was synced comes back clean, and the file is there.
#[test]
fn a_synced_volume_comes_back_clean() {
    let image = mkfs_image("dirty-synced.img", 16, 4);
    let data = expected_content(4000);

    let mut volume = FileSystem::mount(FileImage::open_rw(&image).expect("open")).expect("mount");
    volume.write_file("/KEPT.BIN", &data).expect("write");
    volume.sync().expect("sync");
    assert!(!dirty_bit(&image), "syncing should have cleared the flag");
    drop(volume);

    assert_fsck_clean(&image);
    let mut volume = FileSystem::mount(FileImage::open(&image).expect("open")).expect("remount");
    assert!(!volume.is_dirty());
    let file = volume.open("/KEPT.BIN").expect("open");
    assert_eq!(volume.read_all(&file).expect("read"), data);
}

/// `unmount` is `sync` plus getting the device back, and leaves the volume
/// clean.
#[test]
fn unmounting_syncs_and_returns_the_device() {
    let image = mkfs_image("dirty-unmount.img", 16, 4);
    let mut volume = FileSystem::mount(FileImage::open_rw(&image).expect("open")).expect("mount");
    volume
        .write_file("/KEPT.BIN", &expected_content(1000))
        .expect("write");

    let device = volume.unmount().expect("unmount");
    drop(device);

    assert!(!dirty_bit(&image));
    assert_fsck_clean(&image);
}

/// Reading a volume writes nothing to it, flag included.
///
/// A card can be physically write-protected, or deliberately mounted to be
/// read and nothing else. Setting the flag on mount would make every such
/// use fail, or silently mark a volume unclean that nobody touched — which
/// is why the flag is set on the first change rather than at mount.
#[test]
fn a_read_only_workload_writes_nothing() {
    let image = fixture("fat32-4k.img");
    let device = Recorder::new(FileImage::open(&image).expect("open"));
    let mut volume = FileSystem::mount(device).expect("mount");

    volume.root_dir().expect("root");
    volume.open_dir("/ROMS").expect("subdirectory");
    let file = volume.open("/BIG.BIN").expect("open");
    volume.read_all(&file).expect("read");
    volume.sync().expect("sync with nothing to sync");

    assert_eq!(
        volume.device_mut().writes(),
        vec![],
        "reading a volume should not have written to it"
    );
}

/// The flag reaches the device before the change it is warning about, and
/// is cleared only after everything else has landed.
///
/// This is the whole property. A flag written after its own change is set
/// exactly when it was not needed, and cleared before the last write is a
/// volume that claims to be consistent while it is not — both leave the bit
/// technically present and useless.
#[test]
fn the_flag_brackets_every_change() {
    let image = mkfs_image("dirty-ordering.img", 16, 4);
    let device = Recorder::new(FileImage::open_rw(&image).expect("open"));
    let mut volume = FileSystem::mount(device).expect("mount");
    volume.device_mut().reset();

    volume
        .write_file("/BRACKET.BIN", &expected_content(9000))
        .expect("write");
    let writes = volume.device_mut().writes();
    assert!(writes.len() > 2, "this should have taken several writes");
    assert_eq!(
        writes[0].start_block, 0,
        "the first write of a change must be the boot sector: {writes:?}"
    );

    volume.sync().expect("sync");
    let writes = volume.device_mut().writes();
    assert_eq!(
        writes.last().expect("writes").start_block,
        0,
        "the last write of a sync must be the boot sector: {writes:?}"
    );

    // And exactly two boot-sector writes for the whole cycle: one to set
    // the flag, one to clear it. Setting it per operation would cost a
    // read-modify-write of the boot sector for every file.
    assert_eq!(
        writes.iter().filter(|w| w.start_block == 0).count(),
        2,
        "the flag should be written once each way: {writes:?}"
    );
}

/// A volume that arrived dirty and was only read keeps its warning.
///
/// This crate has not repaired anything. Clearing a flag it did not set
/// would throw away the one signal that something needs looking at, and
/// `sync` is not a repair.
#[test]
fn a_volume_that_arrived_dirty_is_not_quietly_cleaned() {
    let image = mkfs_image("dirty-inherited.img", 16, 4);
    {
        let mut volume =
            FileSystem::mount(FileImage::open_rw(&image).expect("open")).expect("mount");
        volume.create("/A.BIN").expect("create");
    }
    assert!(dirty_bit(&image));

    let mut volume = FileSystem::mount(FileImage::open_rw(&image).expect("open")).expect("remount");
    assert!(volume.is_dirty());
    volume.open("/A.BIN").expect("open");
    volume.sync().expect("sync");
    drop(volume);

    assert!(
        dirty_bit(&image),
        "a sync that wrote nothing should not have cleared someone else's warning"
    );
}
