//! Renaming and moving.
//!
//! Every case is checked against `fsck.vfat` and read back through
//! `mtools`, as the other write tests are. The interrupted cases are the
//! point of the ordering `rename` documents: FAT has no atomic rename, so
//! what matters is what an interruption at each write leaves behind — and
//! for replacing a file, that the name being saved over always names a
//! whole file.

mod support;

use resident_fat::{Error, FileSystem};
use support::*;

/// A fresh volume holding `files`, synced and closed.
fn volume_with(name: &str, dirs: &[&str], files: &[(&str, &[u8])]) -> std::path::PathBuf {
    let image = mkfs_image(name, 272, 4);
    let mut volume = FileSystem::mount(FileImage::open_rw(&image).expect("open")).expect("mount");
    for dir in dirs {
        volume.create_dir(dir).expect("create dir");
    }
    for (path, data) in files {
        volume.write_file(path, data).expect("write");
    }
    volume.sync().expect("sync");
    image
}

/// The volume on `image`, writable.
fn mount_rw(image: &std::path::Path) -> FileSystem<FileImage> {
    FileSystem::mount(FileImage::open_rw(image).expect("open")).expect("mount")
}

/// `path`'s contents through this crate, or `None` if it is not there.
fn read(image: &std::path::Path, path: &str) -> Option<Vec<u8>> {
    let mut volume = FileSystem::mount(FileImage::open(image).expect("open")).expect("remount");
    let file = volume.open(path).ok()?;
    Some(volume.read_all(&file).expect("read"))
}

/// Reads a file out of an image with `mtools`, as bytes.
fn mcopy_read(image: &std::path::Path, path: &str) -> Vec<u8> {
    let out = scratch_dir().join(format!(
        "rename-out-{}-{}",
        image.file_name().unwrap().to_string_lossy(),
        path.trim_start_matches('/').replace(['/', ' '], "_")
    ));
    let _ = std::fs::remove_file(&out);
    mcopy_out(image, path, &out);
    std::fs::read(&out).expect("read what mtools produced")
}

/// Asserts `fsck` finds nothing worse than leaked space.
fn assert_no_corruption(image: &std::path::Path, what: &str) {
    let report = fsck(image);
    if let Some(phrase) = report.indicates_corruption() {
        panic!("{what}: fsck.vfat found {phrase:?}:\n{}", report.output);
    }
}

// ---------------------------------------------------------------------------
// What each case produces
// ---------------------------------------------------------------------------

/// A short name to another short name: the data follows, the old name is
/// gone, and both `fsck` and `mtools` agree.
#[test]
fn a_file_is_renamed_in_its_directory() {
    let data = expected_content(10_000);
    let image = volume_with("rename-simple.img", &[], &[("/A.BIN", &data)]);

    let mut volume = mount_rw(&image);
    volume.rename("/A.BIN", "/B.BIN").expect("rename");
    volume.sync().expect("sync");
    drop(volume);

    assert_fsck_clean(&image);
    assert_eq!(mcopy_read(&image, "/B.BIN"), data);
    assert_eq!(read(&image, "/B.BIN").as_deref(), Some(&data[..]));
    assert_eq!(read(&image, "/A.BIN"), None, "the old name is gone");
}

/// Long names both ways: a longer one than the slots the file has, which
/// cannot be done in place, and back to a shorter one, which is done in
/// place and has to delete the slots the shorter name leaves over.
#[test]
fn long_names_grow_and_shrink() {
    let data = expected_content(5_000);
    let image = volume_with("rename-long.img", &[], &[("/settings.new", &data)]);

    let mut volume = mount_rw(&image);
    let long = "/a much longer name than it had before.toml";
    volume.rename("/settings.new", long).expect("grow");
    volume.sync().expect("sync");
    drop(volume);
    assert_fsck_clean(&image);
    assert_eq!(mcopy_read(&image, long), data);

    let mut volume = mount_rw(&image);
    volume.rename(long, "/short.txt").expect("shrink");
    volume.sync().expect("sync");
    drop(volume);
    assert_fsck_clean(&image);
    assert_eq!(mcopy_read(&image, "/short.txt"), data);
    let names: Vec<_> = mdir_entries(&image, "/").into_iter().collect();
    assert_eq!(names.len(), 1, "one entry, no orphaned slots: {names:?}");
}

/// A change of case alone is a rename — including one that needs a long
/// name where the old one fitted 8.3 — and an identical name does nothing.
#[test]
fn a_change_of_case_is_a_rename() {
    let data = expected_content(700);
    let image = volume_with("rename-case.img", &[], &[("/readme.txt", &data)]);

    let mut volume = mount_rw(&image);
    volume
        .rename("/readme.txt", "/ReadMe.txt")
        .expect("mixed case");
    assert_eq!(volume.open_dir("/").expect("root").iter().count(), 1);
    assert_eq!(
        volume
            .open_dir("/")
            .expect("root")
            .iter()
            .next()
            .unwrap()
            .name(),
        "ReadMe.txt"
    );
    volume
        .rename("/ReadMe.txt", "/README.TXT")
        .expect("upper case");
    volume
        .rename("/README.TXT", "/README.TXT")
        .expect("no change");
    volume.sync().expect("sync");
    drop(volume);

    assert_fsck_clean(&image);
    let entries = mdir_entries(&image, "/");
    assert_eq!(entries.len(), 1, "{entries:?}");
    assert_eq!(mcopy_read(&image, "/README.TXT"), data);
}

/// Over an existing file: the target takes the new contents, the source
/// goes, and the target's old clusters are freed rather than leaked.
#[test]
fn a_file_replaces_a_file() {
    let old = expected_content(40_000);
    let new = expected_content(9_000);
    let image = volume_with(
        "rename-replace.img",
        &[],
        &[("/settings.toml", &old), ("/settings.new", &new)],
    );

    let mut volume = mount_rw(&image);
    let free_before = volume.fat().free_clusters();
    volume
        .rename("/settings.new", "/settings.toml")
        .expect("replace");
    let freed = volume.fat().free_clusters() - free_before;
    assert_eq!(
        freed,
        40_000u32.div_ceil(4096),
        "the old contents are freed"
    );
    volume.sync().expect("sync");
    drop(volume);

    assert_fsck_clean(&image);
    assert_eq!(mcopy_read(&image, "/settings.toml"), new);
    assert_eq!(read(&image, "/settings.new"), None);
}

/// To another directory and back, with `mtools` reading it at each end.
#[test]
fn a_file_moves_between_directories() {
    let data = expected_content(12_345);
    let image = volume_with("rename-move.img", &["/SUB"], &[("/A.BIN", &data)]);

    let mut volume = mount_rw(&image);
    volume
        .rename("/A.BIN", "/SUB/moved here.bin")
        .expect("move in");
    volume.sync().expect("sync");
    drop(volume);
    assert_fsck_clean(&image);
    assert_eq!(mcopy_read(&image, "/SUB/moved here.bin"), data);
    assert_eq!(read(&image, "/A.BIN"), None);

    let mut volume = mount_rw(&image);
    volume
        .rename("/SUB/moved here.bin", "/BACK.BIN")
        .expect("move out");
    volume.sync().expect("sync");
    drop(volume);
    assert_fsck_clean(&image);
    assert_eq!(mcopy_read(&image, "/BACK.BIN"), data);
}

/// A directory moved to another parent keeps its contents and has its `..`
/// pointed at the new parent — which `fsck` checks.
#[test]
fn a_directory_moves_with_its_contents() {
    let data = expected_content(3_000);
    let image = volume_with(
        "rename-dir.img",
        &["/D1", "/D2", "/D2/DEEP"],
        &[("/D1/INNER.TXT", &data)],
    );

    let mut volume = mount_rw(&image);
    volume.rename("/D1", "/D2/DEEP/D1").expect("move directory");
    volume.sync().expect("sync");
    drop(volume);
    assert_fsck_clean(&image);
    assert_eq!(mcopy_read(&image, "/D2/DEEP/D1/INNER.TXT"), data);

    // `..` leads back to the new parent, and on up to the root.
    let mut volume = mount_rw(&image);
    let file = volume
        .open("/D2/DEEP/D1/../../DEEP/D1/INNER.TXT")
        .expect("through ..");
    assert_eq!(volume.read_all(&file).expect("read"), data);

    // And back to the root, where `..` holds 0.
    volume
        .rename("/D2/DEEP/D1", "/D1")
        .expect("back to the root");
    volume.sync().expect("sync");
    drop(volume);
    assert_fsck_clean(&image);
    assert_eq!(mcopy_read(&image, "/D1/INNER.TXT"), data);
}

#[test]
fn a_directory_cannot_move_inside_itself() {
    let image = volume_with("rename-self.img", &["/D1", "/D1/SUB"], &[]);
    let mut volume = mount_rw(&image);
    for to in ["/D1/D1", "/D1/SUB/D1"] {
        match volume.rename("/D1", to) {
            Err(Error::MoveIntoItself { .. }) => {}
            other => panic!("{to}: expected MoveIntoItself, got {other:?}"),
        }
    }
    drop(volume);
    assert_fsck_clean(&image);
}

#[test]
fn what_is_refused() {
    let image = volume_with("rename-refused.img", &["/DIR"], &[("/F.BIN", b"f")]);
    let mut volume = mount_rw(&image);

    match volume.rename("/NOPE.BIN", "/X.BIN") {
        Err(Error::NotFound { .. }) => {}
        other => panic!("missing source: {other:?}"),
    }
    match volume.rename("/F.BIN", "/DIR") {
        Err(Error::AlreadyExists { .. }) => {}
        other => panic!("file over a directory: {other:?}"),
    }
    match volume.rename("/DIR", "/F.BIN") {
        Err(Error::AlreadyExists { .. }) => {}
        other => panic!("directory over a file: {other:?}"),
    }
    match volume.rename("/F.BIN", "/NO/SUCH/DIR.BIN") {
        Err(Error::NotFound { .. }) => {}
        other => panic!("missing destination directory: {other:?}"),
    }
    for bad in ["/F.BIN/", "/DIR/.."] {
        match volume.rename("/F.BIN", bad) {
            Err(Error::BadName { .. }) => {}
            other => panic!("{bad:?}: {other:?}"),
        }
    }
    drop(volume);
    assert_fsck_clean(&image);
}

/// A handle to the renamed file, and one to the file it replaced, are both
/// refused rather than written through.
#[test]
fn handles_do_not_survive_a_rename() {
    let image = volume_with(
        "rename-handles.img",
        &[],
        &[("/OLD.BIN", &expected_content(9000)), ("/NEW.BIN", b"new")],
    );
    let mut volume = mount_rw(&image);
    let mut renamed = volume.open("/NEW.BIN").expect("open");
    let mut replaced = volume.open("/OLD.BIN").expect("open");
    volume.rename("/NEW.BIN", "/OLD.BIN").expect("replace");

    for (what, handle) in [("renamed", &mut renamed), ("replaced", &mut replaced)] {
        match volume.write_at(handle, 0, b"x") {
            Err(Error::StaleFile { .. }) => {}
            other => panic!("{what}: expected StaleFile, got {other:?}"),
        }
    }
    volume.sync().expect("sync");
    drop(volume);
    assert_fsck_clean(&image);
    assert_eq!(read(&image, "/OLD.BIN").as_deref(), Some(&b"new"[..]));
}

// ---------------------------------------------------------------------------
// Interrupted
// ---------------------------------------------------------------------------

/// Replacing a file, interrupted after every possible write: the name
/// being saved over reads back as exactly the old file or exactly the new
/// one, and no cluster ever belongs to two names.
///
/// The property the replace ordering exists for — write the new settings
/// beside the old, rename over, and a power cut at any moment leaves one
/// whole copy under the name the board reads.
#[test]
fn an_interrupted_replace_always_leaves_a_whole_file() {
    let old = expected_content(20_000);
    let new = expected_content(7_000);
    let (mut saw_old, mut saw_new) = (false, false);

    for fail_after in 0..MAX_WRITES {
        let image = volume_with(
            &format!("rename-fail-replace-{fail_after}.img"),
            &[],
            &[("/settings.toml", &old), ("/settings.new", &new)],
        );
        let device = FailAfter::new(FileImage::open_rw(&image).expect("open"), fail_after);
        let mut volume = FileSystem::mount(device).expect("mount");
        let finished =
            volume.rename("/settings.new", "/settings.toml").is_ok() && volume.sync().is_ok();
        drop(volume);

        let what = format!("replace interrupted after {fail_after} writes");
        assert_no_corruption(&image, &what);
        let found = read(&image, "/settings.toml")
            .unwrap_or_else(|| panic!("{what}: settings.toml is missing"));
        assert!(
            found == old || found == new,
            "{what}: settings.toml is neither file ({} bytes)",
            found.len()
        );
        saw_old |= found == old;
        saw_new |= found == new;
        if finished {
            // Both sides of the repoint were reached, so the window
            // between them was interrupted rather than skipped.
            assert!(saw_old && saw_new, "only one outcome was ever produced");
            return;
        }
    }
    panic!("the replace never completed in {MAX_WRITES} writes");
}

/// More writes than any rename here takes, so each interrupted test is
/// sure to reach the run where nothing fails — which it checks, since a
/// loop that stopped short would test only the early failures.
const MAX_WRITES: u32 = 40;

/// A rename in place, interrupted after every possible write: the file is
/// still in its directory, whole, under some name — the old one, the new
/// one, or its 8.3 alias — and nothing is shared.
#[test]
fn an_interrupted_rename_in_place_loses_nothing() {
    let data = expected_content(6_000);

    for fail_after in 0..MAX_WRITES {
        let image = volume_with(
            &format!("rename-fail-here-{fail_after}.img"),
            &[],
            &[("/a rather long name.bin", &data)],
        );
        let device = FailAfter::new(FileImage::open_rw(&image).expect("open"), fail_after);
        let mut volume = FileSystem::mount(device).expect("mount");
        let finished = volume
            .rename("/a rather long name.bin", "/short.bin")
            .is_ok()
            && volume.sync().is_ok();
        drop(volume);

        let what = format!("rename interrupted after {fail_after} writes");
        assert_no_corruption(&image, &what);
        let mut volume = FileSystem::mount(FileImage::open(&image).expect("open")).expect("mount");
        let names: Vec<String> = volume
            .open_dir("/")
            .expect("root")
            .iter()
            .map(|entry| entry.name().to_string())
            .collect();
        assert_eq!(names.len(), 1, "{what}: {names:?}");
        let file = volume.open(&names[0]).expect("open");
        assert_eq!(volume.read_all(&file).expect("read"), data, "{what}");
        if finished {
            assert_eq!(names[0], "short.bin", "{what}");
            return;
        }
    }
    panic!("the rename never completed in {MAX_WRITES} writes");
}

/// A move to another directory, interrupted after every possible write:
/// the file is never lost — it reads back under its old path or its new
/// one. This is the one case with a window where both names share its
/// chain (see `rename`), so that is the one thing `fsck` may report.
#[test]
fn an_interrupted_move_never_loses_the_file() {
    let data = expected_content(6_000);

    for fail_after in 0..MAX_WRITES {
        let image = volume_with(
            &format!("rename-fail-move-{fail_after}.img"),
            &["/SUB"],
            &[("/A.BIN", &data)],
        );
        let device = FailAfter::new(FileImage::open_rw(&image).expect("open"), fail_after);
        let mut volume = FileSystem::mount(device).expect("mount");
        let finished = volume.rename("/A.BIN", "/SUB/A.BIN").is_ok() && volume.sync().is_ok();
        drop(volume);

        let what = format!("move interrupted after {fail_after} writes");
        let report = fsck(&image);
        if let Some(phrase) = report.indicates_corruption() {
            assert_eq!(
                phrase, "share clusters",
                "{what}: fsck.vfat found {phrase:?}:\n{}",
                report.output
            );
        }
        let at_either = read(&image, "/A.BIN").or_else(|| read(&image, "/SUB/A.BIN"));
        assert_eq!(
            at_either.as_deref(),
            Some(&data[..]),
            "{what}: the file is lost"
        );
        if finished {
            assert_fsck_clean(&image);
            assert_eq!(read(&image, "/A.BIN"), None, "{what}");
            return;
        }
    }
    panic!("the move never completed in {MAX_WRITES} writes");
}
