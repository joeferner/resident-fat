//! What can go wrong, and enough detail to act on it.
//!
//! Every variant carries the value that caused it. A filesystem refusing a
//! volume is nearly always something a person has to diagnose from a log
//! line on a device with no debugger, so "bad geometry" without the field
//! and the number is a wasted message.
//!
//! For the same reason [`enum@Error`] implements [`Display`](core::fmt::Display)
//! — and so [`core::error::Error`] — for *every* device error the
//! [`BlockDevice`](crate::blockdev::BlockDevice) trait admits, rather than
//! only for those that are themselves `Display`. See [`Error::Device`].

use thiserror::Error;

/// A FAT format this crate does not implement.
///
/// Reported by name rather than folded into a generic "unsupported": the
/// volume was understood well enough to classify, which is a materially
/// different situation from a corrupt or unrecognisable one, and the
/// difference decides whether reformatting is the answer.
#[non_exhaustive]
#[derive(Error, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// A FAT12 volume.
    #[error("FAT12")]
    Fat12,
    /// A FAT16 volume.
    #[error("FAT16")]
    Fat16,
}

/// A boot sector field this crate refuses, and the value it held.
///
/// The list is deliberately short, because a check that rejects a volume
/// working everywhere else is worse than no check at all. Both the media
/// descriptor and the disk geometry fields are wrong on enough real devices
/// that Linux removed its own checks on them, so nothing here looks at
/// either. What is left is the arithmetic the rest of the crate depends on:
/// a field whose value would make a later calculation meaningless, rather
/// than one that merely looks unusual.
#[non_exhaustive]
#[derive(Error, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Geometry {
    /// The volume's sector size is not the 512 bytes this crate supports.
    ///
    /// FAT allows 1024, 2048 and 4096 as well, and this crate refuses all
    /// three rather than translating them: a sector number and a device
    /// block number are the same number only at 512, and every offset the
    /// rest of the crate computes is a sector number handed to a block
    /// device. Effectively every SD card, and every volume `mkfs.vfat`
    /// produces without being told otherwise, uses 512 — see
    /// [`BLOCK_SIZE`](crate::BLOCK_SIZE).
    #[error("sector size {0} is not the 512 bytes this crate supports")]
    SectorSize(u16),

    /// Sectors per cluster was zero or not a power of two.
    #[error("sectors per cluster {0} is not a power of two")]
    SectorsPerCluster(u8),

    /// The reserved region was empty, so the boot sector is not inside it.
    #[error("reserved sector count is {0}")]
    ReservedSectors(u16),

    /// The volume claims no file allocation tables.
    #[error("file allocation table count is {0}")]
    FatCount(u8),

    /// The table has no length.
    #[error("file allocation table length is {0}")]
    FatLength(u32),

    /// The volume claims no sectors at all.
    #[error("total sector count is {0}")]
    TotalSectors(u32),

    /// The data area begins past the end of the volume.
    #[error("data area starts at sector {data_start}, past the volume's {total_sectors} sectors")]
    DataBeyondVolume {
        /// First sector of the data area.
        data_start: u32,
        /// Sectors the volume claims to have.
        total_sectors: u32,
    },

    /// More clusters than the format can address.
    #[error("{0} clusters is more than the format can address")]
    ClusterCount(u32),

    /// The volume claims more space than whatever contains it.
    ///
    /// The container is the device, or the partition the volume sits in when
    /// it was reached through a partition table — whichever is tighter, and
    /// neither when the device declines to report its size and no partition
    /// was named.
    ///
    /// Distinct from [`DataBeyondVolume`](Self::DataBeyondVolume), which is
    /// the boot sector's fields disagreeing with *each other*. Here they are
    /// internally consistent and the volume simply does not fit where it was
    /// found, so the numbers worth printing are different ones.
    #[error("volume needs blocks {first_block}..{end}, but only {bound} are available")]
    VolumeTooLarge {
        /// First block of the volume.
        first_block: u64,
        /// One past the last block the volume claims.
        end: u64,
        /// One past the last block the container makes available.
        bound: u64,
    },
}

/// What can go wrong inside the allocation table.
///
/// Separate from [`enum@Error`] because **none of it involves a device**. The
/// table is resident, so walking a chain, finding room and freeing clusters
/// are memory operations that either succeed or find the table inconsistent
/// — there is no transfer to fail.
///
/// That is worth a type rather than five more variants on [`enum@Error`]: it lets
/// [`Fat`](crate::Fat)'s methods say what they can actually return instead of
/// being generic over a device error they can never produce. Before this
/// existed, every one of them carried an unconstrained type parameter that
/// the caller had to name — `fat.runs::<D::Error>(cluster)` — for the sole
/// purpose of building an error variant that never arrived.
///
/// Converts into [`enum@Error`] with `?`, so a caller mixing table and device
/// work writes neither a turbofish nor a `map_err`.
#[non_exhaustive]
#[derive(Error, Debug, Clone, Copy, PartialEq, Eq)]
pub enum FatError {
    /// A cluster number outside the addressable range.
    ///
    /// Cluster numbering starts at 2, and the upper bound is what the table
    /// can actually address rather than what the volume claims — the two
    /// disagree on corrupt media, and the table is the one that bounds
    /// memory.
    #[error("cluster {cluster} is outside the volume")]
    BadCluster {
        /// The offending cluster number.
        cluster: u32,
    },

    /// A cluster chain longer than the volume has clusters, which means it
    /// loops.
    ///
    /// Worth bounding even though a loop is corruption rather than a normal
    /// state: with the table resident, walking a chain is a memory loop with
    /// no I/O to slow it down, so an unbounded walk spins instantly rather
    /// than slowly.
    #[error("cluster chain from {start} loops")]
    ChainLoop {
        /// Cluster the walk started from.
        start: u32,
    },

    /// A chain ran into a free cluster.
    ///
    /// Corruption, not the end of the file: a chain ends with an
    /// end-of-chain marker, and a free entry in the middle of one means the
    /// table and the directory disagree about what is allocated.
    #[error("cluster chain from {start} runs into free cluster {cluster}")]
    FreeClusterInChain {
        /// Cluster the walk started from.
        start: u32,
        /// The free cluster it reached.
        cluster: u32,
    },

    /// The volume has no room for what was asked.
    #[error("no room: {wanted} clusters wanted, {free} free")]
    DiskFull {
        /// Clusters the operation needed.
        wanted: u32,
        /// Clusters actually available.
        free: u32,
    },

    /// A file's cluster chain holds less data than its recorded length.
    ///
    /// The directory entry and the allocation table disagree, which is
    /// corruption rather than a short file: the bytes the entry promises are
    /// not anywhere on the volume.
    #[error("the chain from {start} is shorter than the {size} bytes the entry claims")]
    ChainShorterThanFile {
        /// First cluster of the file.
        start: u32,
        /// Length the directory entry recorded.
        size: u32,
    },
}

/// What can go wrong reading a boot sector.
///
/// Separate from [`enum@Error`] for the same reason as [`FatError`], and it
/// is the same reason: **none of it involves a device**.
/// [`BootSector::parse`](crate::BootSector::parse) is handed a block that has
/// already been read, so it either understands those 512 bytes or it does
/// not — there is no transfer left to fail.
///
/// Being its own type is what lets `parse` say so. Reported through
/// [`enum@Error`] it would have had to be generic over a device error it
/// could never produce, and a caller parsing a block it had read itself
/// would have needed a turbofish naming a type that never appeared —
/// `BootSector::parse::<()>(&block)`.
///
/// Converts into [`enum@Error`] with `?`, so mounting, which does touch a
/// device, needs neither turbofish nor `map_err`.
#[non_exhaustive]
#[derive(Error, Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootError {
    /// No boot signature — this is not a FAT volume, or not the right
    /// sector.
    ///
    /// The usual cause is a partitioned card: the partition table sits
    /// where the boot sector was looked for, so the volume has to be
    /// mounted at the partition's first block rather than at block 0.
    #[error("no FAT boot signature")]
    NotFat,

    /// A FAT12 or FAT16 volume, recognised and declined.
    #[error("{0} is not supported")]
    UnsupportedFormat(Format),

    /// A boot sector field this crate will not accept.
    #[error("invalid boot sector: {0}")]
    BadGeometry(Geometry),

    /// The block handed in was shorter than a sector.
    ///
    /// A caller's mistake rather than anything about a volume: every field a
    /// boot sector holds lives in its first 512 bytes, so a shorter slice has
    /// nothing to parse.
    ///
    /// Reported rather than asserted, for the reason given on
    /// [`Error::BufferLength`] — indexing past the end would panic, and a
    /// bare-metal target has no unwinder to catch it. The two are the same
    /// kind of mistake and get the same treatment.
    #[error(
        "boot sector block is {len} bytes, but {} are needed",
        crate::BLOCK_SIZE
    )]
    ShortBlock {
        /// Bytes the caller supplied.
        len: usize,
    },
}

/// Anything that can stop a filesystem operation.
///
/// `#[non_exhaustive]`, and that is a decision rather than a habit: a
/// filesystem accumulates error kinds for years, and whether this enum can
/// grow one without a breaking release is unchangeable once published. The
/// cost is a wildcard arm in every downstream `match`; the alternative is a
/// major version for each newly distinguished failure.
#[non_exhaustive]
#[derive(Error, Debug, Clone, PartialEq, Eq)]
pub enum Error<E> {
    /// The block device failed. Carries whatever it reported.
    ///
    /// Rendered with `{:?}` rather than `{}`, which is what makes this whole
    /// type printable. [`BlockDevice::Error`](crate::blockdev::BlockDevice::Error)
    /// is bounded `Debug` and nothing more, so formatting the payload with
    /// `Display` would give `Error<E>` a `Display` impl that a conforming
    /// device error need not satisfy — and an error type that cannot be
    /// printed is no use on a board whose only diagnostic is a log line.
    ///
    /// Bounding the trait on `Display` instead would cost more than it
    /// bought: `embedded_sdmmc::BlockDevice::Error` is itself only `Debug`,
    /// so the `bridge` module would stop accepting the drivers it exists to
    /// accept. A `#[derive(Debug)]` enum, which is what a device error nearly
    /// always is, prints the same either way.
    #[error("block device error: {0:?}")]
    Device(E),

    /// The boot sector was not one this crate can use.
    ///
    /// Nested rather than flattened because these are the failures that
    /// involve no device — see [`BootError`], which is what
    /// [`BootSector::parse`](crate::BootSector::parse) itself returns. `?`
    /// converts.
    #[error("{0}")]
    Boot(#[from] BootError),

    /// A partition was asked for on a device whose block 0 is not a
    /// partition table.
    ///
    /// Usually means the device is formatted as one bare volume, which
    /// [`FileSystem::mount`](crate::FileSystem::mount) handles. Also
    /// reported for the placeholder table a GPT disk carries, which is a
    /// table this crate deliberately will not mount through.
    #[error("no partition table")]
    NoPartitionTable,

    /// The partition table has nothing in that slot.
    ///
    /// The slot is the raw table position, 0 to 3, so an empty slot before a
    /// used one is a normal arrangement rather than a corrupt table.
    #[error("partition {index} is not in use")]
    NoSuchPartition {
        /// The slot that was asked for.
        index: usize,
    },

    /// The allocation table was inconsistent, or had no room.
    ///
    /// Nested rather than flattened because these are exactly the failures
    /// that involve no device — see [`FatError`], which is what
    /// [`Fat`](crate::Fat)'s own methods return. `?` converts.
    #[error("{0}")]
    Fat(#[from] FatError),

    /// An allocated entry sits after the end-of-directory marker.
    ///
    /// Those entries are files that exist on the volume and that no
    /// implementation will list, since every reader stops at the marker.
    /// Reporting it rather than stopping quietly is deliberate: silently
    /// ignoring files that are really there is how data goes missing
    /// without anyone being told.
    #[error("directory has an allocated entry at index {index}, after its end marker")]
    EntryAfterEnd {
        /// Which 32-byte slot the stray entry occupies.
        index: usize,
    },

    /// Nothing in the directory goes by that name.
    #[error("{name} not found")]
    NotFound {
        /// The name that was looked for.
        name: alloc::string::String,
    },

    /// A path component named a file where a directory was needed.
    #[error("{name} is not a directory")]
    NotADirectory {
        /// The name that turned out to be a file.
        name: alloc::string::String,
    },

    /// A directory was named where a file was needed.
    #[error("{name} is a directory")]
    IsADirectory {
        /// The name that turned out to be a directory.
        name: alloc::string::String,
    },

    /// A directory has no room for another entry and cannot grow.
    #[error("directory is full")]
    DirectoryFull,

    /// A name no FAT volume can store.
    ///
    /// Names too long for 8.3 are not refused — they get a long name and a
    /// generated alias. What is refused is a name that has no storable form
    /// at all: empty, longer than 255 characters, containing one of the
    /// characters the format reserves, or relying on the leading and
    /// trailing spaces and trailing periods Windows silently strips.
    /// Stripping them here would mean handing a file back under a different
    /// name than the one asked for.
    #[error("{name} cannot be stored as a FAT name")]
    BadName {
        /// The name that was refused.
        name: alloc::string::String,
    },

    /// Every 8.3 alias this name could be given was already taken.
    ///
    /// Reachable only in theory: aliases are probed up to `~999999`, and an
    /// aliased name costs at least two of a directory's 65536 slots, so no
    /// directory can hold enough names to exhaust them. It is reported
    /// rather than asserted because the alternative is an unbounded search
    /// on a corrupt directory.
    #[error("no free 8.3 alias is left for {name}")]
    NoAliasAvailable {
        /// The name that could not be given an alias.
        name: alloc::string::String,
    },

    /// A directory was removed that still holds entries.
    ///
    /// Removing it would leave everything inside unreachable but still
    /// marked as allocated — space no `fsck` run reclaims, because the
    /// clusters really are referenced by directory entries nothing can
    /// find.
    #[error("{name} is not empty")]
    DirectoryNotEmpty {
        /// The directory that still has contents.
        name: alloc::string::String,
    },

    /// A file or directory of that name already exists.
    #[error("{name} already exists")]
    AlreadyExists {
        /// The name that was already taken.
        name: alloc::string::String,
    },

    /// A [`File`](crate::File) was used after its directory slot stopped
    /// being its own.
    ///
    /// A handle records where its entry lives so that changing a length is a
    /// rewrite of one known slot rather than a fresh search. Deleting the
    /// file frees that slot, and the next file created in the directory may
    /// take it — at which point the old handle names an entry belonging to
    /// something else. Writing through it would silently truncate a file the
    /// caller never opened, so it is refused.
    ///
    /// Nothing on the volume is wrong when this is reported: the handle is
    /// stale, not the card. Open the path again.
    ///
    /// # When a handle goes stale
    ///
    /// Any deletion from its directory, which includes the deletion
    /// [`write_file`](crate::FileSystem::write_file) performs when it
    /// replaces an existing file. Same-name replacement has to count,
    /// because the slot is genuinely a different file afterwards and the
    /// eleven name bytes are identical — FAT stores no inode, no generation
    /// and no creation cookie for a check to find a difference in, so the
    /// only thing left to notice is that a deletion happened at all.
    ///
    /// That makes deleting a *sibling* invalidate the handle too. The false
    /// positive is deliberate: the cost is reopening a path in a resident
    /// directory, and the alternative is a write landing on whichever file
    /// was handed the slot — verified to leave a volume `fsck.vfat` reports
    /// as having a file whose size does not account for its chain.
    #[error("the handle for {name} no longer matches the entry it was opened from")]
    StaleFile {
        /// The 8.3 name the handle expected to find in its slot.
        name: alloc::string::String,
    },

    /// A buffer whose length is not the one the operation needs.
    ///
    /// Reported rather than asserted. This is a caller's mistake and a
    /// `debug_assert!` would be the usual answer to one, but a panic on a
    /// bare-metal target has no unwinder to catch it — a filesystem
    /// aborting the machine because a buffer was sized wrong is a worse
    /// outcome than the caller being told.
    #[error("buffer is {actual} bytes, but {expected} were needed")]
    BufferLength {
        /// Bytes the operation required.
        expected: u64,
        /// Bytes the caller supplied.
        actual: usize,
    },

    /// A buffer the operation needed would not fit in memory.
    ///
    /// Not necessarily a bug in either the caller or the volume. This crate
    /// trades memory for transfers by design, and the sizes it asks for are
    /// set by *the volume* rather than by anything a program chooses — the
    /// allocation table is four bytes per cluster, and reading a file or a
    /// directory whole means a buffer as large as it is. A host that cannot
    /// spare that much is a legitimate outcome, and the honest answer is to
    /// say which allocation failed rather than to succeed at a smaller one
    /// the caller did not ask for.
    ///
    /// Reported rather than left to the allocator, which is the whole point
    /// of the variant. Rust's infallible allocation calls the `alloc` error
    /// handler on failure, and on a bare-metal target that aborts the
    /// machine — a filesystem is in no position to decide that a card too
    /// big for this board should stop the program. So every allocation
    /// whose size comes off the volume goes through `try_reserve` and
    /// arrives here instead.
    ///
    /// `bytes` is what was being asked for, not what was available. On a
    /// 32-bit target this also covers a request too large for `usize` at
    /// all, which is why the field is a `u64` rather than a `usize`.
    #[error("could not allocate {bytes} bytes")]
    OutOfMemory {
        /// Bytes the operation tried to allocate.
        bytes: u64,
    },
}

/// A `Vec` of `count` copies of `value`, or [`Error::OutOfMemory`].
///
/// Lives beside the error rather than beside its callers because producing
/// that error is the only reason it exists: `vec![value; count]` is shorter
/// and does the same thing right up until the allocation fails, at which
/// point it aborts instead of returning. Every allocation this crate sizes
/// from the volume goes through here, so there is one place to look for how
/// that case is handled.
///
/// `count` is a `u64` rather than a `usize` on purpose. The counts reaching
/// this come from 32-bit on-disk fields and from sums over a cluster chain,
/// so on a 32-bit target they can exceed what `usize` holds — and silently
/// truncating one yields a buffer that is too short rather than an
/// allocation that fails, which is a panic further along instead of an
/// error here.
pub(crate) fn try_filled<T: Clone, E>(value: T, count: u64) -> Result<alloc::vec::Vec<T>, E> {
    let bytes = count.saturating_mul(core::mem::size_of::<T>() as u64);
    let too_big = || Error::OutOfMemory { bytes };

    let count = usize::try_from(count).map_err(|_| too_big())?;
    let mut buffer = alloc::vec::Vec::new();
    buffer.try_reserve_exact(count).map_err(|_| too_big())?;
    // Cannot reallocate: the reservation above is exact and already made.
    buffer.resize(count, value);
    Ok(buffer)
}

impl<E> Error<E> {
    /// Wraps a block device's own error.
    pub fn device(error: E) -> Self {
        Error::Device(error)
    }
}

/// The result of anything that touches a volume.
pub type Result<T, E> = core::result::Result<T, Error<E>>;

#[cfg(test)]
mod tests {
    use super::*;

    /// A device error carrying exactly what the trait demands and nothing
    /// more, which is the shape a `#[derive(Debug)]` enum in a driver has.
    #[derive(Debug)]
    struct DebugOnly;

    /// A conforming device error is enough to print this crate's errors.
    ///
    /// The bound on [`BlockDevice::Error`](crate::blockdev::BlockDevice::Error)
    /// is `Debug` alone, so a driver is under no obligation to implement
    /// `Display`. If `Error<E>`'s own `Display` asked for one anyway — which
    /// it would the moment the `Device` variant formatted its payload with
    /// `{}` instead of `{:?}` — then the errors this crate returns could not
    /// be printed by the consumers it was written for, and the failure would
    /// land in their build rather than in this one.
    #[test]
    fn errors_print_for_any_conforming_device_error() {
        fn printable<T: core::fmt::Display + core::error::Error>(_: &T) {}

        let error: Error<DebugOnly> = Error::device(DebugOnly);
        printable(&error);
        assert_eq!(
            alloc::format!("{error}"),
            "block device error: DebugOnly",
            "the device error should reach the message"
        );

        // And the variants that carry no device error at all stay legible
        // whatever `E` is.
        let full: Error<DebugOnly> = FatError::DiskFull { wanted: 4, free: 1 }.into();
        assert_eq!(
            alloc::format!("{full}"),
            "no room: 4 clusters wanted, 1 free"
        );
    }

    /// An allocation that cannot succeed is reported, not attempted.
    ///
    /// What makes this worth pinning is where the sizes come from. A file's
    /// length and a table's sector count are fields on the volume, so an
    /// infallible `vec![value; n]` hands a corrupt card the power to call
    /// the `alloc` error handler — which aborts, with no unwinder on a
    /// bare-metal target to catch it. Rejecting the card is the filesystem's
    /// business; halting the board is not.
    ///
    /// `u64::MAX` is the request used because it fails the same way on every
    /// target and allocates nothing on any of them: on a 32-bit target it
    /// exceeds `usize` and is refused by the conversion, and on a 64-bit one
    /// it exceeds the largest possible allocation and is refused by
    /// `try_reserve_exact` before a byte is asked for.
    #[test]
    fn an_impossible_allocation_is_reported_rather_than_attempted() {
        let refused: Result<alloc::vec::Vec<u8>, DebugOnly> = try_filled(0u8, u64::MAX);
        match refused {
            Err(Error::OutOfMemory { bytes }) => assert_eq!(bytes, u64::MAX),
            other => panic!("expected OutOfMemory, got {other:?}"),
        }

        // The reported size saturates rather than wrapping. A count that
        // overflows when scaled by the element size must not come back as a
        // small number, which would read as a trivial allocation failing.
        let wide: Result<alloc::vec::Vec<u32>, DebugOnly> = try_filled(0u32, u64::MAX / 2);
        match wide {
            Err(Error::OutOfMemory { bytes }) => assert_eq!(bytes, u64::MAX),
            other => panic!("expected OutOfMemory, got {other:?}"),
        }
    }

    /// And an allocation that can succeed still does, at the exact length.
    #[test]
    fn a_possible_allocation_still_happens() {
        let buffer: Result<alloc::vec::Vec<u8>, DebugOnly> = try_filled(7u8, 4);
        assert_eq!(buffer.expect("4 bytes should be available"), [7, 7, 7, 7]);

        let empty: Result<alloc::vec::Vec<u8>, DebugOnly> = try_filled(0u8, 0);
        assert!(
            empty.expect("an empty buffer is not a failure").is_empty(),
            "a zero-length chain asks for nothing and must not be an error"
        );
    }
}
