//! Writing, allocation and truncation.
//!
//! Two oracles do the real work here, and neither is ours. `fsck.vfat`
//! decides whether what we wrote is a consistent volume — the first time in
//! this project it is judging this crate's output rather than confirming a
//! fixture. `mtools` decides whether the bytes are the bytes, from an
//! implementation that shares none of our assumptions.
//!
//! The injected-failure tests are the ones that matter most. Ordering rules
//! only pay off when something goes wrong, so a rule nobody has interrupted
//! is a rule nobody has tested.

mod support;

use resident_fat::blockdev::BLOCK_SIZE;
use resident_fat::{Error, FatError, FileSystem};
use support::*;

/// A file written in one go is contiguous, and reads back in one call.
///
/// The point of `create_with_size`: knowing the length up front lets the
/// allocator find a single run, so the file can afterwards be moved in one
/// transfer. A file grown a write at a time cannot promise that.
#[test]
fn a_sized_create_is_contiguous_and_costs_one_write() {
    let image = mkfs_image("write-sized.img", 272, 4);
    let payload = expected_content(1600 * 1024);

    let device = Recorder::new(FileImage::open_rw(&image).expect("open"));
    let mut volume = FileSystem::mount(device).expect("mount");

    let mut file = volume
        .create_with_size("/BIG.BIN", payload.len() as u32)
        .expect("create");
    volume.device_mut().reset();
    volume.write_at(&mut file, 0, &payload).expect("write");

    let data_writes: Vec<_> = volume
        .device_mut()
        .writes()
        .into_iter()
        .filter(|w| w.blocks > 1)
        .collect();
    assert_eq!(
        data_writes.len(),
        1,
        "a contiguous file should be one data transfer: {data_writes:?}"
    );
    assert_eq!(data_writes[0].blocks as usize, payload.len() / BLOCK_SIZE);

    volume.sync().expect("sync");
    assert_eq!(volume.runs(file.first_cluster()).expect("runs").len(), 1);
    drop(volume);

    assert_fsck_clean(&image);
    assert_eq!(mcopy_read(&image, "/BIG.BIN"), payload, "mtools disagrees");
}

/// Files of many shapes round-trip through `mtools`, and leave a volume
/// `fsck` is happy with.
#[test]
fn written_files_round_trip_through_mtools() {
    let image = mkfs_image("write-shapes.img", 272, 4);
    let mut volume = FileSystem::mount(FileImage::open_rw(&image).expect("open")).expect("mount");

    // Sizes chosen around the boundaries a run-based writer gets wrong:
    // empty, sub-block, exactly a block, sub-cluster, exactly a cluster,
    // and several clusters plus a remainder.
    let sizes = [0usize, 1, 511, 512, 513, 4095, 4096, 4097, 40_000];
    let mut written = Vec::new();
    for (n, size) in sizes.iter().enumerate() {
        let name = format!("/F{n}.BIN");
        let data = expected_content(*size);
        volume.write_file(&name, &data).expect("write");
        written.push((name, data));
    }
    volume.sync().expect("sync");
    drop(volume);

    assert_fsck_clean(&image);
    for (name, data) in &written {
        if data.is_empty() {
            continue; // mtools declines to copy out an empty file
        }
        assert_eq!(&mcopy_read(&image, name), data, "{name} differs");
    }

    // And this crate reads back what it wrote.
    let mut volume = FileSystem::mount(FileImage::open(&image).expect("open")).expect("remount");
    for (name, data) in &written {
        let file = volume.open(name).expect("open");
        assert_eq!(file.len() as usize, data.len(), "{name} length");
        assert_eq!(
            &volume.read_all(&file).expect("read"),
            data,
            "{name} content"
        );
    }
}

/// A file written by `mtools` can be overwritten by this crate, and vice
/// versa.
#[test]
fn this_crate_and_mtools_can_share_a_volume() {
    let image = mkfs_image("write-shared.img", 272, 4);

    let theirs = expected_content(9000);
    mcopy_in(&image, host_file("theirs.bin", &theirs), "/THEIRS.BIN");

    let mut volume = FileSystem::mount(FileImage::open_rw(&image).expect("open")).expect("mount");
    let file = volume.open("/THEIRS.BIN").expect("open theirs");
    assert_eq!(volume.read_all(&file).expect("read"), theirs);

    let ours = expected_content(12_345);
    volume.write_file("/OURS.BIN", &ours).expect("write ours");
    volume.sync().expect("sync");
    drop(volume);

    assert_fsck_clean(&image);
    assert_eq!(mcopy_read(&image, "/OURS.BIN"), ours);
    assert_eq!(
        mcopy_read(&image, "/THEIRS.BIN"),
        theirs,
        "we damaged their file"
    );
}

/// Truncating shortens the file and frees the tail, and the volume stays
/// consistent.
#[test]
fn truncation_frees_the_tail() {
    let image = mkfs_image("write-truncate.img", 272, 4);
    let mut volume = FileSystem::mount(FileImage::open_rw(&image).expect("open")).expect("mount");

    let data = expected_content(60_000);
    let mut file = volume.write_file("/T.BIN", &data).expect("write");
    let before = volume.fat().free_clusters();

    volume.truncate(&mut file, 5000).expect("truncate");
    volume.sync().expect("sync");

    let after = volume.fat().free_clusters();
    assert!(after > before, "truncation should have freed clusters");
    assert_eq!(file.len(), 5000);
    drop(volume);

    assert_fsck_clean(&image);
    assert_eq!(
        mcopy_read(&image, "/T.BIN"),
        data[..5000],
        "content after truncation"
    );
}

/// Truncating a *fragmented* file is consistent too, which is where a
/// chain-walking truncation goes wrong.
#[test]
fn truncating_a_fragmented_file_stays_consistent() {
    let image = mkfs_image("write-fragtrunc.img", 272, 4);
    let mut volume = FileSystem::mount(FileImage::open_rw(&image).expect("open")).expect("mount");

    // Interleave two files so each ends up in several runs, then shorten
    // one of them across a run boundary.
    let mut a = volume.create("/A.BIN").expect("create A");
    let mut b = volume.create("/B.BIN").expect("create B");
    let chunk = expected_content(4096);
    for n in 0..8u64 {
        volume.write_at(&mut a, n * 4096, &chunk).expect("write A");
        volume.write_at(&mut b, n * 4096, &chunk).expect("write B");
    }
    volume.sync().expect("sync");

    let runs = volume.runs(a.first_cluster()).expect("runs");
    assert!(runs.len() > 1, "A should be fragmented: {runs:?}");

    volume.truncate(&mut a, 10_000).expect("truncate");
    volume.sync().expect("sync");
    let read = volume.read_all(&a).expect("read");
    drop(volume);

    assert_fsck_clean(&image);
    assert_eq!(read.len(), 10_000);
    let expected: Vec<u8> = (0..8).flat_map(|_| chunk.clone()).take(10_000).collect();
    assert_eq!(read, expected);
}

/// Truncating to nothing leaves a valid empty file, not a dangling chain.
#[test]
fn truncating_to_nothing_leaves_an_empty_file() {
    let image = mkfs_image("write-empty.img", 272, 4);
    let mut volume = FileSystem::mount(FileImage::open_rw(&image).expect("open")).expect("mount");

    let mut file = volume
        .write_file("/E.BIN", &expected_content(20_000))
        .expect("write");
    volume.truncate(&mut file, 0).expect("truncate");
    volume.sync().expect("sync");

    assert_eq!(file.len(), 0);
    assert_eq!(
        file.first_cluster(),
        0,
        "an empty file should own no clusters"
    );
    drop(volume);
    assert_fsck_clean(&image);
}

/// Deleting a file frees its clusters and leaves the volume consistent.
#[test]
fn removing_a_file_frees_its_clusters() {
    let image = mkfs_image("write-remove.img", 272, 4);
    let mut volume = FileSystem::mount(FileImage::open_rw(&image).expect("open")).expect("mount");

    let free_at_start = volume.fat().free_clusters();
    volume
        .write_file("/GONE.BIN", &expected_content(50_000))
        .expect("write");
    assert!(volume.fat().free_clusters() < free_at_start);

    volume.remove("/GONE.BIN").expect("remove");
    volume.sync().expect("sync");

    assert_eq!(
        volume.fat().free_clusters(),
        free_at_start,
        "every cluster should have come back"
    );
    assert!(volume.open("/GONE.BIN").is_err(), "the file should be gone");
    drop(volume);

    assert_fsck_clean(&image);
    assert!(
        !mdir(&image, "/").contains("GONE"),
        "mtools still lists the deleted file"
    );
}

/// Filling the volume reports out of space rather than corrupting it, and
/// what is already there survives.
#[test]
fn filling_the_volume_reports_out_of_space() {
    let image = mkfs_image("write-full.img", 16, 4);
    let mut volume = FileSystem::mount(FileImage::open_rw(&image).expect("open")).expect("mount");

    let chunk = expected_content(64 * 1024);
    let mut written = 0usize;
    let mut ran_out = false;
    for n in 0..1000 {
        match volume.write_file(&format!("/P{n}.BIN"), &chunk) {
            Ok(_) => written += 1,
            Err(Error::Fat(FatError::DiskFull { .. })) | Err(Error::DirectoryFull) => {
                ran_out = true;
                break;
            }
            Err(other) => panic!("unexpected error: {other:?}"),
        }
    }
    volume.sync().expect("sync");
    drop(volume);

    assert!(ran_out, "the volume should have filled");
    assert!(written > 0, "nothing was written at all");
    assert_fsck_clean(&image);

    // Everything that did get written is still readable and correct.
    let mut volume = FileSystem::mount(FileImage::open(&image).expect("open")).expect("remount");
    for n in 0..written {
        let file = volume.open(&format!("/P{n}.BIN")).expect("open");
        assert_eq!(volume.read_all(&file).expect("read"), chunk, "P{n} differs");
    }
}

/// A directory grows past one cluster, and every name in it survives.
#[test]
fn a_directory_grows_beyond_one_cluster() {
    let image = mkfs_image("write-bigdir.img", 272, 4);
    let mut volume = FileSystem::mount(FileImage::open_rw(&image).expect("open")).expect("mount");

    // 4 KB clusters hold 128 entries, so this needs several clusters.
    let count = 400;
    for n in 0..count {
        volume
            .write_file(&format!("/D{n:04}.BIN"), format!("{n}\n").as_bytes())
            .unwrap_or_else(|e| panic!("write {n}: {e:?}"));
    }
    volume.sync().expect("sync");

    let root = volume.root_dir().expect("root");
    let listed = root.iter().filter(|e| e.name().starts_with('D')).count();
    assert_eq!(listed, count, "entries went missing as the directory grew");
    drop(volume);

    assert_fsck_clean(&image);
    let listing = mdir(&image, "/");
    for n in [0, 127, 128, 255, 256, count - 1] {
        assert!(
            listing.contains(&format!("D{n:04}")),
            "mtools cannot see D{n:04}"
        );
    }
}

/// Overwriting part of a file leaves the rest alone.
#[test]
fn a_partial_overwrite_leaves_the_rest_alone() {
    let image = mkfs_image("write-partial.img", 272, 4);
    let mut volume = FileSystem::mount(FileImage::open_rw(&image).expect("open")).expect("mount");

    let mut expected = expected_content(30_000);
    let mut file = volume.write_file("/P.BIN", &expected).expect("write");

    // Deliberately unaligned at both ends, so both scratch paths run.
    let patch: Vec<u8> = (0..1234u32).map(|n| (n % 7) as u8 + 1).collect();
    let at = 777usize;
    volume
        .write_at(&mut file, at as u64, &patch)
        .expect("overwrite");
    expected[at..at + patch.len()].copy_from_slice(&patch);
    volume.sync().expect("sync");
    drop(volume);

    assert_fsck_clean(&image);
    assert_eq!(mcopy_read(&image, "/P.BIN"), expected);
}

/// A write past the end grows the file and records the new length.
#[test]
fn writing_past_the_end_grows_the_file() {
    let image = mkfs_image("write-grow.img", 272, 4);
    let mut volume = FileSystem::mount(FileImage::open_rw(&image).expect("open")).expect("mount");

    let mut file = volume.create("/G.BIN").expect("create");
    assert_eq!(file.len(), 0);

    let tail = expected_content(3000);
    volume.write_at(&mut file, 10_000, &tail).expect("write");
    assert_eq!(file.len(), 13_000);
    volume.sync().expect("sync");
    drop(volume);

    assert_fsck_clean(&image);
    let back = mcopy_read(&image, "/G.BIN");
    assert_eq!(back.len(), 13_000);
    assert_eq!(&back[10_000..], &tail[..], "the written tail differs");
}

/// A name with no storable form at all is refused; one that merely does not
/// fit 8.3 is not.
///
/// The line is whether some name really can be written. A reserved
/// character has no long-name form either, so there is nothing to store. A
/// name that is too long, or has a space in it, stores perfectly well as a
/// long name — refusing those would be refusing most of the names anyone
/// actually uses.
#[test]
fn names_with_no_storable_form_are_refused() {
    let image = mkfs_image("write-names.img", 16, 4);
    let mut volume = FileSystem::mount(FileImage::open_rw(&image).expect("open")).expect("mount");

    for name in ["/bad*char.txt", "/who?.txt", "/a<b.txt", "/trailing."] {
        match volume.create(name) {
            Err(Error::BadName { .. }) => {}
            other => panic!("{name} should have been refused, got {other:?}"),
        }
    }
    for name in [
        "/toolongforeight.txt",
        "/GOOD.LONGEXT",
        "/has space.txt",
        "/two.dots.txt",
    ] {
        volume
            .create(name)
            .unwrap_or_else(|e| panic!("{name} has a perfectly good long-name form, but: {e:?}"));
    }

    // Lower case is accepted and stored upper case, which is what 8.3 is.
    let file = volume.create("/ok.txt").expect("lower case is fine");
    assert_eq!(file.len(), 0);
    assert!(
        volume.open("/OK.TXT").is_ok(),
        "should be findable upper case"
    );

    // And a name already taken is refused rather than duplicated.
    match volume.create("/OK.TXT") {
        Err(Error::AlreadyExists { .. }) => {}
        other => panic!("expected AlreadyExists, got {other:?}"),
    }

    volume.sync().expect("sync");
    drop(volume);
    assert_fsck_clean(&image);
}

// ---------------------------------------------------------------------------
// A length never covers bytes nothing wrote
// ---------------------------------------------------------------------------

/// Nothing a file's length covers is a deleted file's data.
///
/// FAT frees a chain without erasing it, so every cluster this crate hands
/// to a new file still holds whatever the last one left there. The three
/// ways a length can come to cover unwritten bytes are a sized create, a
/// write starting past the end, and a growing truncate — and all three have
/// to answer with zeros rather than with the previous owner's data.
///
/// The pattern is written, synced and deleted first, so the clusters really
/// do hold it when the allocator hands them back. `0xAB` because zero would
/// prove nothing: on a fresh image the answer is zeros either way.
#[test]
fn a_length_never_publishes_a_deleted_files_bytes() {
    const SECRET: u8 = 0xAB;
    let planted = vec![SECRET; 64 * 1024];

    type Exercise = dyn Fn(&mut FileSystem<FileImage>, u32) -> resident_fat::File;

    for (slug, what, exercise) in [
        (
            "create",
            "a sized create, reserved but never written",
            &(|volume: &mut FileSystem<FileImage>, size: u32| {
                volume.create_with_size("/NEW.BIN", size).expect("create")
            }) as &Exercise,
        ),
        (
            "gap",
            "a write starting past the end",
            &|volume: &mut FileSystem<FileImage>, size: u32| {
                let mut file = volume.create("/NEW.BIN").expect("create");
                // The last four bytes, so everything before them is gap.
                volume
                    .write_at(&mut file, u64::from(size) - 4, b"tail")
                    .expect("write");
                file
            },
        ),
        (
            "truncate",
            "a growing truncate",
            &|volume: &mut FileSystem<FileImage>, size: u32| {
                let mut file = volume.create("/NEW.BIN").expect("create");
                volume.truncate(&mut file, size).expect("truncate");
                file
            },
        ),
    ] {
        let image = mkfs_image(&format!("write-nostale-{slug}.img"), 32, 4);

        let mut volume =
            FileSystem::mount(FileImage::open_rw(&image).expect("open")).expect("mount");
        volume.write_file("/PLANTED.BIN", &planted).expect("plant");
        volume.sync().expect("sync");
        volume.remove("/PLANTED.BIN").expect("remove");
        volume.sync().expect("sync");

        let file = exercise(&mut volume, planted.len() as u32);
        let visible = volume.read_all(&file).expect("read");
        volume.unmount().expect("unmount");

        let leaked = visible.iter().filter(|&&byte| byte == SECRET).count();
        assert_eq!(
            leaked,
            0,
            "{what}: {leaked} of the {} bytes this file claims are the deleted file's",
            visible.len()
        );

        // And the same question asked of an implementation that is not ours.
        if visible.is_empty() {
            // The reserved-and-never-written case, where `fsck` reports a
            // file holding clusters its length does not account for. That is
            // the reservation, and `fsck`'s own repair for it — truncate to
            // zero — is exactly the right one. Wasted space, not damage.
            assert_no_corruption(&image, what);
        } else {
            assert_fsck_clean(&image);
            assert_eq!(
                mcopy_read(&image, "/NEW.BIN"),
                visible,
                "{what}: mtools disagrees about what the file holds"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Injected failures: leak, never cross-link
// ---------------------------------------------------------------------------

/// An interrupted create never cross-links, and never damages what was
/// already on the volume.
///
/// This is the ordering rules being tested rather than merely stated: a rule
/// nothing has interrupted is a rule nothing has tested. At every failure
/// point the operation may have been lost and clusters may have leaked —
/// both acceptable — but no cluster may end up belonging to two files, and
/// the file that was there before must still read back byte for byte.
#[test]
fn an_interrupted_create_never_cross_links() {
    let bystander = expected_content(20_000);

    for fail_after in 0..14 {
        let image = mkfs_image(&format!("fail-create-{fail_after}.img"), 272, 4);
        {
            let mut volume =
                FileSystem::mount(FileImage::open_rw(&image).expect("open")).expect("mount");
            volume.write_file("/OLD.BIN", &bystander).expect("setup");
            volume.sync().expect("sync");
        }

        let device = FailAfter::new(FileImage::open_rw(&image).expect("open"), fail_after);
        let mut volume = FileSystem::mount(device).expect("mount");
        // The outcome is not the point; how it failed is.
        let _ = volume.write_file("/NEW.BIN", &expected_content(30_000));
        let _ = volume.sync();
        drop(volume);

        let what = format!("create interrupted after {fail_after} writes");
        assert_no_corruption(&image, &what);
        assert_intact(&image, "/OLD.BIN", &bystander, &what);
    }
}

/// The same for truncation, which is where shrink-then-free earns its
/// place.
#[test]
fn an_interrupted_truncation_never_cross_links() {
    let bystander = expected_content(20_000);

    for fail_after in 0..12 {
        let image = mkfs_image(&format!("fail-trunc-{fail_after}.img"), 272, 4);
        {
            let mut volume =
                FileSystem::mount(FileImage::open_rw(&image).expect("open")).expect("mount");
            volume.write_file("/OLD.BIN", &bystander).expect("setup");
            volume
                .write_file("/T.BIN", &expected_content(60_000))
                .expect("setup");
            volume.sync().expect("sync");
        }

        let device = FailAfter::new(FileImage::open_rw(&image).expect("open"), fail_after);
        let mut volume = FileSystem::mount(device).expect("mount");
        if let Ok(mut file) = volume.open("/T.BIN") {
            let _ = volume.truncate(&mut file, 4000);
        }
        let _ = volume.sync();
        drop(volume);

        let what = format!("truncation interrupted after {fail_after} writes");
        assert_no_corruption(&image, &what);
        assert_intact(&image, "/OLD.BIN", &bystander, &what);
    }
}

/// And for extending an existing file.
#[test]
fn an_interrupted_extend_never_cross_links() {
    let bystander = expected_content(20_000);

    for fail_after in 0..12 {
        let image = mkfs_image(&format!("fail-extend-{fail_after}.img"), 272, 4);
        {
            let mut volume =
                FileSystem::mount(FileImage::open_rw(&image).expect("open")).expect("mount");
            volume.write_file("/OLD.BIN", &bystander).expect("setup");
            volume
                .write_file("/X.BIN", &expected_content(5000))
                .expect("setup");
            volume.sync().expect("sync");
        }

        let device = FailAfter::new(FileImage::open_rw(&image).expect("open"), fail_after);
        let mut volume = FileSystem::mount(device).expect("mount");
        if let Ok(mut file) = volume.open("/X.BIN") {
            let _ = volume.write_at(&mut file, 5000, &expected_content(50_000));
        }
        let _ = volume.sync();
        drop(volume);

        let what = format!("extend interrupted after {fail_after} writes");
        assert_no_corruption(&image, &what);
        assert_intact(&image, "/OLD.BIN", &bystander, &what);
    }
}

/// A failed allocation gives back what it took.
///
/// The error path, not the power-loss path: a call that reports failure
/// should not have consumed space, because nothing names it and nothing
/// ever will.
#[test]
fn a_refused_allocation_leaks_nothing() {
    let image = mkfs_image("write-noleak.img", 16, 4);
    let mut volume = FileSystem::mount(FileImage::open_rw(&image).expect("open")).expect("mount");

    let free = volume.fat().free_clusters();
    let far_too_much = vec![0u8; (free as usize + 100) * 4096];
    match volume.write_file("/HUGE.BIN", &far_too_much) {
        Err(Error::Fat(FatError::DiskFull { .. })) => {}
        other => panic!("expected DiskFull, got {other:?}"),
    }
    assert_eq!(
        volume.fat().free_clusters(),
        free,
        "a refused allocation kept some clusters"
    );
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Writes a file into the scratch directory and returns its path.
fn host_file(name: &str, contents: &[u8]) -> std::path::PathBuf {
    let path = scratch_dir().join(name);
    std::fs::write(&path, contents).expect("write");
    path
}

/// Reads a file out of an image with `mtools`, as bytes.
fn mcopy_read(image: &std::path::Path, path: &str) -> Vec<u8> {
    let out = scratch_dir().join(format!(
        "out-{}",
        path.trim_start_matches('/').replace('/', "_")
    ));
    let _ = std::fs::remove_file(&out);
    mcopy_out(image, path, &out);
    std::fs::read(&out).expect("read what mtools produced")
}

/// `fsck` may report wasted space, but never shared or dangling data.
fn assert_no_corruption(image: &std::path::Path, what: &str) {
    let report = fsck(image);
    if let Some(phrase) = report.indicates_corruption() {
        panic!(
            "{what}: fsck.vfat found corruption ({phrase:?}), not merely a leak:\n{}",
            report.output
        );
    }
}

/// A file that existed before an interrupted operation still reads back
/// exactly, through this crate.
///
/// The property that matters most in practice: whatever the interruption
/// cost, it must not have cost somebody else's data.
fn assert_intact(image: &std::path::Path, path: &str, expected: &[u8], what: &str) {
    let mut volume = FileSystem::mount(FileImage::open(image).expect("open"))
        .unwrap_or_else(|e| panic!("{what}: the volume no longer mounts: {e:?}"));
    let file = volume
        .open(path)
        .unwrap_or_else(|e| panic!("{what}: {path} disappeared: {e:?}"));
    let data = volume
        .read_all(&file)
        .unwrap_or_else(|e| panic!("{what}: {path} no longer reads: {e:?}"));
    assert_eq!(data, expected, "{what}: {path} was damaged");
}

// ---------------------------------------------------------------------------
// A stale handle cannot reach another file
// ---------------------------------------------------------------------------

/// A handle kept across a delete is refused, rather than rewriting whatever
/// took its directory slot.
///
/// The damage guarded against is not to the deleted file — that one is gone
/// and nothing can be done for it. It is to the *next* file created in the
/// directory, which is handed the freed slot and the freed clusters: writing
/// through the old handle rewrote that entry's length and starting cluster,
/// so a five-thousand-byte file silently became an eight-byte one holding
/// different bytes. Nothing reported an error, and `fsck` saw only a chain
/// longer than its size.
///
/// The recreated file deliberately takes a *different* name, because that is
/// the case the check can distinguish and the one that matters: a handle for
/// one file must never reach another. Same-name recreation is covered below.
#[test]
fn a_handle_kept_across_a_delete_cannot_rewrite_the_next_file() {
    let image = mkfs_image("write-stale.img", 32, 4);
    let mut volume = FileSystem::mount(FileImage::open_rw(&image).expect("open")).expect("mount");

    // Small, so the write below would extend it and so rewrite the length.
    volume.write_file("/GOING.BIN", &[1u8; 4]).expect("create");
    let mut stale = volume.open("/GOING.BIN").expect("open");
    volume.remove("/GOING.BIN").expect("remove");

    // Takes the freed slot and the freed clusters.
    let payload = expected_content(5000);
    let victim = volume.write_file("/VICTIM.BIN", &payload).expect("create");
    assert_eq!(
        victim.first_cluster(),
        stale.first_cluster(),
        "the test only bites if the new file really reuses the clusters"
    );

    match volume.write_at(&mut stale, 0, &[9u8; 8]) {
        Err(Error::StaleFile { name }) => assert_eq!(name, "GOING.BIN"),
        other => panic!("expected StaleFile, got {other:?}"),
    }
    match volume.truncate(&mut stale, 0) {
        Err(Error::StaleFile { .. }) => {}
        other => panic!("truncate through a stale handle: expected StaleFile, got {other:?}"),
    }
    // And reading, which would otherwise hand back the other file's bytes.
    match volume.read_all(&stale) {
        Err(Error::StaleFile { .. }) => {}
        other => panic!("read through a stale handle: expected StaleFile, got {other:?}"),
    }

    // The bystander is untouched, by its own handle and by name.
    assert_eq!(volume.read_all(&victim).expect("read victim"), payload);
    let reopened = volume.open("/VICTIM.BIN").expect("reopen victim");
    assert_eq!(reopened.len(), 5000, "the victim's length was rewritten");
    assert_eq!(volume.read_all(&reopened).expect("read"), payload);

    volume.sync().expect("sync");
    drop(volume);
    assert_fsck_clean(&image);
}

/// The check costs no device call, which is what lets it be unconditional.
///
/// It compares against the parsed directory, which is resident and was read
/// to produce the handle in the first place. Pinned because the obvious
/// implementation — read the entry's block back and compare — would be
/// correct and would silently add a transfer to every read, turning this
/// crate's one-call contiguous read into two.
#[test]
fn the_staleness_check_touches_no_device() {
    let image = mkfs_image("write-stale-cost.img", 32, 4);
    let payload = expected_content(64 * 1024);
    let device = Recorder::new(FileImage::open_rw(&image).expect("open"));
    let mut volume = FileSystem::mount(device).expect("mount");

    volume.write_file("/BIG.BIN", &payload).expect("create");
    let file = volume.open("/BIG.BIN").expect("open");

    volume.device_mut().reset();
    let data = volume.read_all(&file).expect("read");
    assert_eq!(data, payload);

    let reads = volume.device_mut().reads();
    assert_eq!(
        reads.len(),
        1,
        "a contiguous read should still be one call, got {reads:?}"
    );
}

/// Replacing a file under the same name invalidates a handle to the old one.
///
/// The case a name comparison alone cannot see, and the reason the check
/// also counts deletions: `write_file` over an existing path removes the
/// entry and creates another with the same eleven name bytes, so the slot
/// looks untouched while being a different file. FAT stores no inode, no
/// generation and no creation cookie to tell them apart, which leaves the
/// fact that a deletion happened as the only thing to notice.
///
/// Left undetected this is not cosmetic. Writing through the old handle
/// rewrote the new file's length while its chain kept the old one's, and
/// `fsck.vfat` reported a file whose size does not account for its cluster
/// chain — so the volume, not just the handle, came out wrong.
#[test]
fn replacing_a_file_invalidates_a_handle_to_the_old_one() {
    let image = mkfs_image("write-stale-samename.img", 32, 4);
    let mut volume = FileSystem::mount(FileImage::open_rw(&image).expect("open")).expect("mount");

    volume.write_file("/SAME.BIN", &[1u8; 4]).expect("create");
    let mut stale = volume.open("/SAME.BIN").expect("open");

    let payload = expected_content(5000);
    volume.write_file("/SAME.BIN", &payload).expect("replace");

    match volume.write_at(&mut stale, 0, &[9u8; 8]) {
        Err(Error::StaleFile { name }) => assert_eq!(name, "SAME.BIN"),
        other => panic!("expected StaleFile, got {other:?}"),
    }

    // The replacement is intact, and the handle `write_file` itself returned
    // is not caught by its own deletion — it was stamped after the removal.
    let reopened = volume.open("/SAME.BIN").expect("reopen");
    assert_eq!(reopened.len(), 5000);
    assert_eq!(volume.read_all(&reopened).expect("read"), payload);

    volume.sync().expect("sync");
    drop(volume);
    assert_fsck_clean(&image);
}

/// Deleting a sibling invalidates a handle, and that is the accepted cost.
///
/// A false positive: the handle's own entry is untouched. Counting deletions
/// per directory rather than per slot is what makes same-name replacement
/// detectable at all, and the price is this. Pinned so the trade is visible
/// and so nobody narrows the count to a single slot without noticing what it
/// gives up.
#[test]
fn deleting_a_sibling_invalidates_a_handle_and_reopening_fixes_it() {
    let image = mkfs_image("write-stale-sibling.img", 32, 4);
    let mut volume = FileSystem::mount(FileImage::open_rw(&image).expect("open")).expect("mount");

    let payload = expected_content(4096);
    volume.write_file("/KEEP.BIN", &payload).expect("keep");
    volume.write_file("/GO.BIN", &[7u8; 16]).expect("go");

    let mut keep = volume.open("/KEEP.BIN").expect("open keep");
    volume.remove("/GO.BIN").expect("remove sibling");

    match volume.write_at(&mut keep, 0, &[1u8; 32]) {
        Err(Error::StaleFile { .. }) => {}
        other => panic!("expected StaleFile for a sibling deletion, got {other:?}"),
    }

    // Reopening is the whole remedy, and the file was never damaged.
    let mut keep = volume.open("/KEEP.BIN").expect("reopen");
    assert_eq!(volume.read_all(&keep).expect("read"), payload);
    volume.write_at(&mut keep, 0, &[1u8; 32]).expect("write");

    volume.sync().expect("sync");
    drop(volume);
    assert_fsck_clean(&image);
}

/// Filling the volume leaves a free-space hint another implementation will
/// accept.
///
/// `next_free` is written into the FS Information Sector on sync, and the
/// format wants a cluster number. An allocation that reaches the last cluster
/// leaves the search position one *past* it, which is not one — so the hint
/// is wrapped back to the first cluster instead of published out of range.
///
/// Checked by remounting rather than by reading the field directly: this
/// crate discards a hint outside the volume when it parses one, so an
/// out-of-range value written here comes back as `None`. That makes the
/// round trip the assertion, and it is also the path another implementation
/// would take.
#[test]
fn a_full_volume_leaves_a_usable_free_space_hint() {
    let image = mkfs_image("write-hint.img", 16, 4);
    let mut volume = FileSystem::mount(FileImage::open_rw(&image).expect("open")).expect("mount");

    // Every cluster the volume has, so the last allocation ends at its end.
    let free = volume.fat().free_clusters();
    let everything = vec![0u8; free as usize * 4096];
    volume.write_file("/FULL.BIN", &everything).expect("fill");
    assert_eq!(volume.fat().free_clusters(), 0, "the volume should be full");
    volume.unmount().expect("unmount");

    let volume = FileSystem::mount(FileImage::open_rw(&image).expect("open")).expect("remount");
    let hint = volume.fs_info().next_free;
    assert!(
        hint.is_some(),
        "the hint was discarded on remount, so an out-of-range cluster was written"
    );
    assert!(
        volume.fat().is_valid_cluster(hint.expect("checked")),
        "the hint names a cluster the volume does not have: {hint:?}"
    );

    drop(volume);
    assert_fsck_clean(&image);
}
