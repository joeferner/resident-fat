//! Creating long names, aliases and directories.
//!
//! The oracles are the ones that matter here, because a name is only
//! correct if something that is not this crate agrees about it. `mdir`
//! reports both the long name and the 8.3 alias, from an implementation
//! sharing none of our assumptions; `fsck.vfat` decides whether a
//! long-name run is well formed at all — a checksum that disagrees with its
//! entry, a sequence with a gap, or a run left behind by a delete are each
//! things it names and we do not.
//!
//! The counting tests read the directory's raw bytes rather than its parsed
//! form. "An 8.3 name writes no long-name slots" is a claim about what is
//! on the volume, and asking our own parser what it found would answer a
//! different question.

mod support;

use resident_fat::dir::ENTRY_SIZE;
use resident_fat::{Error, FileSystem};
use support::*;

/// A long-name slot is marked by all four of these bits at once.
const LONG_NAME_ATTRIBUTE: u8 = 0x0F;

/// The raw directory entries of the chain at `cluster`, as bytes.
///
/// Read through the crate's transfer path but not through its parser, which
/// is the part these tests are checking.
fn raw_entries<D: resident_fat::blockdev::BlockDevice>(
    volume: &mut FileSystem<D>,
    cluster: u32,
) -> Vec<[u8; ENTRY_SIZE]> {
    volume
        .read_chain(cluster)
        .expect("read the directory")
        .chunks_exact(ENTRY_SIZE)
        .map(|entry| {
            let mut owned = [0u8; ENTRY_SIZE];
            owned.copy_from_slice(entry);
            owned
        })
        .collect()
}

/// How many of a directory's slots hold long-name fragments.
fn long_name_slots<D: resident_fat::blockdev::BlockDevice>(
    volume: &mut FileSystem<D>,
    cluster: u32,
) -> usize {
    raw_entries(volume, cluster)
        .iter()
        .filter(|entry| entry[0] != 0x00 && entry[0] != 0xE5 && entry[0x0B] == LONG_NAME_ATTRIBUTE)
        .count()
}

/// How many of a directory's slots are in use at all.
fn used_slots<D: resident_fat::blockdev::BlockDevice>(
    volume: &mut FileSystem<D>,
    cluster: u32,
) -> usize {
    raw_entries(volume, cluster)
        .iter()
        .filter(|entry| entry[0] != 0x00 && entry[0] != 0xE5)
        .count()
}

/// A name that already fits 8.3 costs one directory entry, whatever its
/// case.
///
/// This is the saving the case bits buy, and it is the reason a directory
/// of several hundred ROMs stays small: the alternative is three entries per
/// file — two of long name, one of alias — for names that need none of it.
#[test]
fn an_8_3_name_writes_no_long_name_slots() {
    let image = mkfs_image("name-short.img", 16, 4);
    let mut volume = FileSystem::mount(FileImage::open_rw(&image).expect("open")).expect("mount");

    let names = ["game.nes", "GAME2.NES", "readme", "123.TXT", "MiXeD.txt"];
    for name in names {
        volume.create(&format!("/{name}")).expect("create");
    }

    let root = volume.root_cluster();
    assert_eq!(
        long_name_slots(&mut volume, root),
        1,
        "only the one genuinely mixed-case name should have cost slots"
    );

    volume.sync().expect("sync");
    drop(volume);
    assert_fsck_clean(&image);

    // And mtools reports the case back, from the case bits alone.
    let listed = mdir_entries(&image, "");
    for name in names {
        let entry = listed
            .iter()
            .find(|e| e.short.eq_ignore_ascii_case(name) || e.long.as_deref() == Some(name))
            .unwrap_or_else(|| panic!("mdir did not list {name}: {listed:?}"));
        if name == "MiXeD.txt" {
            assert_eq!(entry.long.as_deref(), Some("MiXeD.txt"));
            assert_eq!(entry.short, "MIXED.TXT", "the alias is the name uppercased");
        } else {
            assert_eq!(entry.long, None, "{name} should have no long name");
            assert_eq!(entry.short, name, "{name} should come back exactly");
        }
    }
}

/// A created entry carries a date that exists.
///
/// There is no clock to ask, so every entry gets the FAT epoch — and the
/// point is that this is *not* the same as leaving the fields zero. Zero is
/// year 1980, month 0, day 0, which is not a date; `mdir` renders it as
/// `1980-00-00`.
///
/// `fsck.vfat` does not object to it, which is exactly why this is asserted
/// here. The oracle carries most of the correctness load in these tests, and
/// on this one point it has nothing to say — so a test that only ran `fsck`
/// would let an impossible date through, as one did until this was written.
#[test]
fn created_entries_carry_a_date_that_exists() {
    let image = mkfs_image("name-dates.img", 16, 4);
    let mut volume = FileSystem::mount(FileImage::open_rw(&image).expect("open")).expect("mount");

    volume.create("/PLAIN.BIN").expect("create");
    volume
        .write_file("/a long name.bin", &[1, 2, 3])
        .expect("write");
    volume.create_dir("/SUBDIR").expect("mkdir");

    // The epoch is 0x0021 packed: year 0, month 1, day 1. Zero would be
    // month 0, day 0.
    let root = volume.root_cluster();
    for entry in raw_entries(&mut volume, root) {
        if entry[0] == 0x00 || entry[0] == 0xE5 || entry[0x0B] == LONG_NAME_ATTRIBUTE {
            continue;
        }
        if entry[0x0B] & 0x08 != 0 {
            continue; // the volume label, which mkfs wrote
        }
        for (field, at) in [("creation", 0x10), ("access", 0x12), ("write", 0x18)] {
            let date = u16::from_le_bytes([entry[at], entry[at + 1]]);
            assert_eq!(
                date,
                0x0021,
                "{field} date of {:?} is not the epoch",
                &entry[0..11]
            );
        }
    }

    volume.sync().expect("sync");
    drop(volume);

    assert_fsck_clean(&image);
    for directory in ["", "SUBDIR"] {
        let listing = mdir(&image, directory);
        assert!(
            !listing.contains("1980-00-00"),
            "an impossible date reached the volume:\n{listing}"
        );
        assert!(
            listing.contains("1980-01-01"),
            "expected the epoch in {directory:?}:\n{listing}"
        );
    }
}

/// The names a long name gets stored under are the ones `mtools` would have
/// chosen.
///
/// The alias is the part worth an independent oracle, and comparing our own
/// output to `mdir` would not be one: `mdir` only reports back the alias we
/// wrote. So the same names go into two identical fresh volumes — one
/// filled by `mcopy`, one by this crate, in the same order — and the
/// aliases are compared. Anything we mangle differently shows up as a
/// mismatch against a codebase that shares none of our assumptions.
///
/// An alias is a name people see and scripts hard-code, so agreeing about
/// it matters beyond the volume being well formed.
#[test]
fn aliases_are_the_ones_mtools_would_have_chosen() {
    // Chosen for the branches: a plain long name; an embedded period, where
    // the extension is the last one and not the first; a leading period; a
    // double extension; characters that are legal long and replaced short;
    // and a name that fits 8.3 except for its case, which is the one that
    // should get no tail at all.
    let names = [
        "A Long Name.txt",
        "Super Mario Bros. 3.nes",
        ".gitignore",
        "archive.tar.gz",
        "plus+and=equals.dat",
        "ReadMe.Now",
    ];
    let data = expected_content(600);

    let theirs = mkfs_image("name-long-mtools.img", 16, 4);
    let source = scratch_dir().join("name-long-source.bin");
    std::fs::write(&source, &data).expect("write the source file");
    for name in names {
        mcopy_in(&theirs, &source, name);
    }

    let ours = mkfs_image("name-long-ours.img", 16, 4);
    let mut volume = FileSystem::mount(FileImage::open_rw(&ours).expect("open")).expect("mount");
    for name in names {
        volume
            .write_file(&format!("/{name}"), &data)
            .expect("write");
    }
    volume.sync().expect("sync");
    drop(volume);

    assert_fsck_clean(&ours);

    let mine = mdir_entries(&ours, "");
    let mtools = mdir_entries(&theirs, "");
    let mut aliases = Vec::new();
    for name in names {
        let find = |listed: &[MdirEntry], which: &str| {
            listed
                .iter()
                .find(|e| e.long.as_deref() == Some(name) || e.short == name)
                .unwrap_or_else(|| panic!("{which} did not list {name}: {listed:?}"))
                .short
                .clone()
        };
        let expected = find(&mtools, "mtools");
        assert_eq!(find(&mine, "we"), expected, "{name} got a different alias");
        aliases.push(expected);
    }

    // "ReadMe.Now" fits 8.3 but for its case, so it should have picked up
    // the exact alias rather than a numeric tail -- if that branch were
    // missing the two implementations would still agree, on the wrong
    // answer, so it is worth naming.
    assert_eq!(aliases[5], "README.NOW");

    // And both names find the file again, from this crate.
    let mut volume = FileSystem::mount(FileImage::open(&ours).expect("open")).expect("remount");
    for (name, alias) in names.iter().zip(&aliases) {
        let by_long = volume.open(&format!("/{name}")).expect("open by long name");
        let by_alias = volume.open(&format!("/{alias}")).expect("open by alias");
        assert_eq!(by_long, by_alias, "{name} and {alias} are the same file");
        assert_eq!(volume.read_all(&by_long).expect("read"), data);
    }
}

/// A basis name colliding many times keeps getting a longer tail, and every
/// one of the files stays reachable.
///
/// Linux stops probing sequentially after nine because each probe is a
/// directory scan. With the directory resident a probe is a map lookup, so
/// there is no reason to stop — and no need for the hash-based alias a
/// rescanning implementation is pushed into.
#[test]
fn a_name_colliding_many_times_still_resolves() {
    let image = mkfs_image("name-collide.img", 16, 4);
    let mut volume = FileSystem::mount(FileImage::open_rw(&image).expect("open")).expect("mount");

    let names: Vec<String> = (1..=12)
        .map(|n| format!("Long Game Title {n}.nes"))
        .collect();
    for name in &names {
        volume.create(&format!("/{name}")).expect("create");
    }

    // Nine two-character tails, then the base gives up a character so the
    // tail can have three.
    let expected: Vec<String> = (1..=9)
        .map(|n| format!("LONGGA~{n}.NES"))
        .chain((10..=12).map(|n| format!("LONGG~{n}.NES")))
        .collect();

    volume.sync().expect("sync");
    drop(volume);
    assert_fsck_clean(&image);

    let listed = mdir_entries(&image, "");
    for (name, alias) in names.iter().zip(&expected) {
        let entry = listed
            .iter()
            .find(|e| e.long.as_deref() == Some(name.as_str()))
            .unwrap_or_else(|| panic!("mdir did not report {name}: {listed:?}"));
        assert_eq!(&entry.short, alias, "{name} got the wrong alias");
    }

    let mut volume = FileSystem::mount(FileImage::open(&image).expect("open")).expect("remount");
    for (name, alias) in names.iter().zip(&expected) {
        assert!(
            volume.open(&format!("/{name}")).is_ok(),
            "{name} should still be openable"
        );
        assert!(
            volume.open(&format!("/{alias}")).is_ok(),
            "{alias} should be openable too"
        );
    }
}

/// A long name that does not fit in the directory's current cluster spans
/// into the next one, and is still one name.
///
/// A run of slots is consecutive by directory *index*, and consecutive
/// indices need not be consecutive clusters. Nothing in the format forbids
/// the split, so the thing to check is that neither we nor anyone else
/// treats a cluster boundary as the end of a run.
#[test]
fn a_long_name_run_can_span_a_cluster_boundary() {
    // 1 KB clusters hold 32 slots, so filling 20 of them puts the 21 a
    // 255-character name needs across the boundary.
    let image = mkfs_image("name-straddle.img", 16, 1);
    let mut volume = FileSystem::mount(FileImage::open_rw(&image).expect("open")).expect("mount");

    let root = volume.root_cluster();
    while used_slots(&mut volume, root) < 20 {
        let n = used_slots(&mut volume, root);
        volume.create(&format!("/PAD{n}.BIN")).expect("pad");
    }
    assert_eq!(volume.runs(root).expect("runs")[0].clusters, 1);

    let long = format!("{}.txt", "n".repeat(251));
    let data = expected_content(300);
    volume
        .write_file(&format!("/{long}"), &data)
        .expect("write the long name");

    assert!(
        volume
            .runs(root)
            .expect("runs")
            .iter()
            .map(|run| run.clusters)
            .sum::<u32>()
            > 1,
        "the name should have forced the directory to grow"
    );

    volume.sync().expect("sync");
    drop(volume);

    assert_fsck_clean(&image);
    let listed = mdir_entries(&image, "");
    assert!(
        listed.iter().any(|e| e.long.as_deref() == Some(&long)),
        "mtools lost the straddling name: {listed:?}"
    );

    let mut volume = FileSystem::mount(FileImage::open(&image).expect("open")).expect("remount");
    let file = volume.open(&format!("/{long}")).expect("open");
    assert_eq!(volume.read_all(&file).expect("read"), data);
}

/// An interrupted name write never leaves an entry stranded past the
/// directory's end marker.
///
/// A name's slots can span more than one block, and a block is the smallest
/// thing a device will accept — so a long-name write is several writes, and
/// the order they land in decides what an interruption leaves.
///
/// Writing the block holding the 8.3 entry first is the ordering that goes
/// wrong: interrupted there, the entry sits after slots that are still the
/// unused 0x00 marker. Every reader stops at that marker, so the file
/// exists, owns clusters, and is invisible — which is worse than a leak,
/// because nothing will ever reclaim it. Writing the slots first leaves the
/// opposite: fragments with no entry after them, which every reader
/// including this one treats as nothing at all.
///
/// The failing device is what makes this a test rather than a claim. With
/// the writes in the wrong order and nothing interrupted, the volume comes
/// out identical either way.
#[test]
fn an_interrupted_name_write_never_strands_an_entry() {
    // 1 KB clusters, so a block boundary falls at slot 16 and a 21-slot
    // name started at slot 10 straddles it while staying in one cluster.
    let long = format!("{}.txt", "s".repeat(251));

    for fail_after in 0..12 {
        let image = mkfs_image(&format!("name-fail-{fail_after}.img"), 16, 1);
        {
            let mut volume =
                FileSystem::mount(FileImage::open_rw(&image).expect("open")).expect("setup");
            // The volume label already occupies slot 0, so nine 8.3 names
            // take the count to ten and leave the next run starting there.
            for n in 0..9 {
                volume.create(&format!("/PAD{n}.BIN")).expect("pad");
            }
            let root = volume.root_cluster();
            assert_eq!(used_slots(&mut volume, root), 10);
            volume.sync().expect("sync");
        }

        {
            let device = FailAfter::new(FileImage::open_rw(&image).expect("open"), fail_after);
            let mut volume = FileSystem::mount(device).expect("mount");
            // Either outcome is allowed. What is not allowed is what it
            // leaves behind.
            let _ = volume.write_file(&format!("/{long}"), &expected_content(500));
            let _ = volume.sync();
        }

        let mut volume = FileSystem::mount(FileImage::open(&image).expect("open"))
            .unwrap_or_else(|e| panic!("fail_after={fail_after}: could not remount: {e:?}"));
        volume.root_dir().unwrap_or_else(|e| {
            panic!("fail_after={fail_after}: the root directory is unreadable: {e:?}")
        });
        // The bystanders were complete before the interrupted write began,
        // so none of them may have been disturbed by it.
        for n in 0..9 {
            let name = format!("/PAD{n}.BIN");
            volume
                .open(&name)
                .unwrap_or_else(|e| panic!("fail_after={fail_after}: {name} went missing: {e:?}"));
        }

        let report = fsck(&image);
        assert_eq!(
            report.indicates_corruption(),
            None,
            "fail_after={fail_after}:\n{}",
            report.output
        );
    }
}

/// Deleting a long-named file takes its slots with it.
///
/// Leaving them is not merely untidy: they are directory space nothing can
/// reuse, and `fsck.vfat` reports every one of them. The reuse is what the
/// slot count checks — a delete that only marked the 8.3 entry would leave
/// the directory growing with every rewrite.
#[test]
fn deleting_a_long_name_leaves_no_orphan_slots() {
    let image = mkfs_image("name-delete.img", 16, 4);
    let mut volume = FileSystem::mount(FileImage::open_rw(&image).expect("open")).expect("mount");
    let root = volume.root_cluster();

    volume
        .write_file("/A Fairly Long Name.txt", &expected_content(2000))
        .expect("write");
    let with_file = used_slots(&mut volume, root);
    assert!(with_file > 2, "the name should have needed slots");

    volume.remove("/A Fairly Long Name.txt").expect("remove");
    assert_eq!(
        used_slots(&mut volume, root),
        1,
        "only the volume label should be left in use"
    );

    volume.sync().expect("sync");
    drop(volume);

    let report = fsck(&image);
    assert!(
        !report.output.contains("Orphaned long file name part"),
        "the long name outlived its file:\n{}",
        report.output
    );
    assert!(report.clean, "fsck rejected the volume:\n{}", report.output);
}

/// Rewriting the same long-named file does not grow its directory.
///
/// `write_file` removes and recreates, so this is the delete path and the
/// create path having to agree about the same slots. If a delete left its
/// run behind, an over-the-air update writing the same file each time would
/// grow the directory until it filled the volume.
#[test]
fn rewriting_a_long_named_file_does_not_grow_the_directory() {
    let image = mkfs_image("name-rewrite.img", 16, 4);
    let mut volume = FileSystem::mount(FileImage::open_rw(&image).expect("open")).expect("mount");
    let root = volume.root_cluster();

    let name = "/firmware bundle.img";
    volume
        .write_file(name, &expected_content(5000))
        .expect("write");
    let settled = used_slots(&mut volume, root);

    for round in 0..6 {
        let data = expected_content(5000 + round * 100);
        volume.write_file(name, &data).expect("rewrite");
        assert_eq!(
            used_slots(&mut volume, root),
            settled,
            "round {round} left slots behind"
        );
    }

    volume.sync().expect("sync");
    drop(volume);
    assert_fsck_clean(&image);
    assert_eq!(mcopy_read(&image, "/firmware bundle.img").len(), 5500);
}

/// A created directory carries the `.` and `..` entries the format
/// requires, with `..` naming cluster 0 when the parent is the root.
///
/// That last part is the trap: the cluster a directory entry carries and
/// the cluster the directory's data lives at are the same number
/// everywhere except here. Writing the root's real cluster gives a volume
/// that mounts, reads and lists correctly, and that `fsck.vfat` rejects.
#[test]
fn a_created_directory_links_back_to_its_parent() {
    let image = mkfs_image("name-mkdir.img", 16, 4);
    let mut volume = FileSystem::mount(FileImage::open_rw(&image).expect("open")).expect("mount");

    let root = volume.root_cluster();
    let sub = volume.create_dir("/SAVES").expect("create /SAVES");
    let deep = volume
        .create_dir("/SAVES/2026")
        .expect("create /SAVES/2026");

    let listed = volume.open_dir("/SAVES").expect("open /SAVES");
    let dot = listed.get(".").expect(". is missing");
    let dot_dot = listed.get("..").expect(".. is missing");
    assert_eq!(dot.first_cluster(), sub, ". names its own directory");
    assert_eq!(
        dot_dot.first_cluster(),
        0,
        ".. in a child of the root stores 0, not the root's cluster"
    );

    let listed = volume.open_dir("/SAVES/2026").expect("open /SAVES/2026");
    assert_eq!(
        listed.get("..").expect("..").first_cluster(),
        sub,
        ".. names the real parent everywhere else"
    );
    assert_ne!(deep, sub);
    assert_ne!(sub, root);

    let data = expected_content(3000);
    volume
        .write_file("/SAVES/2026/SLOT1.SAV", &data)
        .expect("write");

    // Resolution follows the links back up, including the 0 that stands for
    // the root.
    let entries = volume.open_dir("/SAVES/2026/..").expect("up one").len();
    assert_eq!(entries, volume.open_dir("/SAVES").expect("again").len());
    assert!(
        volume.open("/SAVES/2026/../2026/SLOT1.SAV").is_ok(),
        "a path going up and back down should find the same file"
    );
    assert!(
        volume.open("/SAVES/..").is_err(),
        "the root is a directory, not a file"
    );

    volume.sync().expect("sync");
    drop(volume);

    assert_fsck_clean(&image);
    assert_eq!(mcopy_read(&image, "/SAVES/2026/SLOT1.SAV"), data);
    let listed = mdir_entries(&image, "SAVES");
    assert!(
        listed.iter().any(|e| e.short == "2026"),
        "mtools did not list the nested directory: {listed:?}"
    );
}

/// A directory with a long name works like any other, and removing one
/// requires it to be empty.
#[test]
fn directories_take_long_names_and_refuse_to_vanish_with_contents() {
    let image = mkfs_image("name-rmdir.img", 16, 4);
    let mut volume = FileSystem::mount(FileImage::open_rw(&image).expect("open")).expect("mount");

    volume.create_dir("/My Save Games").expect("create");
    volume
        .write_file("/My Save Games/slot one.sav", &expected_content(700))
        .expect("write");

    match volume.remove_dir("/My Save Games") {
        Err(Error::DirectoryNotEmpty { .. }) => {}
        other => panic!("expected DirectoryNotEmpty, got {other:?}"),
    }

    volume.sync().expect("sync");
    {
        // Everything so far survives an independent reading.
        assert_fsck_clean(&image);
        let listed = mdir_entries(&image, "");
        assert!(
            listed
                .iter()
                .any(|e| e.long.as_deref() == Some("My Save Games")),
            "mtools did not list the directory: {listed:?}"
        );
    }

    volume
        .remove("/My Save Games/slot one.sav")
        .expect("remove");
    volume
        .remove_dir("/My Save Games")
        .expect("remove the directory");
    volume.sync().expect("sync");
    drop(volume);

    assert_fsck_clean(&image);
    let listed = mdir_entries(&image, "");
    assert!(
        !listed
            .iter()
            .any(|e| e.long.as_deref() == Some("My Save Games")),
        "the directory outlived its removal: {listed:?}"
    );
}

fn mcopy_read(image: &std::path::Path, path: &str) -> Vec<u8> {
    let out = scratch_dir().join(format!(
        "read-{}",
        path.trim_start_matches('/').replace(['/', ' '], "_")
    ));
    let _ = std::fs::remove_file(&out);
    mcopy_out(image, path, &out);
    std::fs::read(&out).expect("could not read what mcopy wrote out")
}
