//! Reading directories, and long names.
//!
//! `mdir` is the oracle throughout. It prints both the long name and the
//! 8.3 alias for every entry, so it says what this crate has to agree with
//! from a codebase that is not ours — which is the only way a long-name
//! implementation can be checked without restating its own logic in the
//! test.

mod support;

use std::collections::BTreeMap;

use resident_fat::dir::{Directory, ENTRY_SIZE};
use resident_fat::{Error, FileSystem};
use support::*;

/// Every name in `/ROMS` matches `mdir`, long and short.
///
/// Three hundred entries, each needing several long-name slots, is the
/// workload the ROM picker represents and the one that made the old
/// per-block implementation slow.
#[test]
fn three_hundred_long_names_match_mdir() {
    let image = fixture("fat32-4k.img");
    let expected = names_by_short(&mdir_entries(&image, "/ROMS"));
    assert_eq!(expected.len(), 300, "the fixture should hold 300 ROM names");

    let mut volume = FileSystem::mount(FileImage::open(&image).expect("open")).expect("mount");
    let found = names_by_short_of(volume.open_dir("/ROMS").expect("open /ROMS"));

    assert_eq!(found, expected, "the name list disagrees with mdir");
}

/// The awkward short-name cases, each of which exercises a different rule.
///
/// `mdir` is again the authority. The interesting ones are the two with no
/// long name at all: a name that already fits 8.3 needs no long-name
/// slots, and an all-lower-case one needs none either because the entry
/// carries case flags instead.
#[test]
fn short_name_edge_cases_match_mdir() {
    let image = fixture("fat32-4k.img");
    let expected = names_by_short(&mdir_entries(&image, "/EDGE"));

    let mut volume = FileSystem::mount(FileImage::open(&image).expect("open")).expect("mount");
    let found = names_by_short_of(volume.open_dir("/EDGE").expect("open /EDGE"));
    assert_eq!(found, expected, "the /EDGE listing disagrees with mdir");

    // Spot-check the two that must have *no* long name, since that is a
    // property a listing comparison would also satisfy by inventing one.
    assert_eq!(
        expected.get("UPPER.TXT"),
        Some(&None),
        "mdir's view changed"
    );
    assert_eq!(
        found.get("UPPER.TXT"),
        Some(&None),
        "a plain 8.3 name needs no long name"
    );
    assert_eq!(
        found.get("LOWER.TXT"),
        Some(&None),
        "a lower-case 8.3 name needs no long name"
    );
}

/// Lower-case 8.3 names are shown lower case, via the entry's case flags
/// rather than a long name.
#[test]
fn case_flags_are_applied_to_short_names() {
    let mut volume =
        FileSystem::mount(FileImage::open(fixture("fat32-4k.img")).expect("open")).expect("mount");
    let directory = volume.open_dir("/EDGE").expect("open /EDGE");

    let entry = directory
        .get("lower.txt")
        .expect("lower.txt should be found");
    assert_eq!(entry.long_name(), None, "it needs no long name");
    assert_eq!(
        entry.name(),
        "lower.txt",
        "the stored case should be applied"
    );
}

/// A file is findable by its long name and by its 8.3 alias, in any case.
#[test]
fn lookup_accepts_either_name_in_any_case() {
    let mut volume =
        FileSystem::mount(FileImage::open(fixture("fat32-4k.img")).expect("open")).expect("mount");
    let directory = volume.open_dir("/ROMS").expect("open /ROMS");

    let long = "Game Title 042 (USA, Europe).nes";
    let by_long = directory.get(long).expect("findable by long name");
    let by_lower = directory
        .get(&long.to_lowercase())
        .expect("findable case-insensitively");
    let by_short = directory
        .get(&by_long.short_name().to_display_string())
        .expect("findable by 8.3 alias");

    assert_eq!(by_long.name(), long);
    assert_eq!(by_lower.name(), long);
    assert_eq!(by_short.name(), long);
}

/// Reading a directory costs one transfer per run of its chain, and
/// re-reading it costs nothing at all.
///
/// The second half is the point. Opening several files in one directory is
/// what the ROM picker does, and it is where a per-block implementation
/// spends a rescan each time.
#[test]
fn a_second_enumeration_touches_no_device() {
    let image = fixture("fat32-4k.img");
    let expected = layout("fat32-4k.img");

    let device = Recorder::new(FileImage::open(&image).expect("open"));
    let mut volume = FileSystem::mount(device).expect("mount");

    // The directory's own chain, from the manifest rather than from this
    // crate, so the expected call count comes from outside.
    let roms_runs = expected.file("/ROMS").runs.len();

    volume.device_mut().reset();
    let first = volume.open_dir("/ROMS").expect("open").len();
    let reads_for_root_and_roms = volume.device_mut().reads().len();
    assert!(
        reads_for_root_and_roms <= roms_runs + 2,
        "reading a directory should cost about one transfer per run, not {reads_for_root_and_roms}"
    );

    volume.device_mut().reset();
    let second = volume.open_dir("/ROMS").expect("open again").len();
    assert_eq!(
        volume.device_mut().reads().len(),
        0,
        "a second enumeration should issue no device calls"
    );
    assert_eq!(
        first, second,
        "the cached directory disagrees with the first read"
    );
}

/// Enumerating 300 entries is one pass over a slice, and lookup is a map
/// hit — so doing both for every entry is not quadratic.
#[test]
fn enumeration_and_lookup_agree() {
    let mut volume =
        FileSystem::mount(FileImage::open(fixture("fat32-4k.img")).expect("open")).expect("mount");
    let directory = volume.open_dir("/ROMS").expect("open");

    // 300 ROMs plus `.` and `..`, which are real entries and stay listed:
    // a caller walking back up needs `..`, and hiding them would mean this
    // crate deciding which of a directory's contents are real.
    assert_eq!(directory.len(), 302);
    for entry in directory.iter() {
        let name = entry.name();
        let found = directory
            .get(name)
            .unwrap_or_else(|| panic!("{name} enumerated but not findable"));
        assert_eq!(found.first_cluster(), entry.first_cluster(), "{name}");
    }
}

/// Paths resolve, including the forms that should mean the same thing.
#[test]
fn paths_resolve() {
    let mut volume =
        FileSystem::mount(FileImage::open(fixture("fat32-4k.img")).expect("open")).expect("mount");

    let by_absolute = volume.open_dir("/ROMS").expect("absolute").len();
    let by_relative = volume.open_dir("ROMS").expect("relative").len();
    let by_dot = volume.open_dir("./ROMS").expect("dotted").len();
    assert_eq!(by_absolute, by_relative);
    assert_eq!(by_absolute, by_dot);

    let root = volume.root_dir().expect("root").len();
    assert_eq!(volume.open_dir("/").expect("slash").len(), root);
}

/// `..` from a child of the root leads back to the root, even though the
/// entry holds cluster 0 rather than the root's cluster number.
#[test]
fn dot_dot_from_a_child_of_the_root_reaches_the_root() {
    let mut volume =
        FileSystem::mount(FileImage::open(fixture("fat32-4k.img")).expect("open")).expect("mount");

    let root = volume.root_dir().expect("root").len();
    let up = volume.open_dir("/ROMS/..").expect("walk back up").len();
    assert_eq!(up, root, "`..` should reach the root, not cluster 0");
}

/// A missing name and a file used as a directory each report themselves.
#[test]
fn bad_paths_say_what_was_wrong() {
    let mut volume =
        FileSystem::mount(FileImage::open(fixture("fat32-4k.img")).expect("open")).expect("mount");

    match volume.open_dir("/NOPE") {
        Err(Error::NotFound { name }) => assert_eq!(name, "NOPE"),
        other => panic!("expected NotFound, got {other:?}"),
    }
    match volume.open_dir("/HELLO.TXT") {
        Err(Error::NotADirectory { name }) => assert_eq!(name, "HELLO.TXT"),
        other => panic!("expected NotADirectory, got {other:?}"),
    }
}

/// The volume label entry is not a file, and is not listed as one.
#[test]
fn the_volume_label_is_not_listed() {
    let mut volume =
        FileSystem::mount(FileImage::open(fixture("fat32-4k.img")).expect("open")).expect("mount");
    let root = volume.root_dir().expect("root");

    assert!(
        root.iter().all(|e| !e.attributes().is_volume_id()),
        "the volume label leaked into the listing"
    );
    assert!(
        root.get("RFAT4K").is_none(),
        "the volume label should not be findable as a file"
    );
}

// ---------------------------------------------------------------------------
// Tolerance: damage costs its own entry and nothing more
// ---------------------------------------------------------------------------

/// A long-name run with a broken sequence number costs that file its long
/// name — and costs the rest of the directory nothing.
///
/// The behaviour that matters most for a card someone else formatted: one
/// bad run must not swallow the names that follow it.
#[test]
fn a_damaged_long_name_run_does_not_poison_the_directory() {
    // Three files with long names in a scratch volume, so the damage is
    // done to something no other test shares.
    let scratch = long_named_scratch("longnames-sequence.img");
    let intact = directory_names(MemoryImage::load(&scratch).expect("load scratch"), "/");
    assert_eq!(
        intact.len(),
        3,
        "the scratch volume should hold three files"
    );
    assert!(
        intact.values().all(|long| long.is_some()),
        "all three should start with long names: {intact:?}"
    );

    let mut damaged = MemoryImage::load(&scratch).expect("load scratch");
    let root = root_dir_offset(&damaged);
    let slots = find_long_name_slots(damaged.bytes(), root, "MIDDLE~1.TXT");
    let continuation = slots
        .iter()
        .copied()
        .find(|&s| damaged.bytes()[s] & 0x40 == 0)
        .expect("the middle file's name should need more than one slot");
    // A sequence number that does not follow on from the slot before it.
    damaged.bytes_mut()[continuation] = 0x09;

    let after = directory_names(damaged, "/");
    assert_eq!(
        after.len(),
        intact.len(),
        "every file should still be listed: {after:?}"
    );
    assert_eq!(
        after.get("MIDDLE~1.TXT"),
        Some(&None),
        "the damaged entry should fall back to its 8.3 name"
    );
    for (short, long) in &intact {
        if short != "MIDDLE~1.TXT" {
            assert_eq!(after.get(short), Some(long), "{short} lost its name too");
        }
    }
}

/// A long name whose checksum disagrees with its entry is discarded, and
/// the file stays listed under its 8.3 name.
#[test]
fn a_checksum_mismatch_falls_back_to_the_short_name() {
    let scratch = long_named_scratch("longnames-checksum.img");
    let mut image = MemoryImage::load(&scratch).expect("load");
    let root = root_dir_offset(&image);
    let slots = find_long_name_slots(image.bytes(), root, "MIDDLE~1.TXT");
    assert!(!slots.is_empty(), "the file should have long-name slots");

    // Every slot in the run, not just one. Changing a single slot's
    // checksum breaks the run's *internal* agreement, which is caught by
    // the sequence check before the name is ever compared against its
    // entry — so it would test the wrong thing. Changing all of them
    // leaves a perfectly well-formed run that simply belongs to a
    // different entry, which is what this check is for.
    for slot in slots {
        image.bytes_mut()[slot + 0x0D] ^= 0xFF;
    }

    let after = directory_names(image, "/");
    assert_eq!(after.len(), 3, "the file should still be listed");
    assert_eq!(
        after.get("MIDDLE~1.TXT"),
        Some(&None),
        "a mismatched checksum should cost the long name, not the file"
    );
}

/// An allocated entry after the end-of-directory marker is reported.
///
/// Such entries are files no reader will list, because every reader stops
/// at the marker. Saying so beats stopping quietly.
#[test]
fn an_entry_after_the_end_marker_is_reported() {
    let scratch = long_named_scratch("longnames-endmarker.img");
    let mut image = MemoryImage::load(&scratch).expect("load");
    let root = root_dir_offset(&image);

    // Find the marker, then plant a plausible entry two slots past it.
    let bytes = image.bytes();
    let end = (0..)
        .map(|n| root + n * ENTRY_SIZE)
        .find(|&at| bytes[at] == 0x00)
        .expect("the root directory should have an end marker");

    let stray = end + 2 * ENTRY_SIZE;
    image.bytes_mut()[stray..stray + 11].copy_from_slice(b"STRAY   TXT");
    image.bytes_mut()[stray + 0x0B] = 0x20; // archive, a plain file

    let mut volume = FileSystem::mount(image).expect("mount");
    match volume.root_dir() {
        Err(Error::EntryAfterEnd { index }) => {
            assert_eq!(index, (stray - root) / ENTRY_SIZE)
        }
        other => panic!("expected EntryAfterEnd, got {}", describe(other)),
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// `mdir`'s entries as a map from 8.3 name to long name, with `.` and `..`
/// dropped since they are structure rather than content.
fn names_by_short(entries: &[MdirEntry]) -> BTreeMap<String, Option<String>> {
    entries
        .iter()
        .filter(|e| e.short != "." && e.short != "..")
        .map(|e| (e.short.to_uppercase(), e.long.clone()))
        .collect()
}

/// The same shape, from this crate.
fn names_by_short_of(directory: &Directory) -> BTreeMap<String, Option<String>> {
    directory
        .iter()
        .filter(|e| {
            let short = e.short_name().to_display_string();
            short != "." && short != ".."
        })
        .map(|e| {
            (
                e.short_name().to_display_string().to_uppercase(),
                e.long_name().map(str::to_owned),
            )
        })
        .collect()
}

/// This crate's view of a directory, as short-to-long names.
fn directory_names(image: MemoryImage, path: &str) -> BTreeMap<String, Option<String>> {
    let mut volume = FileSystem::mount(image).expect("mount");
    names_by_short_of(volume.open_dir(path).expect("open"))
}

/// Builds a small volume holding three files with long names, so the
/// tolerance tests have something to damage that is not a shared fixture.
///
/// `name` distinguishes one caller's image from another's: these tests run
/// in parallel, and a shared scratch path means two of them formatting and
/// populating the same file at once.
fn long_named_scratch(name: &str) -> std::path::PathBuf {
    let image = mkfs_image(name, 16, 4);
    for (name, contents) in [
        ("first one.txt", b"1" as &[u8]),
        ("middle one.txt", b"2"),
        ("last one.txt", b"3"),
    ] {
        let source = scratch_dir().join(name);
        std::fs::write(&source, contents).expect("write");
        mcopy_in(&image, &source, &format!("/{name}"));
    }
    assert_fsck_clean(&image);
    image
}

/// Byte offset of the root directory's first entry.
fn root_dir_offset(image: &MemoryImage) -> usize {
    let boot = image.bytes();
    let bytes_per_sector = u16::from_le_bytes([boot[0x0B], boot[0x0C]]) as usize;
    let sectors_per_cluster = boot[0x0D] as usize;
    let reserved = u16::from_le_bytes([boot[0x0E], boot[0x0F]]) as usize;
    let fats = boot[0x10] as usize;
    let sectors_per_fat =
        u32::from_le_bytes([boot[0x24], boot[0x25], boot[0x26], boot[0x27]]) as usize;
    let root_cluster =
        u32::from_le_bytes([boot[0x2C], boot[0x2D], boot[0x2E], boot[0x2F]]) as usize;

    let data_start = reserved + fats * sectors_per_fat;
    (data_start + (root_cluster - 2) * sectors_per_cluster) * bytes_per_sector
}

/// Offsets of every long-name slot belonging to the entry with this 8.3
/// name.
///
/// Harness code that walks the raw bytes rather than calling the crate, so
/// a test can damage a run in ways the crate would consider impossible.
fn find_long_name_slots(bytes: &[u8], root: usize, short: &str) -> Vec<usize> {
    let target: Vec<u8> = {
        let (base, extension) = short.split_once('.').unwrap_or((short, ""));
        let mut raw = [b' '; 11];
        raw[..base.len()].copy_from_slice(base.as_bytes());
        raw[8..8 + extension.len()].copy_from_slice(extension.as_bytes());
        raw.to_vec()
    };

    let mut slots: Vec<usize> = Vec::new();
    for n in 0.. {
        let at = root + n * ENTRY_SIZE;
        if at + ENTRY_SIZE > bytes.len() || bytes[at] == 0x00 {
            return Vec::new();
        }
        if bytes[at + 0x0B] == 0x0F {
            slots.push(at);
            continue;
        }
        if bytes[at..at + 11] == target[..] {
            // The run precedes the entry it names.
            return slots;
        }
        slots.clear();
    }
    Vec::new()
}

/// Formats a mount/read result without needing `Debug` on the device.
fn describe<T>(result: Result<T, Error<std::io::Error>>) -> String {
    match result {
        Ok(_) => "Ok(..)".to_string(),
        Err(e) => format!("{e:?}"),
    }
}
