//! A FAT32 filesystem that spends memory to move data in whole runs.
//!
//! Built for a bare-metal system with memory to spare — a board with a
//! gigabyte of RAM and an SD card, rather than a microcontroller with
//! kilobytes. It keeps the whole allocation table and every directory it
//! has read in memory, which is what lets a contiguous file be moved in a
//! single device transfer however large it is.
//!
//! ```no_run
//! use resident_fat::{BlockDevice, FileSystem, Result};
//!
//! # fn example<D: BlockDevice>(device: D) -> Result<(), D::Error> {
//! // Reads the boot sector and loads the whole allocation table.
//! let mut volume = FileSystem::mount(device)?;
//!
//! // Listing a directory. Parsed on the first look at it, and from then
//! // on this costs no device traffic at all.
//! for entry in volume.open_dir("/roms")?.iter() {
//!     println!("{} ({} bytes)", entry.name(), entry.len());
//! }
//!
//! // Reading a file: one transfer per contiguous run of its chain, so a
//! // file written in one go comes back in one call.
//! let file = volume.open("/roms/game.nes")?;
//! let data = volume.read_all(&file)?;
//!
//! // Writing one. The length is known before a byte is written, so the
//! // whole chain is allocated at once and the file lands contiguous.
//! volume.write_file("/saves/game.sav", &data)?;
//!
//! // Writes the table back and clears the volume's dirty flag. Until this
//! // happens, allocations exist only in memory.
//! volume.unmount()?;
//! # Ok(())
//! # }
//! ```
//!
//! # Status
//!
//! **Early.** A FAT32 volume can be mounted, walked, read, written, grown
//! and truncated; long names and directories are both read and created.
//! Every claim below is checked against `fsck.vfat` and `mtools`, which are
//! independent implementations.
//!
//! What is missing is native adapters for the block devices real hardware
//! provides — the `bridge` module, behind the `embedded-sdmmc` feature,
//! covers those in the meantime. The version number should be read as an
//! early one, and the API will change.
//!
//! # Getting a block device
//!
//! Everything above starts with a `device`, and supplying one is the only
//! thing this crate asks of a consumer. Two ways:
//!
//! * **Implement [`BlockDevice`]** — four methods, two of which are
//!   `read` and `write` over `&[u8]` whose length is a multiple of
//!   [`BLOCK_SIZE`]. Transfers are byte slices rather than a block newtype
//!   so that a caller's own buffer reaches the device without a copy, and
//!   so that the unit of transfer can be a whole run.
//! * **Enable the `embedded-sdmmc` feature** and wrap a driver you already
//!   have in `bridge::FromEmbeddedSdmmc`. That covers most SD card drivers
//!   written for embedded Rust, unchanged.
//!
//! An implementation should be cheap to call with *large* slices. One that
//! internally splits a long transfer into a command per block gives up most
//! of the reason to use this crate; if the hardware has a real limit, say
//! so with [`BlockDevice::max_transfer_blocks`] and long transfers are
//! split to fit.
//!
//! # Mounting
//!
//! Three ways in, so the common cases are not guesswork:
//!
//! * [`FileSystem::mount`] — the whole device is the volume, which is what
//!   `mkfs.vfat` on a raw device produces.
//! * [`FileSystem::mount_at`] — the volume starts at a block number the
//!   caller already knows.
//! * `FileSystem::mount_partition` (with the `mbr` feature) — read the
//!   partition table and mount the volume in one of its slots. This is the
//!   one for a card an imaging tool wrote.
//!
//! The third exists because telling a partition table from a boot sector is
//! not something every consumer should be reimplementing: both end with the
//! same `0x55AA` signature, and reading one as the other yields block
//! numbers that are wrong but plausible.
//!
//! # What makes this different
//!
//! Embedded FAT implementations cache one 512-byte block, because they
//! cannot assume memory exists. That single decision propagates: a file
//! write becomes one device command per block, chain walking re-reads the
//! table from the device, and looking a file up by name rescans the
//! directory.
//!
//! This crate assumes memory exists — a few megabytes of it — and keeps
//! two structures resident:
//!
//! * **The whole file allocation table.** A 32 GB volume with 32 KB
//!   clusters is about a million entries, four bytes each. Chain walking
//!   becomes array indexing, allocation becomes an array scan, and both
//!   on-disk copies of the table are written back once, on sync, rather
//!   than read-modify-written per entry.
//! * **Parsed directories.** Read once, in one transfer; enumeration is
//!   then a slice walk and lookup is a map hit.
//!
//! The payoff is not the caching itself, it is what the caching enables:
//! with the chain in memory, **contiguous runs are free to discover**. A
//! file that occupies one unbroken run of clusters is read or written in a
//! single device transfer, however large it is. An implementation that has
//! to consult the device to learn where the next cluster lives cannot do
//! better than one transfer per cluster, because it does not yet know that
//! the next one is adjacent.
//!
//! # What it costs
//!
//! Memory, and the [`alloc`] crate — this is not a filesystem for a
//! microcontroller with kilobytes of RAM, and `embedded-sdmmc` remains the
//! right answer there. It is a filesystem for the increasingly common case
//! of a bare-metal system with a gigabyte sitting idle.
//!
//! Four bytes per cluster, so the bill is set by the volume rather than by
//! anything a program chooses: a 968 MB FAT32 partition with 4 KB clusters
//! holds 247,955 of them and costs **968 KiB** resident. The number grows
//! with the volume and shrinks with the cluster size, and large cards use
//! large clusters, so a 32 GB volume formatted the way such cards come is
//! around 4 MB rather than 32.
//!
//! Directories cost extra, and are kept once read rather than evicted; see
//! [`fs::FileSystem`]. A volume whose table will not fit reports that at
//! mount rather than aborting.
//!
//! # Scope
//!
//! FAT32. FAT12 and FAT16 are not implemented, but the design deliberately
//! does not assume 32-bit entries: the resident table is `u32` regardless
//! of the on-disk width, so a narrower format would cost only pack and
//! unpack at load and sync. exFAT is a different filesystem wearing a
//! similar name, and is out of scope.
//!
//! # Features
//!
//! Neither is on by default.
//!
//! * **`mbr`** — reading partition tables, so mounting a card an imaging
//!   tool wrote does not need a second crate. About a hundred lines and no
//!   dependencies. Adds `FileSystem::mount_partition` and the `mbr` module.
//!   GPT is not supported; the protective record a GPT disk carries is
//!   recognised and declined rather than mounted.
//! * **`embedded-sdmmc`** — a blanket bridge from that crate's
//!   `BlockDevice` to [`BlockDevice`], so an existing driver works here
//!   unchanged. Adds the `bridge` module. Enabling it makes
//!   `embedded-sdmmc` a *public* dependency: a semver-breaking release
//!   there is one here too, which is why it is opt-in.
//!
//! # Toolchain
//!
//! Stable Rust 1.85 or newer, and no nightly, no `-Zbuild-std` and no
//! linker script: this is ordinary portable Rust that happens to be
//! `no_std`, and `rustup` ships a precompiled `core` and `alloc` for every
//! target it is meant to run on. `alloc` is required rather than optional.

// `no_std` everywhere except under `cargo test`, where the unit tests want
// the standard library for image fixtures and for shelling out to the
// `fsck.vfat` and `mtools` oracles. Integration tests under `tests/` link
// `std` regardless -- they are separate crates -- so this only affects
// `#[cfg(test)]` modules inside `src/`.
#![cfg_attr(not(test), no_std)]
// Every public item carries rustdoc, enforced rather than reviewed. A
// filesystem's API is the part consumers have to reason about without
// reading the implementation, so an undocumented public item is a defect,
// not a style lapse.
#![deny(missing_docs)]
// Feature-gate annotations in the rendered documentation. Enabled by
// `make doc` and by the docs.rs metadata in `Cargo.toml`, ignored
// otherwise, so this costs a normal build nothing.
#![cfg_attr(docsrs, feature(doc_cfg))]

extern crate alloc;

/// Compiles the README's Rust blocks as doctests.
///
/// The front page of the repository is the first code most people read, and
/// nothing else checks it — a signature that drifts there is wrong in the
/// place it is most likely to be copied from.
///
/// Included only under `cfg(doctest)`, so this collects the examples without
/// putting the README into the rendered documentation. Merging the two
/// outright would drag the CI badges and the repository-relative licence
/// links onto docs.rs, where the badges are noise and the links are 404s;
/// the crate-level documentation above is written for a reader who has
/// already decided to use this, which is a different job from the README's.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeExamples;

pub mod blockdev;
pub mod boot;
#[cfg(feature = "embedded-sdmmc")]
#[cfg_attr(docsrs, doc(cfg(feature = "embedded-sdmmc")))]
pub mod bridge;
pub mod codepage;
pub mod dir;
pub mod error;
pub mod fat;
pub mod file;
pub mod fs;
#[cfg(feature = "mbr")]
#[cfg_attr(docsrs, doc(cfg(feature = "mbr")))]
pub mod mbr;
pub mod time;

mod name;

// Everything a consumer names in ordinary use, at the crate root, so that a
// program mounting a volume and reading a file need not know which module
// each type lives in. The rule is that anything a root-exported method
// takes or returns is itself root-exported -- `FileSystem::boot_sector`
// hands back a `BootSector`, so `BootSector` belongs here too.
//
// The feature-gated modules are the exception: `mbr` and `bridge` stay
// reached by their module path, which says which feature they need at the
// use site rather than leaving a name to disappear from the root when a
// feature is off.
pub use blockdev::{BLOCK_SIZE, BlockDevice};
pub use boot::{BootSector, FsInfo};
pub use codepage::Codepage;
pub use dir::{Attributes, DirEntry, Directory, ShortName};
pub use error::{BootError, Error, FatError, Format, Geometry, Result};
pub use fat::{Fat, Run};
pub use file::File;
pub use fs::FileSystem;
pub use time::{Clock, DateTime, EpochClock, FnClock, Packed};
