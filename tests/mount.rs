//! Mounting, the boot sector, and the resident allocation table.
//!
//! The chain assertions here compare against `manifest.json`, which
//! `scripts/fatmap.py` produced by reading the images independently of this
//! crate. That is the point — a test that worked out the expected clusters
//! by calling the code under test would agree with its bugs.

mod support;

use resident_fat::blockdev::BLOCK_SIZE;
use resident_fat::boot::{BootSector, FsInfo};
use resident_fat::{BootError, Error, FatError, FileSystem, Format, Geometry, Run};
use support::*;

/// The three cluster sizes, since cluster-size arithmetic is where a
/// filesystem's off-by-one errors live.
const FAT32_IMAGES: [&str; 4] = [
    "fat32-4k.img",
    "fat32-16k.img",
    "fat32-32k.img",
    "fat32-frag.img",
];

/// Every FAT32 fixture mounts, and reports the geometry the image actually
/// has.
#[test]
fn mounts_every_fat32_fixture() {
    for name in FAT32_IMAGES {
        let expected = layout(name);
        let volume = FileSystem::mount(FileImage::open(fixture(name)).expect("open"))
            .unwrap_or_else(|e| panic!("{name} did not mount: {e:?}"));

        let boot = volume.boot_sector();
        assert_eq!(
            boot.cluster_bytes(),
            expected.cluster_bytes,
            "{name}: cluster size"
        );
        assert_eq!(
            boot.root_cluster, expected.root_cluster,
            "{name}: root cluster"
        );
        assert_eq!(
            volume.fat().cluster_count(),
            expected.cluster_count,
            "{name}: cluster count"
        );
        assert!(
            !volume.is_dirty(),
            "{name}: a freshly built volume is not dirty"
        );
    }
}

/// Chains walked from the resident table match what an independent reader
/// found on the same images.
#[test]
fn cluster_chains_match_the_manifest() {
    for name in FAT32_IMAGES {
        let expected = layout(name);
        let volume =
            FileSystem::mount(FileImage::open(fixture(name)).expect("open")).expect("mount");

        // A manifest that parsed to nothing would make this loop pass
        // without checking anything, which is the failure mode a test like
        // this actually has.
        assert!(
            expected.files.len() > 100,
            "{name}: only {} files in the manifest, so this proves little",
            expected.files.len()
        );

        // Every file, not a sample: the ROM directory alone is 300 chains,
        // and checking all of them costs microseconds now that the table is
        // an array.
        for file in &expected.files {
            let runs = volume
                .runs(file.first_cluster())
                .unwrap_or_else(|e| panic!("{name}: {} failed to walk: {e:?}", file.path));

            let found: Vec<(u32, u32)> = runs
                .iter()
                .map(|run| (run.start_cluster, run.clusters))
                .collect();
            assert_eq!(
                found, file.runs,
                "{name}: {} has a different chain",
                file.path
            );
        }
    }
}

/// The deliberately fragmented file really is in several runs, and the
/// contiguous one really is in a single run.
///
/// The property the whole "one call per run" argument rests on. If this
/// ever reports one
/// run for `FRAG.BIN`, the fixture stopped being fragmented and the later
/// "one call per run" assertions became vacuous.
#[test]
fn contiguity_is_what_the_fixtures_promise() {
    let big = layout("fat32-4k.img");
    let volume =
        FileSystem::mount(FileImage::open(fixture("fat32-4k.img")).expect("open")).expect("mount");
    let runs = volume
        .runs(big.file("/BIG.BIN").first_cluster())
        .expect("walk");
    assert_eq!(
        runs.len(),
        1,
        "BIG.BIN should occupy one contiguous run: {runs:?}"
    );

    let frag = layout("fat32-frag.img");
    let volume = FileSystem::mount(FileImage::open(fixture("fat32-frag.img")).expect("open"))
        .expect("mount");
    let runs = volume
        .runs(frag.file("/FRAG.BIN").first_cluster())
        .expect("walk");
    assert!(
        runs.len() > 1,
        "FRAG.BIN should be fragmented, so the run assertions later mean something: {runs:?}"
    );
}

/// A file's bytes come back correct, and a contiguous one costs exactly one
/// device call.
///
/// The crate's central claim, measured rather than asserted. 1.6 MB is 3200
/// blocks; an implementation that fetched a block at a time would report
/// 3200 here and pass every content check while doing so.
#[test]
fn a_contiguous_file_reads_in_one_device_call() {
    let expected = layout("fat32-4k.img");
    let big = expected.file("/BIG.BIN");

    let device = Recorder::new(FileImage::open(fixture("fat32-4k.img")).expect("open"));
    let mut volume = FileSystem::mount(device).expect("mount");

    let runs = volume.runs(big.first_cluster()).expect("walk");
    assert_eq!(runs.len(), 1);

    let mut data = vec![0u8; runs[0].bytes(expected.cluster_bytes) as usize];
    volume.device_mut().reset();
    volume.read_run(&runs[0], &mut data).expect("read the run");

    let reads = volume.device_mut().reads();
    assert_eq!(
        reads.len(),
        1,
        "a contiguous file should cost one call: {reads:?}"
    );
    assert_eq!(
        reads[0].blocks,
        data.len() as u64 / BLOCK_SIZE as u64,
        "the single call should move the whole file"
    );

    data.truncate(big.size as usize);
    assert_eq!(
        data,
        expected_content(big.size as usize),
        "BIG.BIN read back wrong"
    );
}

/// A fragmented file costs one call per run — not one per block, and not
/// one for the whole thing.
#[test]
fn a_fragmented_file_costs_one_call_per_run() {
    let expected = layout("fat32-frag.img");
    let frag = expected.file("/FRAG.BIN");

    let device = Recorder::new(FileImage::open(fixture("fat32-frag.img")).expect("open"));
    let mut volume = FileSystem::mount(device).expect("mount");

    let runs = volume.runs(frag.first_cluster()).expect("walk");
    // Against the manifest, not against `runs`: comparing the call count to
    // this crate's own idea of the run count would stay true if run
    // detection broke, since both numbers would move together.
    assert_eq!(
        runs.len(),
        frag.runs.len(),
        "run count disagrees with the manifest"
    );

    volume.device_mut().reset();
    let data = volume
        .read_chain(frag.first_cluster())
        .expect("read the chain");

    let reads = volume.device_mut().reads();
    assert_eq!(
        reads.len(),
        frag.runs.len(),
        "expected one call per run, got {} calls for {} runs",
        reads.len(),
        frag.runs.len()
    );

    let content = &data[..frag.size as usize];
    assert_eq!(
        content,
        expected_content(frag.size as usize),
        "FRAG.BIN read back wrong"
    );
}

/// Loading the table is a handful of large reads, not thousands of small
/// ones.
#[test]
fn the_table_loads_in_a_few_large_reads() {
    let device = Recorder::new(FileImage::open(fixture("fat32-32k.img")).expect("open"));
    let volume = FileSystem::mount(device).expect("mount");

    // Mount reads the boot sector, the hints sector, and the table.
    let reads = volume.device().reads();
    assert!(
        reads.len() <= 8,
        "mount should be a handful of reads, not {}: {reads:?}",
        reads.len()
    );

    let table_blocks: u64 = reads
        .iter()
        .filter(|r| r.blocks > 1)
        .map(|r| r.blocks)
        .sum();
    assert!(
        table_blocks > 0,
        "the table should be read in multi-block transfers: {reads:?}"
    );
}

/// FAT12 and FAT16 are recognised and declined by name.
///
/// The trap this guards is specific: on those formats the 32-bit
/// sectors-per-table field does not exist — the offset holds boot code — so
/// an implementation that reads it first sees an enormous plausible-looking
/// number and mounts garbage.
#[test]
fn other_fat_formats_are_refused_by_name() {
    for (name, expected) in [("fat12.img", Format::Fat12), ("fat16.img", Format::Fat16)] {
        let result = FileSystem::mount(FileImage::open(fixture(name)).expect("open"));
        match result {
            Err(Error::Boot(BootError::UnsupportedFormat(format))) => {
                assert_eq!(format, expected, "{name} was classified wrongly")
            }
            Err(other) => panic!("{name}: expected UnsupportedFormat, got {other:?}"),
            Ok(_) => panic!("{name}: mounted a volume this crate does not support"),
        }
    }
}

/// A FAT12/FAT16-shaped boot sector holding more clusters than either format
/// can address is refused, and refused without panicking.
///
/// Two things are being pinned. The first is that it must not come back as
/// `UnsupportedFormat(Fat16)`: the structure says FAT12 or FAT16 and the
/// arithmetic says neither, so the volume is corrupt rather than merely
/// unsupported, and those call for different responses — reformatting fixes
/// only one of them.
///
/// The second is that it must not abort. Classification used to assert this
/// case away on the belief that it could not arise; it arises on any corrupt
/// or hostile boot sector, and a debug build that panicked here would take
/// down a bare-metal target — which has no unwinder — over what a card
/// claimed. That is the failure this test exists to catch, so it has to run
/// with debug assertions on, which `cargo test` gives it.
#[test]
fn a_small_format_with_an_impossible_cluster_count_is_refused_not_asserted() {
    let mut image = MemoryImage::load(fixture("fat16.img")).expect("load");

    // Nonzero, so the structure still classifies as FAT12/FAT16 and the
    // 32-bit sectors-per-table field is never consulted.
    image.bytes_mut()[0x16..0x18].copy_from_slice(&1u16.to_le_bytes());
    // One sector per cluster and a huge volume: far more clusters than the
    // 65524 FAT16 tops out at.
    image.bytes_mut()[0x0D] = 1;
    image.bytes_mut()[0x13..0x15].copy_from_slice(&0u16.to_le_bytes());
    image.bytes_mut()[0x20..0x24].copy_from_slice(&0x4000_0000u32.to_le_bytes());

    match FileSystem::mount(image) {
        Err(Error::Boot(BootError::BadGeometry(Geometry::ClusterCount(clusters)))) => {
            assert!(
                clusters > 65524,
                "the refusal should name a count no small format can hold, got {clusters}"
            );
        }
        other => panic!("expected BadGeometry(ClusterCount), got {other:?}"),
    }
}

/// A FAT32 volume with fewer clusters than the FAT12 threshold still mounts
/// as FAT32.
///
/// `fat32-frag.img` has about four thousand clusters, below the 4085 that
/// would make a volume FAT12 by cluster count — yet it is structurally
/// FAT32, and every other tool mounts it. Classifying by cluster count
/// would refuse it.
#[test]
fn a_small_volume_is_still_fat32_if_its_structure_says_so() {
    let expected = layout("fat32-frag.img");
    assert!(
        expected.cluster_count < 4085,
        "this test is pointless unless the fixture is below the FAT12 threshold"
    );
    FileSystem::mount(FileImage::open(fixture("fat32-frag.img")).expect("open"))
        .expect("a structurally FAT32 volume should mount however few clusters it has");
}

/// Something that is not a FAT volume is refused, rather than parsed into
/// nonsense.
#[test]
fn a_volume_without_a_boot_signature_is_refused() {
    let mut image = MemoryImage::load(fixture("fat32-frag.img")).expect("load");
    image.bytes_mut()[0x1FE] = 0;
    image.bytes_mut()[0x1FF] = 0;

    match FileSystem::mount(image) {
        Err(Error::Boot(BootError::NotFat)) => {}
        other => panic!("expected NotFat, got {other:?}"),
    }
}

/// A block shorter than a sector is refused, not indexed past the end.
///
/// `BootSector::parse` and `FsInfo::parse` both take a `&[u8]` and read
/// fixed offsets out of it, so a short slice used to reach a raw index panic
/// — guarded only by a `debug_assert!`, which is compiled out of exactly the
/// build that runs on hardware. A panic there aborts a bare-metal target,
/// which has no unwinder, so the length is checked in release too.
#[test]
fn a_short_block_is_refused_rather_than_indexed() {
    let whole = std::fs::read(fixture("fat32-frag.img")).expect("read");

    // Every truncation, including the empty slice and one byte short of a
    // whole sector, which is the case an off-by-one would let through.
    for len in [0usize, 1, 11, 0x1FE, BLOCK_SIZE - 1] {
        match BootSector::parse(&whole[..len]) {
            Err(BootError::ShortBlock { len: reported }) => assert_eq!(reported, len),
            other => panic!("a {len}-byte block should be refused, got {other:?}"),
        }
    }

    // And a whole one still parses, so the check is not simply refusing
    // everything.
    let boot = BootSector::parse(&whole[..BLOCK_SIZE]).expect("a whole sector parses");

    // The hints have no failure to report -- they are hints -- so a short
    // block gives the same nothing as a block with wrong signatures.
    assert_eq!(
        FsInfo::parse(&whole[..7], &boot),
        FsInfo::default(),
        "a short hints block should read as no hints"
    );
}

// ---------------------------------------------------------------------------
// Corruption: every one of these must fail, and none may hang
// ---------------------------------------------------------------------------

/// A chain that points back at itself errors instead of spinning.
///
/// The bound matters more here than in an implementation that reads the
/// table from the device: walking is a memory loop with no I/O to slow a
/// runaway down, so an unbounded walk would hang the caller instantly.
#[test]
fn a_cyclic_chain_errors_rather_than_hanging() {
    let layout = layout("fat32-frag.img");
    let start = layout.file("/FRAG.BIN").first_cluster();

    let mut image = MemoryImage::load(fixture("fat32-frag.img")).expect("load");
    let boot = BootSector::parse(&image.bytes()[..BLOCK_SIZE]).expect("parse");

    // Point the chain's first cluster back at itself.
    set_fat_entry(&mut image, &boot, start, start);

    let volume = FileSystem::mount(image).expect("mount");
    match volume.runs(start) {
        Err(Error::Fat(FatError::ChainLoop { start: reported })) => assert_eq!(reported, start),
        other => panic!("expected ChainLoop, got {other:?}"),
    }
}

/// A chain entry pointing past the end of the volume is rejected.
#[test]
fn a_cluster_past_the_end_is_rejected() {
    let layout = layout("fat32-frag.img");
    let start = layout.file("/FRAG.BIN").first_cluster();

    let mut image = MemoryImage::load(fixture("fat32-frag.img")).expect("load");
    let boot = BootSector::parse(&image.bytes()[..BLOCK_SIZE]).expect("parse");
    let past_the_end = boot.cluster_count + 100;
    set_fat_entry(&mut image, &boot, start, past_the_end);

    let volume = FileSystem::mount(image).expect("mount");
    match volume.runs(start) {
        Err(Error::Fat(FatError::BadCluster { cluster })) => assert_eq!(cluster, past_the_end),
        other => panic!("expected BadCluster, got {other:?}"),
    }
}

/// A run that starts inside the volume and ends outside it is refused.
///
/// `Run`'s fields are public, so a caller can assemble one rather than take
/// it from `runs()` — and validating only the first cluster would let the
/// transfer continue off the end of the data area into whatever follows. On a
/// partitioned card that is another filesystem, so the device would return
/// its bytes rather than an error.
#[test]
fn a_run_ending_past_the_volume_is_refused() {
    let mut volume =
        FileSystem::mount(FileImage::open(fixture("fat32-4k.img")).expect("open")).expect("mount");
    let cluster_bytes = volume.boot_sector().cluster_bytes();
    let count = volume.fat().cluster_count();
    // Cluster numbering starts at 2, so this is the last one the volume has.
    let last_valid = count + 1;

    for (what, clusters) in [
        ("starting at the last cluster and running past it", 8u32),
        ("a length that would wrap the cluster number", u32::MAX),
    ] {
        let run = Run {
            file_cluster: 0,
            start_cluster: last_valid,
            clusters,
        };
        // Deliberately empty. The run is rejected before the buffer length
        // is looked at, which is what stops a caller having to allocate the
        // terabytes `u32::MAX` clusters would need just to be told the run
        // is impossible.
        match volume.read_run(&run, &mut []) {
            Err(Error::Fat(FatError::BadCluster { .. })) => {}
            other => panic!("{what}: expected BadCluster, got {other:?}"),
        }
    }

    // The last cluster on its own is still readable, so the bound is the
    // volume's end and not one short of it.
    let run = Run {
        file_cluster: 0,
        start_cluster: last_valid,
        clusters: 1,
    };
    let mut buffer = vec![0u8; run.bytes(cluster_bytes) as usize];
    volume
        .read_run(&run, &mut buffer)
        .expect("the last cluster of the volume is readable");
}

/// A free entry inside a chain is corruption, not the end of the file.
#[test]
fn a_free_cluster_inside_a_chain_is_corruption() {
    let layout = layout("fat32-frag.img");
    let start = layout.file("/FRAG.BIN").first_cluster();

    let mut image = MemoryImage::load(fixture("fat32-frag.img")).expect("load");
    let boot = BootSector::parse(&image.bytes()[..BLOCK_SIZE]).expect("parse");
    set_fat_entry(&mut image, &boot, start, 0);

    let volume = FileSystem::mount(image).expect("mount");
    match volume.runs(start) {
        Err(Error::Fat(FatError::FreeClusterInChain { start: s, cluster })) => {
            assert_eq!(s, start);
            assert_eq!(cluster, start);
        }
        other => panic!("expected FreeClusterInChain, got {other:?}"),
    }
}

/// A boot sector claiming more clusters than its table can address is
/// clamped to the table, not believed.
///
/// Believing the claim is how a later read runs off the end of the resident
/// array, somewhere much less obviously connected to the cause.
#[test]
fn a_cluster_count_beyond_the_table_is_clamped() {
    let mut image = MemoryImage::load(fixture("fat32-frag.img")).expect("load");
    let honest = BootSector::parse(&image.bytes()[..BLOCK_SIZE]).expect("parse");

    // Multiply the volume's sector count without growing the table.
    let inflated = honest.total_sectors * 8;
    image.bytes_mut()[0x13..0x15].copy_from_slice(&0u16.to_le_bytes());
    image.bytes_mut()[0x20..0x24].copy_from_slice(&inflated.to_le_bytes());

    let boot = BootSector::parse(&image.bytes()[..BLOCK_SIZE]).expect("parse");
    let addressable = boot.sectors_per_fat * (u32::from(boot.bytes_per_sector) / 4) - 2;
    assert_eq!(
        boot.cluster_count, addressable,
        "the cluster count should be clamped to what the table addresses"
    );
    assert!(
        boot.cluster_count < (inflated - boot.data_start) / u32::from(boot.sectors_per_cluster),
        "the clamp should have bitten"
    );
}

/// A volume claiming to be larger than the device it sits on is refused at
/// mount.
#[test]
fn a_volume_larger_than_its_device_is_refused() {
    let mut image = MemoryImage::load(fixture("fat32-frag.img")).expect("load");
    image.bytes_mut()[0x13..0x15].copy_from_slice(&0u16.to_le_bytes());
    image.bytes_mut()[0x20..0x24].copy_from_slice(&0x00FF_FFFFu32.to_le_bytes());

    match FileSystem::mount(image) {
        Err(Error::Boot(BootError::BadGeometry(Geometry::VolumeTooLarge { .. }))) => {}
        other => panic!("expected VolumeTooLarge, got {other:?}"),
    }
}

/// A device that cannot report its size is mounted anyway.
///
/// Plenty of real drivers do not know: an SD card keeps its capacity in the
/// CSD register, and a driver written to move blocks has no other reason to
/// go and read it. This crate used to refuse to mount through such a driver
/// — the volume was fine, the boot sector was fine, and the filesystem
/// declined because the layer underneath was reticent about something it
/// only wanted for a sanity check.
///
/// Nothing about the volume is treated differently afterwards, which is the
/// part worth asserting: the size check is skipped and everything else
/// happens as usual.
#[test]
fn a_device_that_cannot_report_its_size_still_mounts() {
    let expected = layout("fat32-4k.img");
    let mut volume = FileSystem::mount(Reticent(
        FileImage::open(fixture("fat32-4k.img")).expect("open"),
    ))
    .expect("a volume should not be refused for its device's reticence");

    assert_eq!(volume.fat().cluster_count(), expected.cluster_count);
    let big = expected.file("/BIG.BIN");
    let file = volume.open("/BIG.BIN").expect("open");
    assert_eq!(file.len() as u64, big.size);
    assert_eq!(
        volume.read_all(&file).expect("read"),
        expected_content(big.size as usize)
    );
    let root = volume.root_dir().expect("root");
    for name in ["BIG.BIN", "HELLO.TXT", "ROMS", "EDGE"] {
        assert!(root.get(name).is_some(), "{name} is missing from the root");
    }
}

/// The size check still fires when the device *does* know.
///
/// Allowing "I do not know" must not turn the check off for everyone. The
/// two tests are kept adjacent because that is the regression: making the
/// oversized case pass by never checking would satisfy one of them and
/// nothing would notice.
#[test]
fn an_unknown_size_does_not_disable_the_check_for_known_ones() {
    let mut image = MemoryImage::load(fixture("fat32-frag.img")).expect("load");
    image.bytes_mut()[0x13..0x15].copy_from_slice(&0u16.to_le_bytes());
    image.bytes_mut()[0x20..0x24].copy_from_slice(&0x00FF_FFFFu32.to_le_bytes());

    // The same doctored volume: refused on a device that knows its size,
    // accepted on one that does not.
    let mut bytes = image.bytes().to_vec();
    match FileSystem::mount(image) {
        Err(Error::Boot(BootError::BadGeometry(Geometry::VolumeTooLarge { .. }))) => {}
        other => panic!("expected VolumeTooLarge, got {other:?}"),
    }

    let reticent = Reticent(MemoryImage::from_bytes(core::mem::take(&mut bytes)));
    assert!(
        FileSystem::mount(reticent).is_ok(),
        "with no size to check against there is nothing to refuse"
    );
}

/// Boot sector fields that make no sense are refused, each naming itself.
#[test]
fn nonsensical_geometry_is_refused_field_by_field() {
    /// One field, the nonsense to write into it, and what should come back.
    struct Case {
        what: &'static str,
        offset: usize,
        bytes: &'static [u8],
        expected: fn(&Geometry) -> bool,
    }

    let cases = [
        Case {
            what: "sector size",
            offset: 0x0B,
            bytes: &[0x00, 0x03],
            expected: |g| matches!(g, Geometry::SectorSize(768)),
        },
        Case {
            what: "sectors per cluster",
            offset: 0x0D,
            bytes: &[3],
            expected: |g| matches!(g, Geometry::SectorsPerCluster(3)),
        },
        Case {
            what: "reserved sectors",
            offset: 0x0E,
            bytes: &[0, 0],
            expected: |g| matches!(g, Geometry::ReservedSectors(0)),
        },
        Case {
            what: "fat count",
            offset: 0x10,
            bytes: &[0],
            expected: |g| matches!(g, Geometry::FatCount(0)),
        },
    ];

    for case in cases {
        let mut image = MemoryImage::load(fixture("fat32-frag.img")).expect("load");
        image.bytes_mut()[case.offset..case.offset + case.bytes.len()].copy_from_slice(case.bytes);

        match FileSystem::mount(image) {
            Err(Error::Boot(BootError::BadGeometry(problem))) => assert!(
                (case.expected)(&problem),
                "{}: refused, but reported {problem:?}",
                case.what
            ),
            other => panic!("{}: expected BadGeometry, got {other:?}", case.what),
        }
    }
}

/// The sector sizes FAT allows and this crate does not are refused too.
///
/// Kept apart from the table above because these are the dangerous ones,
/// and a check that only rejects nonsense would miss all three. 768 is not
/// a power of two, so *any* sanity check catches it; 1024, 2048 and 4096
/// are legal FAT sector sizes that a plausibility check waves through.
///
/// What makes accepting one unsafe is that a sector number and a device
/// block number are the same number only at 512. Every offset this crate
/// computes -- `cluster_sector`, `fat_start`, `data_start`, the dirty
/// sectors the table hands to the flush -- is a sector number that reaches
/// the device as a block number. At 4096 the volume mounts, reports a
/// believable cluster count, reads whatever lies at an eighth of the
/// intended offset, and on the first flush writes eight times the region it
/// meant to. So the refusal has to be at mount, where there is still
/// something to refuse.
#[test]
fn a_sector_size_other_than_512_is_refused() {
    for size in [1024u16, 2048, 4096] {
        let mut image = MemoryImage::load(fixture("fat32-frag.img")).expect("load");
        image.bytes_mut()[0x0B..0x0D].copy_from_slice(&size.to_le_bytes());

        match FileSystem::mount(image) {
            Err(Error::Boot(BootError::BadGeometry(Geometry::SectorSize(reported)))) => {
                assert_eq!(reported, size, "the refusal should name the size it found");
            }
            other => panic!("{size}-byte sectors should be refused, got {other:?}"),
        }
    }
}

/// Writes a raw entry into every copy of the table in a loaded image.
///
/// Harness code that deliberately does not use the crate: a test that
/// corrupted the table by calling the code under test would be limited to
/// corruption that code considers possible.
fn set_fat_entry(image: &mut MemoryImage, boot: &BootSector, cluster: u32, value: u32) {
    let sector_size = usize::from(boot.bytes_per_sector);
    for copy in 0..u32::from(boot.fat_count) {
        let fat = (boot.fat_start + copy * boot.sectors_per_fat) as usize * sector_size;
        let at = fat + cluster as usize * 4;
        image.bytes_mut()[at..at + 4].copy_from_slice(&value.to_le_bytes());
    }
}

/// A run read into a buffer of the wrong size is an error, not a panic.
///
/// `read_run` deliberately offers no way to read a run piecemeal, so the
/// length is a contract — but a bare-metal target has no unwinder, and a
/// filesystem that aborted the machine over a mis-sized buffer would be a
/// worse neighbour than one that says so and lets the caller decide.
#[test]
fn a_mis_sized_buffer_is_reported_rather_than_asserted() {
    let expected = layout("fat32-4k.img");
    let mut volume =
        FileSystem::mount(FileImage::open(fixture("fat32-4k.img")).expect("open")).expect("mount");

    let runs = volume
        .runs(expected.file("/BIG.BIN").first_cluster())
        .expect("walk");
    let exact = runs[0].bytes(expected.cluster_bytes) as usize;

    for (what, len) in [("short", exact - 1), ("long", exact + 1)] {
        let mut wrong = vec![0u8; len];
        match volume.read_run(&runs[0], &mut wrong) {
            Err(Error::BufferLength { expected, actual }) => {
                assert_eq!(
                    expected as usize, exact,
                    "{what}: should name the run's size"
                );
                assert_eq!(actual, len, "{what}: should name what was supplied");
            }
            other => panic!("{what}: expected BufferLength, got {other:?}"),
        }
    }

    // And the exact length still works, so the check is not simply refusing
    // everything.
    let mut right = vec![0u8; exact];
    volume.read_run(&runs[0], &mut right).expect("exact length");
}
