# resident-fat

[![CI](https://img.shields.io/github/actions/workflow/status/joeferner/resident-fat/ci.yml?branch=main&label=CI)](https://github.com/joeferner/resident-fat/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/resident-fat.svg)](https://crates.io/crates/resident-fat)
[![docs.rs](https://img.shields.io/docsrs/resident-fat)](https://docs.rs/resident-fat)

A FAT32 filesystem that spends memory to move data in whole runs.

## Status

**Early.** A FAT32 volume can be mounted, walked, read, written, grown and
truncated; long names and directories are both read and created. Every
claim below is checked against `fsck.vfat` and `mtools`, which are
independent implementations.

What is missing is native adapters for the block devices real hardware
provides — the `bridge` module, behind the `embedded-sdmmc` feature, covers
those in the meantime. The version number should be read as an early one,
and the API will change.

## Why a card is slow

Because it charges per *command*, not per block. A single-block write costs
something like 17 ms; a 128-block write costs about 26 ms — half as much
again, for a hundred and twenty-eight times the data. Per block that is
roughly **60× cheaper**.

So what decides how long a write takes is how many commands it becomes, and
that is a question about the filesystem rather than about the card. Two
things drive the count up, and this crate is arranged around both: not
knowing that a stretch of clusters is contiguous until after it has been
written one block at a time, and re-reading the allocation table from the
device — twice, since a volume carries two copies — for every cluster
linked.

## Using it

Supply a block device — implement `BlockDevice`, or enable the
`embedded-sdmmc` feature and wrap a driver you already have — and the rest
looks like this:

```rust
use resident_fat::{BlockDevice, FileSystem, Result};

fn load<D: BlockDevice>(device: D) -> Result<Vec<u8>, D::Error> {
    let mut volume = FileSystem::mount(device)?;
    let file = volume.open("/roms/game.nes")?;
    volume.read_all(&file)
}
```

That read is a single device transfer if the file is contiguous, whatever
its size.

## Mounting

Three ways in, so the common cases are not guesswork:

- `FileSystem::mount(device)` — the whole device is the volume, which is
  what `mkfs.vfat` on a raw device produces.
- `FileSystem::mount_at(device, block)` — the volume starts at a block
  number you already know.
- `FileSystem::mount_partition(device, 0)` — read the partition table and
  mount that slot. Needs the `mbr` feature.

The third is the one for a card an imaging tool wrote, and it exists
because telling a partition table from a boot sector is not something every
consumer should be reimplementing: the two end with the same `0x55AA`
signature, and reading one as the other yields block numbers that are wrong
but plausible.

## What makes it different

Embedded FAT implementations cache one 512-byte block, because they cannot
assume memory exists. That single decision propagates further than it
looks: a file write becomes one device command per block, chain walking
re-reads the table from the device, and looking a file up by name rescans
the directory from the start.

This crate assumes memory exists — a few megabytes of it — and keeps two
structures resident:

- **The whole file allocation table.** A 32 GB volume with 32 KB clusters
  is about a million entries at four bytes each. Chain walking becomes
  array indexing, allocation becomes an array scan, and both on-disk copies
  of the table are written back once, on sync, rather than
  read-modify-written per entry.
- **Parsed directories.** Read once, in one transfer. Enumeration is then a
  slice walk and lookup is a map hit.

The payoff is not the caching itself, it is what the caching enables: with
the chain in memory, **contiguous runs are free to discover**. A file
occupying one unbroken run of clusters is read or written in a single
device transfer, however large it is. An implementation that must consult
the device to learn where the next cluster lives cannot do better than one
transfer per cluster, because it does not yet know the next one is
adjacent.

## What it costs

Memory, and the `alloc` crate. This is not a filesystem for a
microcontroller with kilobytes of RAM — [`embedded-sdmmc`][sdmmc] remains
the right answer there, and this crate can bridge to the block device you
already implemented for it. It is a filesystem for the increasingly common
case of a bare-metal system with a gigabyte sitting idle.

[sdmmc]: https://crates.io/crates/embedded-sdmmc

## Scope

FAT32. FAT12 and FAT16 are not implemented, though the design deliberately
does not assume 32-bit entries — the resident table is `u32` regardless of
the on-disk width, so a narrower format would cost only pack and unpack at
load and sync. exFAT is a different filesystem wearing a similar name, and
is out of scope.

## Toolchain

Stable Rust 1.85 or newer. No nightly, no `-Zbuild-std`, no linker script:
the library is ordinary portable Rust that happens to be `no_std`, and
`rustup` ships a precompiled `core` and `alloc` for every target it is
meant to run on.

CI builds it for `thumbv7em-none-eabi` and
`aarch64-unknown-none-softfloat` on every change, because a host build
proves nothing about `no_std` — `std` is available there, so an accidental
dependency on it compiles clean and is discovered by a consumer instead.

## Features

| Feature | Default | What it does |
| --- | --- | --- |
| `mbr` | no | Reading partition tables, so mounting a card an imaging tool wrote does not need a second crate. About a hundred lines and no dependencies. Adds `FileSystem::mount_partition` and the `mbr` module. GPT is not supported; the protective record a GPT disk carries is recognised and declined rather than mounted. |
| `embedded-sdmmc` | no | A blanket bridge from [`embedded_sdmmc::BlockDevice`][sdmmc] to this crate's block device trait, so an existing driver works here unchanged. Enabling it makes `embedded-sdmmc` a *public* dependency: a semver-breaking release there breaks this crate's API too, which is why it is opt-in. |

## Licence

MIT or Apache-2.0, at your option. See [LICENSE-MIT](LICENSE-MIT) and
[LICENSE-APACHE](LICENSE-APACHE).

Linux's `fs/fat` was read while designing this, for its edge cases and its
write-ordering rules, and is GPL-2.0 — so it was treated as documentation
rather than as source material. Where a procedure needed following closely,
the sources were Microsoft's FAT32 specification and ChaN's FatFs, both
permissively licensed.