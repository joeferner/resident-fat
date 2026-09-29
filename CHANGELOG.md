# Changelog

Notable changes to `resident-fat`, in the format of
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/). This crate
follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html), with
the usual pre-1.0 caveat that a `0.x` release bumps the *minor* for a
breaking change.

**One kind of change semver does not cover, and this crate has to:** the
bytes written to the card. A change to allocation policy, write ordering, or
which directory-entry fields get populated is invisible to the compiler and
to every downstream build, and can still be the most consequential thing in
a release — the previous version's volumes are the compatibility surface.
Anything in that category gets an entry here whether or not the Rust API
moved.

## [Unreleased]

### Added

- **`FileSystem::rename(from, to)`**: renames or moves a file or
  directory, replacing a file already at `to`. No data moves; only
  directory entries change, keeping the file's attributes and timestamps.
  FAT has no atomic rename, so the order of the writes is what decides
  what an interruption leaves, and it differs by case:
  - **file over file** — `from` deleted, then `to`'s entry repointed at
    its data in one sector write, then `to`'s old data freed. `to` always
    names a whole file, old or new, and no cluster ever belongs to two
    names: write-then-rename is now a safe way to replace a file;
  - **same directory, a name taking no more slots** — rewritten in place,
    so an interruption leaves the file under its old name, its new one,
    or its 8.3 alias, never lost or shared;
  - **to another directory, or a longer name** — new entries, then the
    old ones deleted. The one case with a window: interrupted between the
    two, both names share the file's chain, which loses nothing and which
    `fsck.vfat` repairs.

  A directory moved to another parent has its `..` repointed; moving one
  inside itself is the new **`Error::MoveIntoItself`**. Handles to the
  renamed file, and to a file it replaced, go stale. Each case is checked
  against `fsck.vfat` and `mtools`, and interrupted at every write.

- **`counted::Counted`**, a block device wrapper that counts every read
  and write — calls and blocks — into a caller-owned `counted::Counters`,
  usually a `static`, so a snapshot can be taken wherever the numbers are
  wanted rather than only where the volume is held. `Counters::since`
  differences two snapshots with wrapping arithmetic, and
  `counted::Counts` prints as one line ending in blocks per call, the
  figure that says whether a volume is being written in runs or a block
  at a time. It forwards `max_transfer_blocks`, which a transparent
  wrapper has to. On targets with 32-bit atomics; moved from
  `rpi-water-sensor`, where it measured the 67× OTA speed-up.

### Changed

- **The README's Status section**, which still said native block device
  adapters were missing. `rpi-hal` ships them.

No change to what is written to the card by any operation that existed
before; `rename` is new, so everything it writes is.

## [0.2.0] - 2026-09-29

### Added

- **`FileSystem::mount_first_fat`**, behind `mbr`: mounts a device's FAT
  volume whichever way it was formatted — through a partition table, the
  first partition whose type byte says FAT (by type rather than by slot,
  and held to the partition's length); with no table, the whole device,
  as `mount` does. What every board mounting a card written by an
  imaging tool otherwise writes by hand, with the boot-sector-or-table
  check that is easy to get wrong.
- **`Error::NoFatPartition`**, for a table with nothing FAT in it —
  distinct from `NoPartitionTable`, since there *is* a table and the fix
  is a different one. `Error` is `#[non_exhaustive]`, so this is not a
  breaking change.
- **`FileSystem::first_block`**, the device block the volume begins at.

No change to what is written to the card.

## [0.1.0] - 2026-09-01

First release. There is no earlier version to have changed from, so what the
crate does is left to the [README](README.md) and the [API
documentation](https://docs.rs/resident-fat) rather than restated as a list
of additions here. Entries proper begin at 0.2.0.

Read the version number as an early one. A FAT32 volume can be mounted,
walked, read, written, grown and truncated; long names and directories are
both read and created; and the on-disk result is checked against `fsck.vfat`
and `mtools`, which are independent implementations. What is missing is
native adapters for the block devices real hardware provides — the
`embedded-sdmmc` bridge covers those in the meantime — and the API will
change.

[0.2.0]: https://github.com/joeferner/resident-fat/releases/tag/v0.2.0
[0.1.0]: https://github.com/joeferner/resident-fat/releases/tag/v0.1.0
