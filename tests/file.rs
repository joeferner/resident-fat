//! Reading files, and what those reads cost.
//!
//! The device-call counts here are the crate's central claim stated as
//! numbers. Content correctness is necessary but not sufficient — an
//! implementation fetching one block at a time would pass every byte
//! comparison in this file while doing three thousand times the work.

mod support;

use resident_fat::blockdev::BLOCK_SIZE;
use resident_fat::{Error, FatError, FileSystem};
use support::*;

/// Reading a whole contiguous file costs exactly one device call, and
/// returns exactly the file's length.
///
/// 1.6 MB is 3200 blocks. The two assertions are independent: the call
/// count says the transfer was not split, and the length says the slack in
/// the last cluster was not handed back as content.
#[test]
fn a_whole_contiguous_file_is_one_call() {
    let expected = layout("fat32-4k.img");
    let size = expected.file("/BIG.BIN").size as usize;

    let device = Recorder::new(FileImage::open(fixture("fat32-4k.img")).expect("open"));
    let mut volume = FileSystem::mount(device).expect("mount");
    let file = volume.open("/BIG.BIN").expect("open BIG.BIN");
    assert_eq!(file.len() as usize, size);

    volume.device_mut().reset();
    let data = volume.read_all(&file).expect("read");

    let reads = volume.device_mut().reads();
    assert_eq!(reads.len(), 1, "expected one transfer, got {reads:?}");
    assert_eq!(
        data.len(),
        size,
        "read_all should return exactly the file's length"
    );
    assert_eq!(data, expected_content(size), "content differs");
}

/// A fragmented file costs one call per run, and its bytes are still
/// correct across the seams.
#[test]
fn a_fragmented_file_is_one_call_per_run() {
    let expected = layout("fat32-frag.img");
    let frag = expected.file("/FRAG.BIN");

    let device = Recorder::new(FileImage::open(fixture("fat32-frag.img")).expect("open"));
    let mut volume = FileSystem::mount(device).expect("mount");
    let file = volume.open("/FRAG.BIN").expect("open FRAG.BIN");

    volume.device_mut().reset();
    let data = volume.read_all(&file).expect("read");

    assert_eq!(
        volume.device_mut().reads().len(),
        frag.runs.len(),
        "expected one transfer per run"
    );
    assert_eq!(data, expected_content(frag.size as usize));
}

/// A read that starts and ends on block boundaries goes straight into the
/// caller's buffer: one call, no scratch.
#[test]
fn an_aligned_read_is_a_single_direct_transfer() {
    let device = Recorder::new(FileImage::open(fixture("fat32-4k.img")).expect("open"));
    let mut volume = FileSystem::mount(device).expect("mount");
    let file = volume.open("/BIG.BIN").expect("open");

    let offset = 4 * BLOCK_SIZE as u64;
    let mut buffer = vec![0u8; 16 * BLOCK_SIZE];

    volume.device_mut().reset();
    let read = volume.read_at(&file, offset, &mut buffer).expect("read");

    let reads = volume.device_mut().reads();
    assert_eq!(read, buffer.len());
    assert_eq!(
        reads.len(),
        1,
        "an aligned read should be one call: {reads:?}"
    );
    assert_eq!(
        reads[0].blocks, 16,
        "and should move exactly what was asked for"
    );

    let all = expected_content(offset as usize + buffer.len());
    assert_eq!(buffer, all[offset as usize..]);
}

/// An unaligned read costs three calls — the two ragged ends and the whole
/// blocks between them — and the middle one moves the bulk of the data.
///
/// The alternative implementation, reading every touched block into a
/// temporary and copying out, would be one call here. It would also need a
/// temporary as large as the read and would copy every byte twice, which
/// for a megabyte-scale read is the worse trade. Asserting the shape keeps
/// that decision from being undone by accident.
#[test]
fn an_unaligned_read_uses_scratch_only_at_the_ends() {
    let device = Recorder::new(FileImage::open(fixture("fat32-4k.img")).expect("open"));
    let mut volume = FileSystem::mount(device).expect("mount");
    let file = volume.open("/BIG.BIN").expect("open");

    let offset = 100u64; // inside the first block
    let mut buffer = vec![0u8; 5 * BLOCK_SIZE + 37];

    volume.device_mut().reset();
    volume.read_at(&file, offset, &mut buffer).expect("read");

    let reads = volume.device_mut().reads();
    assert_eq!(
        reads.len(),
        3,
        "expected leading, middle, trailing: {reads:?}"
    );
    assert_eq!(reads[0].blocks, 1, "the leading scratch read is one block");
    assert_eq!(reads[2].blocks, 1, "the trailing scratch read is one block");
    assert!(
        reads[1].blocks >= 4,
        "the middle should carry the bulk of it: {reads:?}"
    );

    let all = expected_content(offset as usize + buffer.len());
    assert_eq!(buffer, all[offset as usize..], "unaligned content differs");
}

/// A read entirely inside one block is a single call.
#[test]
fn a_read_within_one_block_is_one_call() {
    let device = Recorder::new(FileImage::open(fixture("fat32-4k.img")).expect("open"));
    let mut volume = FileSystem::mount(device).expect("mount");
    let file = volume.open("/BIG.BIN").expect("open");

    let mut buffer = [0u8; 20];
    volume.device_mut().reset();
    volume.read_at(&file, 30, &mut buffer).expect("read");

    assert_eq!(volume.device_mut().reads().len(), 1);
    assert_eq!(&buffer, &expected_content(50)[30..50]);
}

/// A device that cannot move a long run gets its limit respected, and the
/// data still arrives.
///
/// `Capped` panics rather than erroring on an over-long transfer, so this
/// fails loudly if the limit is ignored instead of quietly working because
/// the test device was forgiving.
#[test]
fn a_device_transfer_limit_is_respected() {
    let expected = layout("fat32-4k.img");
    let size = expected.file("/BIG.BIN").size as usize;

    let device = Recorder::new(Capped::new(
        FileImage::open(fixture("fat32-4k.img")).expect("open"),
        8,
    ));
    let mut volume = FileSystem::mount(device).expect("mount");
    let file = volume.open("/BIG.BIN").expect("open");

    volume.device_mut().reset();
    let data = volume.read_all(&file).expect("read");

    let reads = volume.device_mut().reads();
    assert!(
        reads.iter().all(|r| r.blocks <= 8),
        "a transfer exceeded the device's limit"
    );
    let blocks = size.div_ceil(BLOCK_SIZE);
    assert_eq!(
        reads.len(),
        blocks.div_ceil(8),
        "the read should be split into the fewest calls the limit allows"
    );
    assert_eq!(data, expected_content(size), "capped content differs");
}

/// Reads at and past the end of a file behave, rather than erroring or
/// reading slack.
#[test]
fn reads_stop_at_the_end_of_the_file() {
    let mut volume =
        FileSystem::mount(FileImage::open(fixture("fat32-4k.img")).expect("open")).expect("mount");
    let file = volume.open("/HELLO.TXT").expect("open");
    let size = file.len() as u64;

    let mut buffer = [0u8; 512];
    // Past the end.
    assert_eq!(volume.read_at(&file, size, &mut buffer).expect("read"), 0);
    assert_eq!(
        volume.read_at(&file, size + 99, &mut buffer).expect("read"),
        0
    );

    // Straddling the end: the read stops there rather than returning the
    // rest of the cluster, which is slack and not part of the file.
    let read = volume.read_at(&file, size - 3, &mut buffer).expect("read");
    assert_eq!(read, 3);

    let whole = volume.read_all(&file).expect("read all");
    assert_eq!(whole.len(), size as usize);
    assert_eq!(&buffer[..3], &whole[whole.len() - 3..]);
}

/// Every byte of a file comes back the same however the read is chopped up.
///
/// Cluster and block boundaries are where a run-based reader goes wrong, so
/// the offsets and lengths here are chosen to land on and around both.
#[test]
fn reads_agree_however_they_are_split() {
    let expected = layout("fat32-frag.img");
    let frag = expected.file("/FRAG.BIN");
    let cluster = expected.cluster_bytes as u64;

    let mut volume = FileSystem::mount(FileImage::open(fixture("fat32-frag.img")).expect("open"))
        .expect("mount");
    let file = volume.open("/FRAG.BIN").expect("open");
    let whole = expected_content(frag.size as usize);

    let offsets = [
        0,
        1,
        BLOCK_SIZE as u64 - 1,
        BLOCK_SIZE as u64,
        BLOCK_SIZE as u64 + 1,
        cluster - 1,
        cluster,
        cluster + 1,
        cluster * 2 + 17,
        frag.size - 1,
    ];
    let lengths = [1usize, 7, BLOCK_SIZE - 1, BLOCK_SIZE, BLOCK_SIZE + 1, 5000];

    for offset in offsets {
        for length in lengths {
            let mut buffer = vec![0u8; length];
            let read = volume.read_at(&file, offset, &mut buffer).expect("read");

            let end = (offset as usize + length).min(whole.len());
            let wanted = &whole[offset as usize..end];
            assert_eq!(
                read,
                wanted.len(),
                "offset {offset}, length {length}: short"
            );
            assert_eq!(
                &buffer[..read],
                wanted,
                "offset {offset}, length {length}: content differs"
            );
        }
    }
}

/// Opening a directory as a file says so.
#[test]
fn opening_a_directory_as_a_file_is_refused() {
    let mut volume =
        FileSystem::mount(FileImage::open(fixture("fat32-4k.img")).expect("open")).expect("mount");

    match volume.open("/ROMS") {
        Err(Error::IsADirectory { name }) => assert_eq!(name, "ROMS"),
        other => panic!("expected IsADirectory, got {other:?}"),
    }
    match volume.open("/NOPE.BIN") {
        Err(Error::NotFound { name }) => assert_eq!(name, "NOPE.BIN"),
        other => panic!("expected NotFound, got {other:?}"),
    }
}

/// Files open by long name, in a directory reached by path.
#[test]
fn files_open_by_long_name() {
    let mut volume =
        FileSystem::mount(FileImage::open(fixture("fat32-4k.img")).expect("open")).expect("mount");

    let by_long = volume
        .open("/ROMS/Game Title 007 (USA, Europe).nes")
        .expect("open by long name");
    let data = volume.read_all(&by_long).expect("read");
    assert_eq!(data, b"rom 007\n", "the wrong file was opened");
}

/// A file whose entry claims more bytes than its chain holds is reported
/// rather than silently truncated.
#[test]
fn a_length_longer_than_the_chain_is_reported() {
    let mut image = MemoryImage::load(fixture("fat32-frag.img")).expect("load");

    // Inflate FRAG.BIN's recorded size without giving it more clusters.
    let root = root_directory_offset(image.bytes());
    let at = find_entry(image.bytes(), root, b"FRAG    BIN").expect("FRAG.BIN in the root");
    image.bytes_mut()[at + 0x1C..at + 0x20].copy_from_slice(&(1024u32 * 1024).to_le_bytes());

    let mut volume = FileSystem::mount(image).expect("mount");
    let file = volume.open("/FRAG.BIN").expect("open");
    match volume.read_all(&file) {
        Err(Error::Fat(FatError::ChainShorterThanFile { size, .. })) => {
            assert_eq!(size, 1024 * 1024)
        }
        other => panic!(
            "expected ChainShorterThanFile, got {:?}",
            other.map(|d| d.len())
        ),
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Byte offset of the root directory's first entry, read straight from the
/// boot sector rather than through the crate.
fn root_directory_offset(boot: &[u8]) -> usize {
    let bytes_per_sector = u16::from_le_bytes([boot[0x0B], boot[0x0C]]) as usize;
    let sectors_per_cluster = boot[0x0D] as usize;
    let reserved = u16::from_le_bytes([boot[0x0E], boot[0x0F]]) as usize;
    let fats = boot[0x10] as usize;
    let sectors_per_fat =
        u32::from_le_bytes([boot[0x24], boot[0x25], boot[0x26], boot[0x27]]) as usize;
    let root_cluster =
        u32::from_le_bytes([boot[0x2C], boot[0x2D], boot[0x2E], boot[0x2F]]) as usize;

    (reserved + fats * sectors_per_fat + (root_cluster - 2) * sectors_per_cluster)
        * bytes_per_sector
}

/// Offset of the directory entry with this 8.3 name.
fn find_entry(bytes: &[u8], root: usize, name: &[u8; 11]) -> Option<usize> {
    (0..)
        .map(|n| root + n * 32)
        .take_while(|&at| at + 32 <= bytes.len() && bytes[at] != 0x00)
        .find(|&at| &bytes[at..at + 11] == name.as_slice())
}
