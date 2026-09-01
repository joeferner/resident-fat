//! The clock entries are stamped from.
//!
//! `fsck.vfat` is deliberately not the oracle here, because **it does not
//! check dates at all** — a directory full of entries stamped
//! month 0 of day 0 passes it silently — so a test that ran `fsck` and
//! stopped would be checking nothing. `mdir` prints the date it read, which
//! is what an independent implementation makes of the bytes, and the packed
//! field is checked directly for the cases where a rendering could hide a
//! difference.

mod support;

use resident_fat::time::{Clock, DateTime, EpochClock, FnClock};
use resident_fat::{FileSystem, Packed};
use support::*;

/// A clock that reports whatever it was built with, and counts how often it
/// is asked.
///
/// The count matters: an operation that read the clock once per entry could
/// stamp a file's long-name slots and its 8.3 entry from different instants
/// if the two straddled a tick, which is the kind of thing that shows up
/// once a year.
struct FixedClock {
    at: DateTime,
    reads: std::cell::Cell<usize>,
}

impl FixedClock {
    fn new(at: DateTime) -> Self {
        FixedClock {
            at,
            reads: std::cell::Cell::new(0),
        }
    }
}

impl Clock for FixedClock {
    fn now(&self) -> DateTime {
        self.reads.set(self.reads.get() + 1);
        self.at
    }
}

fn at(year: u16, month: u8, day: u8, hour: u8, minute: u8, second: u8) -> DateTime {
    DateTime::new(year, month, day, hour, minute, second, 0).expect("a real date")
}

/// The raw directory entries of the volume's root.
fn raw_root<D: resident_fat::blockdev::BlockDevice>(volume: &mut FileSystem<D>) -> Vec<[u8; 32]> {
    let cluster = volume.root_cluster();
    volume
        .read_chain(cluster)
        .expect("read the root")
        .chunks_exact(32)
        .map(|entry| {
            let mut owned = [0u8; 32];
            owned.copy_from_slice(entry);
            owned
        })
        .collect()
}

/// A supplied clock reaches the volume, and an independent reader agrees.
#[test]
fn a_supplied_clock_stamps_what_it_says() {
    let image = mkfs_image("clock-supplied.img", 16, 4);
    let when = at(2026, 6, 15, 10, 30, 44);

    let mut volume = FileSystem::mount(FileImage::open_rw(&image).expect("open")).expect("mount");
    volume.set_clock(Box::new(FixedClock::new(when)));
    volume
        .write_file("/STAMPED.BIN", &expected_content(3000))
        .expect("write");
    volume.create_dir("/SUBDIR").expect("mkdir");
    volume
        .write_file("/a long name.txt", b"hello")
        .expect("write");
    volume.unmount().expect("unmount");

    // What this crate reads back.
    let mut volume = FileSystem::mount(FileImage::open(&image).expect("open")).expect("remount");
    let root = volume.root_dir().expect("root");
    for name in ["STAMPED.BIN", "SUBDIR", "a long name.txt"] {
        let entry = root
            .get(name)
            .unwrap_or_else(|| panic!("{name} is missing"));
        assert_eq!(entry.created(), when, "{name} creation");
        assert_eq!(entry.modified(), when, "{name} modification");
        // Access is a date with no time, so it comes back at midnight.
        assert_eq!(entry.accessed(), at(2026, 6, 15, 0, 0, 0), "{name} access");
    }

    // And what a reader that is not this crate makes of the same bytes.
    let listing = mdir(&image, "");
    assert!(
        listing.contains("2026-06-15") && listing.contains("10:30"),
        "mtools read a different date:\n{listing}"
    );
}

/// Without a clock, entries carry the epoch — which is a date, unlike the
/// zeroes an implementation with nothing to say would otherwise write.
#[test]
fn the_default_clock_is_the_epoch() {
    let image = mkfs_image("clock-default.img", 16, 4);
    let mut volume = FileSystem::mount(FileImage::open_rw(&image).expect("open")).expect("mount");
    volume.write_file("/PLAIN.BIN", b"x").expect("write");

    // Checked in the packed field as well as through the accessors: 0x0021
    // is year 0, month 1, day 1, and 0x0000 is the month-0 day-0 date that
    // has no meaning and that `fsck` would not object to.
    for entry in raw_root(&mut volume) {
        if entry[0] == 0x00 || entry[0] == 0xE5 || entry[0x0B] & 0x08 != 0 {
            continue;
        }
        let date = u16::from_le_bytes([entry[0x10], entry[0x11]]);
        assert_eq!(date, 0x0021, "an entry was stamped with a non-date");
    }

    volume.unmount().expect("unmount");
    let listing = mdir(&image, "");
    assert!(
        listing.contains("1980-01-01") && !listing.contains("1980-00-00"),
        "the default should be a date that exists:\n{listing}"
    );
    assert_eq!(EpochClock.now(), DateTime::EPOCH);
}

/// Writing to a file updates its modification time, and leaves its creation
/// time alone.
///
/// The distinction is the point of having three timestamps: a file created
/// once and appended to for a year should say so.
#[test]
fn writing_moves_the_modification_time_only() {
    let image = mkfs_image("clock-modified.img", 16, 4);
    let created = at(2026, 1, 2, 3, 4, 4);
    let written = at(2026, 9, 30, 20, 15, 30);

    let mut volume = FileSystem::mount(FileImage::open_rw(&image).expect("open")).expect("mount");
    volume.set_clock(Box::new(FixedClock::new(created)));
    let mut file = volume.create_with_size("/GROW.BIN", 0).expect("create");

    volume.set_clock(Box::new(FixedClock::new(written)));
    volume
        .write_at(&mut file, 0, &expected_content(5000))
        .expect("write");
    volume.unmount().expect("unmount");

    let mut volume = FileSystem::mount(FileImage::open(&image).expect("open")).expect("remount");
    let entry = volume
        .root_dir()
        .expect("root")
        .get("GROW.BIN")
        .expect("the file")
        .clone();
    assert_eq!(entry.created(), created, "creation should not have moved");
    assert_eq!(entry.modified(), written, "modification should have");
    assert_eq!(entry.len(), 5000);
}

/// One operation reads the clock once, so every entry it writes carries the
/// same instant.
///
/// A long name is several directory entries written together. Reading the
/// clock per entry would let a tick fall between them, and the file's own
/// slots would then disagree about when it was made.
#[test]
fn an_operation_reads_the_clock_once() {
    let image = mkfs_image("clock-once.img", 16, 4);
    let clock = SharedClock::default();

    let mut volume = FileSystem::mount(FileImage::open_rw(&image).expect("open")).expect("mount");
    volume.set_clock(Box::new(clock.clone()));
    volume
        .write_file("/a name long enough to need slots.bin", b"hi")
        .expect("write");

    // `write_file` is a remove that finds nothing, a create, and a write, so
    // more than one is expected -- what must not happen is one per entry,
    // which for this name would be four.
    let reads = clock.reads();
    assert!(
        (1..=3).contains(&reads),
        "expected one read per operation, got {reads}"
    );
    volume.unmount().expect("unmount");
}

/// A clock with no state of its own, which is the common shape on a board
/// whose wall clock is a global.
#[test]
fn a_function_can_be_a_clock() {
    fn boot_time() -> DateTime {
        // 2026-01-01 00:00:00 UTC.
        DateTime::from_unix_seconds(1_767_225_600)
    }

    let image = mkfs_image("clock-fn.img", 16, 4);
    let mut volume = FileSystem::mount(FileImage::open_rw(&image).expect("open")).expect("mount");
    volume.set_clock(Box::new(FnClock::new(boot_time)));
    volume.write_file("/FN.BIN", b"x").expect("write");
    volume.unmount().expect("unmount");

    let mut volume = FileSystem::mount(FileImage::open(&image).expect("open")).expect("remount");
    assert_eq!(
        volume
            .root_dir()
            .expect("root")
            .get("FN.BIN")
            .expect("file")
            .created(),
        at(2026, 1, 1, 0, 0, 0)
    );
}

/// A clock reporting a time FAT cannot hold is clamped, not wrapped.
///
/// The failure that matters: 1979 wrapping to 2107 would stamp a device with
/// an unset clock a century in the future, which sorts to the top of every
/// listing and looks like corruption rather than like a missing clock.
#[test]
fn an_out_of_range_clock_is_clamped() {
    let image = mkfs_image("clock-range.img", 16, 4);
    let mut volume = FileSystem::mount(FileImage::open_rw(&image).expect("open")).expect("mount");

    volume.set_clock(Box::new(FixedClock::new(at(1970, 1, 1, 0, 0, 0))));
    volume.write_file("/EARLY.BIN", b"x").expect("write");
    volume.set_clock(Box::new(FixedClock::new(at(2200, 1, 1, 0, 0, 0))));
    volume.write_file("/LATE.BIN", b"x").expect("write");
    volume.unmount().expect("unmount");

    let mut volume = FileSystem::mount(FileImage::open(&image).expect("open")).expect("remount");
    let root = volume.root_dir().expect("root");
    assert_eq!(
        root.get("EARLY.BIN").expect("early").created(),
        DateTime::EPOCH
    );
    assert_eq!(
        root.get("LATE.BIN").expect("late").created(),
        DateTime::LATEST
    );

    let listing = mdir(&image, "");
    assert!(
        !listing.contains("1970"),
        "1970 is not storable:\n{listing}"
    );
    assert!(
        !listing.contains("2200"),
        "2200 is not storable:\n{listing}"
    );
}

/// The timestamps a volume written elsewhere carries are read back as they
/// were stored.
#[test]
fn timestamps_written_by_mtools_read_back() {
    // The fixtures are stamped `2026-01-02 03:04:05` by `mkfixtures.sh`,
    // which `mcopy -m` preserved from the source files' mtimes.
    let mut volume =
        FileSystem::mount(FileImage::open(fixture("fat32-4k.img")).expect("open")).expect("mount");
    let entry = volume
        .root_dir()
        .expect("root")
        .get("BIG.BIN")
        .expect("BIG.BIN")
        .clone();

    let modified = entry.modified();
    assert_eq!((modified.year, modified.month, modified.day), (2026, 1, 2));
    assert_eq!((modified.hour, modified.minute), (3, 4));
    // Two-second granularity: 05 is stored as 2, and comes back as 4.
    assert_eq!(modified.second, 4);
}

/// A `Packed` value round-trips through an entry, which is the type the
/// accessors are built on.
#[test]
fn the_packed_form_is_public_and_round_trips() {
    let when = at(2026, 6, 15, 10, 30, 45);
    let packed: Packed = when.pack();
    assert_eq!(DateTime::unpack(packed), when);
}

/// A counting clock a test can keep a handle on after handing one to the
/// volume.
#[derive(Clone, Default)]
struct SharedClock(std::sync::Arc<std::sync::atomic::AtomicUsize>);

impl SharedClock {
    fn reads(&self) -> usize {
        self.0.load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl Clock for SharedClock {
    fn now(&self) -> DateTime {
        self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        DateTime::EPOCH
    }
}
