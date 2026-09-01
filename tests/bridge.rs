//! The `embedded-sdmmc` block device bridge.
//!
//! The bridge exists so a device already written for the de-facto embedded
//! block device trait works here unchanged. Reading the right bytes through
//! it is the easy half and not the interesting one — a bridge that issued
//! one command per block would also read the right bytes, and would give
//! away the entire reason for using this crate.
//!
//! So these tests count commands. The device under them implements
//! `embedded-sdmmc`'s trait and knows nothing about this crate, which is the
//! situation a consumer is actually in.

#![cfg(feature = "embedded-sdmmc")]

mod support;

use resident_fat::FileSystem;
use resident_fat::bridge::{DEFAULT_STAGING_BLOCKS, FromEmbeddedSdmmc};
use support::*;

/// A volume on a foreign block device mounts and reads correctly.
#[test]
fn a_bridged_device_reads_the_right_bytes() {
    let expected = layout("fat32-4k.img");
    let size = expected.file("/BIG.BIN").size as usize;

    let device =
        FromEmbeddedSdmmc::new(SdmmcImage::open_rw(fixture("fat32-4k.img")).expect("open"));
    let mut volume = FileSystem::mount(device).expect("mount");

    let file = volume.open("/BIG.BIN").expect("open");
    assert_eq!(file.len() as usize, size);
    assert_eq!(
        volume.read_all(&file).expect("read"),
        expected_content(size),
        "the bridge changed the bytes"
    );

    // And a directory walk works, which is the other thing a consumer will
    // do first.
    assert_eq!(volume.open_dir("/ROMS").expect("open /ROMS").len(), 302);
}

/// A megabyte read reaches the foreign device as a handful of long commands
/// rather than thousands of short ones.
///
/// This is the whole point of the bridge, and the number it has to beat is
/// not hypothetical: `embedded-sdmmc`'s own volume manager caches a single
/// block, so the same read through it is one command per block. Anything
/// close to that here would mean the bridge had thrown the advantage away in
/// the course of adapting to the trait.
#[test]
fn a_long_read_becomes_few_commands() {
    let expected = layout("fat32-4k.img");
    let size = expected.file("/BIG.BIN").size as usize;
    let data_blocks = size.div_ceil(512);

    let device =
        FromEmbeddedSdmmc::new(SdmmcImage::open_rw(fixture("fat32-4k.img")).expect("open"));
    let mut volume = FileSystem::mount(device).expect("mount");
    let file = volume.open("/BIG.BIN").expect("open");

    volume.device().inner().reset();
    volume.read_all(&file).expect("read");

    let device = volume.device().inner();
    assert_eq!(
        device.blocks_moved(),
        data_blocks,
        "the bridge should move exactly the file's blocks"
    );
    assert_eq!(
        device.calls(),
        data_blocks.div_ceil(DEFAULT_STAGING_BLOCKS),
        "a contiguous file should cost one command per staged buffer"
    );
    assert_eq!(
        device.blocks_per_call(),
        DEFAULT_STAGING_BLOCKS,
        "every command should be a full buffer"
    );
}

/// The staging size is the command length, and the filesystem respects it.
///
/// Both directions are asserted because both are load-bearing: the bridge
/// reports its capacity through `max_transfer_blocks`, and a filesystem that
/// ignored it would hand over a slice the staging buffer cannot hold. One
/// block is included as the degenerate case — it is what a single-block
/// implementation does, and having the number here is what makes the others
/// mean something.
#[test]
fn the_staging_size_decides_the_command_length() {
    let expected = layout("fat32-4k.img");
    let size = expected.file("/BIG.BIN").size as usize;
    let data_blocks = size.div_ceil(512);

    for staging in [1usize, 8, 64, 512] {
        let device = FromEmbeddedSdmmc::with_blocks(
            SdmmcImage::open_rw(fixture("fat32-4k.img")).expect("open"),
            staging,
        );
        let mut volume = FileSystem::mount(device).expect("mount");
        let file = volume.open("/BIG.BIN").expect("open");

        volume.device().inner().reset();
        let read = volume.read_all(&file).expect("read");

        let device = volume.device().inner();
        assert_eq!(
            device.calls(),
            data_blocks.div_ceil(staging),
            "staging {staging} blocks gave the wrong command count"
        );
        assert_eq!(read.len(), size, "staging {staging} read the wrong length");
        assert_eq!(
            read,
            expected_content(size),
            "staging {staging} changed the bytes"
        );
    }
}

/// Writing through the bridge produces a volume `fsck` and `mtools` accept.
///
/// The copy the bridge makes goes both ways, and a staging buffer reused
/// across batches is exactly where a stale tail would survive into the next
/// command — so the check that matters is an independent reader seeing the
/// bytes that were meant, not a read-back through the same code.
#[test]
fn writing_through_the_bridge_round_trips() {
    let image = mkfs_image("bridge-write.img", 272, 4);
    let payload = expected_content(1600 * 1024);

    let device = FromEmbeddedSdmmc::new(SdmmcImage::open_rw(&image).expect("open"));
    let mut volume = FileSystem::mount(device).expect("mount");

    volume.device().inner().reset();
    volume.write_file("/BIG.BIN", &payload).expect("write");
    let calls = volume.device().inner().calls();
    let moved = volume.device().inner().blocks_moved();
    volume.unmount().expect("unmount");

    assert_fsck_clean(&image);
    let out = scratch_dir().join("bridge-write-out.bin");
    let _ = std::fs::remove_file(&out);
    mcopy_out(&image, "/BIG.BIN", &out);
    assert_eq!(
        std::fs::read(&out).expect("read back"),
        payload,
        "mtools disagrees about what was written"
    );

    // The data alone is 3200 blocks. Allowing generous slack for the
    // allocation table and the directory, this still has to be nearer
    // twenty-five commands than three thousand, or the bridge is not doing
    // its job.
    assert!(
        calls < 60,
        "a 1.6 MB write took {calls} commands moving {moved} blocks"
    );
}

/// A device that cannot report its capacity, forwarding everything else.
///
/// The shape of a real SD adapter whose `num_blocks` is a stub returning an
/// "unsupported" error, because the driver never reads the card's CSD
/// register. `embedded-sdmmc`'s trait has no way to say "I do not know", so
/// an error is the only thing such a driver can return.
struct NoCapacity<D>(D);

impl<D: embedded_sdmmc::BlockDevice<Error = std::io::Error>> embedded_sdmmc::BlockDevice
    for NoCapacity<D>
{
    type Error = std::io::Error;

    fn read(
        &self,
        blocks: &mut [embedded_sdmmc::Block],
        start: embedded_sdmmc::BlockIdx,
    ) -> Result<(), Self::Error> {
        self.0.read(blocks, start)
    }

    fn write(
        &self,
        blocks: &[embedded_sdmmc::Block],
        start: embedded_sdmmc::BlockIdx,
    ) -> Result<(), Self::Error> {
        self.0.write(blocks, start)
    }

    fn num_blocks(&self) -> Result<embedded_sdmmc::BlockCount, Self::Error> {
        Err(std::io::Error::other("capacity readout is not implemented"))
    }
}

/// The whole stack a real board boots through: a driver that does not know
/// its own size, the bridge, and a volume inside a partition.
///
/// Every piece of this was tested and the combination was not, which is how
/// a board came back from an update unable to mount its own card. The bridge
/// forwarded the driver's `Unsupported` into a check that refused the mount,
/// and nothing on the host had ever put a reticent device and a filesystem
/// together.
///
/// It is one test standing in for a hardware round trip that costs a card
/// being pulled, so it is worth having even though every layer beneath it is
/// covered separately.
#[test]
fn a_reticent_driver_under_a_partition_still_mounts() {
    let device = FromEmbeddedSdmmc::new(NoCapacity(
        SdmmcImage::open_rw(fixture("fat32-mbr.img")).expect("open"),
    ));
    let mut volume = FileSystem::mount_at(device, 8192).expect("mount through the whole stack");

    let file = volume.open("/BIG.BIN").expect("open");
    assert_eq!(
        volume.read_all(&file).expect("read"),
        expected_content(1600 * 1024)
    );
}

/// A staging buffer of no blocks is refused at construction.
#[test]
#[should_panic(expected = "at least one block")]
fn a_zero_staging_buffer_is_refused() {
    let _ = FromEmbeddedSdmmc::with_blocks(
        SdmmcImage::open_rw(fixture("fat32-4k.img")).expect("open"),
        0,
    );
}
