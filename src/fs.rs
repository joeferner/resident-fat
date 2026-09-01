//! Mounting a volume, and what you get when you have.

use alloc::collections::BTreeMap;
use alloc::string::ToString;
use alloc::vec;
use alloc::vec::Vec;

use crate::blockdev::{BLOCK_SIZE, BlockDevice};
use crate::boot::{BootSector, FsInfo};
use crate::codepage::Codepage;
use crate::dir::{Directory, ENTRY_SIZE};
use crate::error::{Error, FatError, Result};
use crate::fat::{Fat, Run};

/// How many slots one 512-byte block holds.
const SLOTS_PER_BLOCK: u32 = (BLOCK_SIZE / ENTRY_SIZE) as u32;

/// The most clusters a directory is grown by while looking for room for one
/// name.
///
/// Two would do. A name needs at most 21 slots — twenty long-name slots and
/// the 8.3 entry — and the smallest cluster a volume can have is one 512-byte
/// sector, holding 16; slot numbering runs continuously across a directory's
/// chain, so two such clusters always yield a free run of 32. Four is slack
/// over that, not a second calculation.
///
/// The bound exists so that a directory whose chain does not behave as
/// expected fails rather than growing until the volume is full. Reaching it
/// means something is wrong with the directory, not that the name was
/// unusually long.
const MAX_DIRECTORY_GROWTH: u32 = 4;

/// A mounted FAT32 volume.
///
/// Owns its block device. Methods take `&mut self` and there is no interior
/// mutability: a consumer that needs to share a volume picks its own
/// strategy — a mutex, a worker on another core, or sole ownership — rather
/// than paying for one this crate guessed at.
pub struct FileSystem<D: BlockDevice> {
    device: D,
    boot: BootSector,
    fs_info: FsInfo,
    fat: Fat,
    first_block: u64,
    /// Parsed directories, keyed by first cluster.
    ///
    /// Never evicted. A directory costs a few tens of kilobytes parsed and
    /// a volume holds a handful that anything actually walks, so a cache
    /// policy would be machinery guarding against a problem this crate's
    /// premise says does not arise. If that stops being true, eviction
    /// belongs here and nowhere else.
    directories: BTreeMap<u32, Directory>,
    /// How many entries have been deleted from each directory, by first
    /// cluster.
    ///
    /// A [`File`](crate::File) handle records the count its directory had
    /// when the handle was made, and a write refuses if the count has moved.
    /// That is what makes a handle held across a deletion detectable at all:
    /// deleting frees the slot, the next file created takes it, and the
    /// eleven name bytes the handle also checks are no help when the name is
    /// the same — which is exactly what replacing a file does.
    ///
    /// Counted per directory rather than per volume so that deleting a file
    /// in one directory does not invalidate handles into every other. It
    /// still invalidates handles to *siblings*, which is a false positive
    /// and the deliberate direction to err in: the caller reopens, where the
    /// alternative is a silent rewrite of whichever file was given the slot.
    ///
    /// Never pruned. One `u32` per directory ever written to is nothing
    /// beside the parsed directories themselves, and forgetting a count
    /// would make the handles it was guarding valid again.
    deletions: BTreeMap<u32, u32>,
    /// How 8.3 names are read and written. See [`crate::codepage`].
    codepage: Codepage,
    /// Where the timestamps on new entries come from. See
    /// [`crate::time::Clock`].
    ///
    /// Boxed rather than a type parameter on `FileSystem`, which would
    /// spread through every signature that mentions a volume for the sake of
    /// one call per file created — and boxed rather than a bare function
    /// pointer, because a clock reasonably owns something: an offset, a
    /// peripheral handle, a fixed value a test wants to control.
    /// [`crate::time::FnClock`] covers the stateless case.
    clock: alloc::boxed::Box<dyn crate::time::Clock>,
    /// Whether this mount has set the volume's dirty flag.
    ///
    /// Also the record of whether anything has been written at all, which is
    /// what lets a sync with nothing to do cost nothing.
    marked_dirty: bool,
}

impl<D: BlockDevice + core::fmt::Debug> core::fmt::Debug for FileSystem<D> {
    /// Written out rather than derived, because a clock is a trait object
    /// and requiring every implementation of [`Clock`](crate::time::Clock)
    /// to be `Debug` would be a tax on implementors for the sake of one line
    /// of output. The parsed directories are summarised rather than printed:
    /// a volume that has walked a ROM directory would otherwise put three
    /// hundred entries into the first panic message that mentions it.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("FileSystem")
            .field("device", &self.device)
            .field("first_block", &self.first_block)
            .field("clusters", &self.fat.cluster_count())
            .field("cached_directories", &self.directories.len())
            .field("marked_dirty", &self.marked_dirty)
            .finish_non_exhaustive()
    }
}

impl<D: BlockDevice> FileSystem<D> {
    /// Mounts the volume that begins at block 0 of `device`.
    ///
    /// Right for a device formatted as one volume, which is what a card
    /// written by `mkfs.vfat` onto a raw device is.
    ///
    /// A card with a partition table — anything an imaging tool wrote —
    /// needs one of the other two ways in: `mount_partition`, behind the
    /// `mbr` feature, which finds the volume itself; or
    /// [`mount_at`](Self::mount_at) with a block number the caller already
    /// knows.
    ///
    /// (`mount_partition` is named rather than linked because a link to it
    /// does not resolve when the `mbr` feature is off, which would make
    /// `cargo doc` warn in every build that does not enable it.)
    pub fn mount(device: D) -> Result<Self, D::Error> {
        Self::mount_at(device, 0)
    }

    /// Mounts the volume that begins at `first_block`.
    ///
    /// Every block number the volume uses is relative to this, so a
    /// partitioned card works by passing the partition's first block.
    pub fn mount_at(device: D, first_block: u64) -> Result<Self, D::Error> {
        Self::mount_with(device, first_block, Codepage::ASCII)
    }

    /// Mounts with a codepage for the volume's 8.3 names.
    ///
    /// Only worth supplying for a card whose short names really are not
    /// ASCII — see [`crate::codepage`]. It changes how a name is rendered
    /// and which characters can be stored in an alias; it does not change
    /// the bytes on the volume, and it does not affect long names, which are
    /// UCS-2 and need no codepage at all.
    ///
    /// Set here and not afterwards, and read back with
    /// [`codepage`](Self::codepage). There is no setter on purpose: the 8.3
    /// name a directory was parsed under is the key it is cached by, so
    /// changing the codepage under a mounted volume would leave entries
    /// findable only by the name they were read as. Mount again to change
    /// it.
    pub fn mount_with(device: D, first_block: u64, codepage: Codepage) -> Result<Self, D::Error> {
        Self::mount_within(device, first_block, codepage, None)
    }

    /// The mount every other one is built on.
    ///
    /// `limit` is how many blocks the volume is allowed to occupy from
    /// `first_block`, when something already knows — a partition table entry
    /// says so, and a volume claiming more sectors than the partition
    /// holding it is as wrong as one claiming more than the device.
    fn mount_within(
        mut device: D,
        first_block: u64,
        codepage: Codepage,
        limit: Option<u64>,
    ) -> Result<Self, D::Error> {
        let mut block = [0u8; BLOCK_SIZE];
        device
            .read(first_block, &mut block)
            .map_err(Error::device)?;

        let boot = BootSector::parse(&block)?;

        // The volume must fit inside whatever contains it. Believing a boot
        // sector that claims more is how a read runs off the end later,
        // somewhere less obviously connected to the cause.
        //
        // Two bounds, and the tighter one wins: the device's own size, which
        // it may decline to report — see `BlockDevice::block_count`, where
        // `None` is an answer rather than a failure — and the partition's,
        // when the caller came in through a partition table. With neither,
        // the check is skipped.
        let device_blocks = device.block_count().map_err(Error::device)?;
        let bound = match (device_blocks, limit.map(|blocks| first_block + blocks)) {
            (Some(device_end), Some(partition_end)) => Some(device_end.min(partition_end)),
            (device_end, partition_end) => device_end.or(partition_end),
        };
        if let Some(bound) = bound {
            let volume_blocks = u64::from(boot.total_sectors) * u64::from(boot.bytes_per_sector)
                / BLOCK_SIZE as u64;
            let end = first_block + volume_blocks;
            if end > bound {
                // Its own variant, and not `DataBeyondVolume`: that one is
                // the boot sector's fields contradicting each other, and its
                // two numbers say nothing about the bound that was actually
                // exceeded here.
                return Err(crate::error::BootError::BadGeometry(
                    crate::error::Geometry::VolumeTooLarge {
                        first_block,
                        end,
                        bound,
                    },
                )
                .into());
            }
        }

        let fs_info = match boot.fs_info_sector {
            Some(sector) => {
                device
                    .read(first_block + u64::from(sector), &mut block)
                    .map_err(Error::device)?;
                FsInfo::parse(&block, &boot)
            }
            None => FsInfo::default(),
        };

        let fat = Fat::load(&mut RelativeDevice::new(&mut device, first_block), &boot)?;

        Ok(FileSystem {
            device,
            boot,
            fs_info,
            fat,
            first_block,
            directories: BTreeMap::new(),
            deletions: BTreeMap::new(),
            codepage,
            clock: alloc::boxed::Box::new(crate::time::EpochClock),
            marked_dirty: false,
        })
    }

    /// The codepage 8.3 names are read and written through.
    pub fn codepage(&self) -> &Codepage {
        &self.codepage
    }

    /// Supplies the clock new entries are stamped from.
    ///
    /// Optional. Without one, everything written carries the FAT epoch,
    /// which is a real date and is the right answer for a board that does
    /// not know the time — see [`crate::time::Clock`] for why that is the
    /// default rather than a failure.
    ///
    /// Affects only what is written afterwards. Entries already on the
    /// volume keep the timestamps they were given, and this does not go back
    /// and correct them: a device that learns the time from the network
    /// halfway through its life has files from both eras, and inventing a
    /// timestamp for the older ones would be worse than leaving them honest.
    pub fn set_clock(&mut self, clock: alloc::boxed::Box<dyn crate::time::Clock>) {
        self.clock = clock;
    }

    /// What the clock says, for stamping an entry.
    pub(crate) fn now(&self) -> crate::time::DateTime {
        self.clock.now()
    }

    /// Mounts the volume in partition-table slot `index`.
    ///
    /// The third of the three ways in, and the one for a card written by an
    /// imaging tool: it reads the master boot record and mounts the
    /// partition that slot describes. `index` is the raw table slot, 0 to 3,
    /// rather than a count of the partitions in use — a card whose volume
    /// sits in slot 1 with slot 0 empty is a real arrangement, and numbering
    /// by use would silently mount the wrong one.
    ///
    /// [`crate::mbr::PartitionTable`] is public for the cases this does not cover:
    /// finding the FAT partition when its slot is not known, or handling a
    /// device that might have no table at all.
    ///
    /// The volume is held to the partition's length as well as the device's,
    /// so a boot sector claiming more sectors than the partition it sits in
    /// is refused here even on a device that cannot report its own size.
    ///
    /// # Errors
    ///
    /// [`Error::NoPartitionTable`] when block 0 is not a partition table —
    /// including when it is a boot sector, which is the shape [`mount`]
    /// handles. [`Error::NoSuchPartition`] when the slot is empty or out of
    /// range.
    ///
    /// [`mount`]: Self::mount
    #[cfg(feature = "mbr")]
    #[cfg_attr(docsrs, doc(cfg(feature = "mbr")))]
    pub fn mount_partition(mut device: D, index: usize) -> Result<Self, D::Error> {
        let table =
            crate::mbr::PartitionTable::read(&mut device)?.ok_or(Error::NoPartitionTable)?;
        let partition = *table.get(index).ok_or(Error::NoSuchPartition { index })?;
        if partition.is_protective_gpt() {
            return Err(Error::NoPartitionTable);
        }
        Self::mount_within(
            device,
            partition.first_block,
            Codepage::ASCII,
            Some(partition.blocks),
        )
    }

    /// The volume's validated geometry.
    pub fn boot_sector(&self) -> &BootSector {
        &self.boot
    }

    /// The volume's free-space hints, such as they are.
    pub fn fs_info(&self) -> &FsInfo {
        &self.fs_info
    }

    /// The resident allocation table.
    pub fn fat(&self) -> &Fat {
        &self.fat
    }

    /// The resident allocation table, mutably.
    ///
    /// Allocating or freeing through this changes what the volume records
    /// as in use without changing any directory entry, so the two can be
    /// made to disagree. It exists because the file operations in
    /// [`crate::file`] need it; reach for those instead.
    pub(crate) fn fat_mut(&mut self) -> &mut Fat {
        &mut self.fat
    }

    /// Whether the volume was already marked dirty when this mount found
    /// it.
    ///
    /// True means the last writer did not finish: either it never synced, or
    /// it lost power partway through. It says nothing about what is wrong,
    /// only that something might be — under this crate's write ordering the
    /// likely damage is leaked clusters, which cost space and nothing else,
    /// but the volume may equally have been written by something with weaker
    /// ordering.
    ///
    /// This reports what was on the volume at mount, and does not change
    /// when this mount sets the flag for its own writes.
    pub fn is_dirty(&self) -> bool {
        self.boot.dirty
    }

    /// Marks the volume as having a writer, unless it already is marked.
    ///
    /// Called from [`write_blocks`](Self::write_blocks) rather than from
    /// each operation that mutates, so that it cannot be forgotten by
    /// something added later — and so that it necessarily happens *before*
    /// the write it is warning about, which is the only ordering that makes
    /// the flag mean anything.
    ///
    /// It costs one read and one write, once, on the first change after a
    /// mount or a sync. A volume that is only read is never written to,
    /// which matters for a card that is physically or deliberately
    /// read-only.
    fn mark_dirty(&mut self) -> Result<(), D::Error> {
        if self.marked_dirty {
            return Ok(());
        }
        // Set before the write rather than after, so the flag is already on
        // the volume if the caller's next write is the one interrupted.
        self.marked_dirty = true;
        self.set_dirty_flag(true).inspect_err(|_| {
            // ...and put back if the flag never reached the volume, or
            // every later write would skip this and go to a volume nothing
            // had marked. The caller sees the failure either way; what this
            // protects is the write after the one it retries.
            self.marked_dirty = false;
        })
    }

    /// Writes the volume state byte in the boot sector.
    ///
    /// Goes to the device directly rather than through
    /// [`write_blocks`](Self::write_blocks), which would ask for the flag to
    /// be set as a result of setting it.
    fn set_dirty_flag(&mut self, dirty: bool) -> Result<(), D::Error> {
        const STATE: usize = 0x41;
        const STATE_DIRTY: u8 = 0x01;

        let mut block = [0u8; BLOCK_SIZE];
        self.device
            .read(self.first_block, &mut block)
            .map_err(Error::device)?;
        if dirty {
            block[STATE] |= STATE_DIRTY;
        } else {
            block[STATE] &= !STATE_DIRTY;
        }
        self.device
            .write(self.first_block, &block)
            .map_err(Error::device)
    }

    /// The runs the chain starting at `cluster` occupies.
    ///
    /// Reported through this volume's error type rather than the table's, so
    /// a caller mixing it with reads and writes has one type to handle. Ask
    /// [`fat`](Self::fat) directly for the narrower [`FatError`].
    pub fn runs(&self, cluster: u32) -> Result<Vec<Run>, D::Error> {
        Ok(self.fat.runs(cluster)?)
    }

    /// Reads a run's clusters into `into`, in one device transfer.
    ///
    /// `into` must be exactly the run's length in bytes; anything else is
    /// [`Error::BufferLength`]. One call per run is the property everything
    /// else is arranged to make possible, so this deliberately offers no way
    /// to read a run piecemeal.
    ///
    /// Both ends of the run are checked against the volume, not just the
    /// first. [`Run`]'s fields are public, so a run a caller assembled — as
    /// opposed to one [`runs`](Self::runs) returned — can start inside the
    /// volume and end outside it, and the transfer would then continue off
    /// the end of the data area into whatever follows it. On a partitioned
    /// card that is another filesystem.
    pub fn read_run(&mut self, run: &Run, into: &mut [u8]) -> Result<(), D::Error> {
        // The run is checked before the buffer, and that order is the useful
        // one: the buffer has to be exactly the run's length, so validating
        // it first would oblige a caller holding a nonsensical run to
        // allocate a buffer to match it before being told the run is
        // nonsense. A run of `u32::MAX` clusters would ask for terabytes.
        if run.clusters != 0 {
            if !self.fat.is_valid_cluster(run.start_cluster) {
                return Err(FatError::BadCluster {
                    cluster: run.start_cluster,
                }
                .into());
            }
            // In `u64`, because a run long enough to wrap `u32` is exactly
            // the shape being rejected and must not wrap into a
            // valid-looking one.
            let last = u64::from(run.start_cluster) + u64::from(run.clusters) - 1;
            let reported = last.min(u64::from(u32::MAX)) as u32;
            if last > u64::from(u32::MAX) || !self.fat.is_valid_cluster(reported) {
                return Err(FatError::BadCluster { cluster: reported }.into());
            }
        }

        let expected = run.bytes(self.boot.cluster_bytes());
        if into.len() as u64 != expected {
            return Err(Error::BufferLength {
                expected,
                actual: into.len(),
            });
        }
        if run.clusters == 0 {
            // Nothing to transfer. Returned here rather than falling through,
            // because the device would otherwise be handed a zero-length
            // read — which the trait does not describe and drivers need not
            // accept.
            return Ok(());
        }

        let sector = self.boot.cluster_sector(run.start_cluster);
        self.read_blocks(u64::from(sector), into)
    }

    /// Reads consecutive volume-relative blocks into `into`.
    ///
    /// One device call, unless the device says it cannot move that many
    /// blocks at once — see
    /// [`max_transfer_blocks`](BlockDevice::max_transfer_blocks). Splitting
    /// here rather than inside the device keeps the transfer count
    /// something a test can see.
    pub(crate) fn read_blocks(&mut self, block: u64, into: &mut [u8]) -> Result<(), D::Error> {
        let blocks = into.len() / BLOCK_SIZE;
        let cap = usize::try_from(self.device.max_transfer_blocks())
            .unwrap_or(usize::MAX)
            .max(1);

        if blocks <= cap {
            return self
                .device
                .read(self.first_block + block, into)
                .map_err(Error::device);
        }

        // Past this point `cap < blocks`, so the multiplication below
        // cannot overflow however large a limit the device reported.
        debug_assert!(cap < blocks);

        for (batch, chunk) in into.chunks_mut(cap * BLOCK_SIZE).enumerate() {
            self.device
                .read(self.first_block + block + (batch * cap) as u64, chunk)
                .map_err(Error::device)?;
        }
        Ok(())
    }

    /// Reads the whole chain beginning at `cluster`, one transfer per run.
    ///
    /// Returns every byte of every cluster, so the caller truncates to the
    /// file's recorded size — the chain knows how much space a file
    /// occupies, not how much of it is meaningful.
    ///
    /// The buffer is as large as the chain, and the chain's length comes off
    /// the volume, so this reports [`Error::OutOfMemory`] rather than
    /// aborting when it will not fit — see that variant. A corrupt chain can
    /// be as long as the volume has clusters.
    pub fn read_chain(&mut self, cluster: u32) -> Result<Vec<u8>, D::Error> {
        let runs = self.runs(cluster)?;
        let cluster_bytes = self.boot.cluster_bytes();
        let total: u64 = runs.iter().map(|run| run.bytes(cluster_bytes)).sum();

        let mut data = crate::error::try_filled(0u8, total)?;
        let mut at = 0usize;
        for run in &runs {
            let length = run.bytes(cluster_bytes) as usize;
            self.read_run(run, &mut data[at..at + length])?;
            at += length;
        }
        Ok(data)
    }

    /// The first cluster of the root directory.
    pub fn root_cluster(&self) -> u32 {
        self.boot.root_cluster
    }

    /// The directory whose chain begins at `cluster`, reading and parsing
    /// it if this is the first time it has been asked for.
    ///
    /// One transfer per run of the directory's chain on the first call, and
    /// **no device traffic at all** afterwards. That second property is
    /// what makes opening several files in one directory cheap, and it is
    /// the thing a per-block implementation cannot offer however good its
    /// caching is.
    pub fn read_dir(&mut self, cluster: u32) -> Result<&Directory, D::Error> {
        if !self.directories.contains_key(&cluster) {
            let data = self.read_chain(cluster)?;
            let parsed = Directory::parse(&data, &self.codepage)?;
            self.directories.insert(cluster, parsed);
        }
        Ok(&self.directories[&cluster])
    }

    /// The root directory.
    pub fn root_dir(&mut self) -> Result<&Directory, D::Error> {
        self.read_dir(self.boot.root_cluster)
    }

    /// The directory at `path`, which is `/`-separated and may name either
    /// long or short names.
    ///
    /// A leading slash is optional and `.` components are skipped, so
    /// `/ROMS`, `ROMS` and `./ROMS` are the same directory.
    pub fn open_dir(&mut self, path: &str) -> Result<&Directory, D::Error> {
        let cluster = self.resolve_dir(path)?;
        self.read_dir(cluster)
    }

    /// The first cluster of the directory at `path`.
    ///
    /// The half of [`open_dir`](Self::open_dir) that does not borrow the
    /// cache, so a caller can go on to mutate the directory it found.
    pub(crate) fn resolve_dir(&mut self, path: &str) -> Result<u32, D::Error> {
        let mut cluster = self.boot.root_cluster;

        for component in path.split('/') {
            if component.is_empty() || component == "." {
                continue;
            }

            // The cluster is copied out before the next iteration borrows
            // the cache again, which is also where `..` needs care.
            let next = {
                let directory = self.read_dir(cluster)?;
                let entry = directory.get(component).ok_or_else(|| Error::NotFound {
                    name: component.to_string(),
                })?;
                if !entry.is_directory() {
                    return Err(Error::NotADirectory {
                        name: component.to_string(),
                    });
                }
                entry.first_cluster()
            };

            // `..` in a child of the root holds 0 rather than the root's
            // own cluster number. Following it literally would look up
            // cluster 0, which is reserved.
            cluster = if next == 0 {
                self.boot.root_cluster
            } else {
                next
            };
        }

        Ok(cluster)
    }

    /// Forgets every parsed directory.
    ///
    /// Only needed if something outside this crate has written to the
    /// volume underneath it, which is not a supported arrangement — but
    /// the alternative to offering this is a caller with a stale cache and
    /// no way to say so.
    pub fn forget_directories(&mut self) {
        self.directories.clear();
    }

    /// Forgets one parsed directory, for when it has stopped existing.
    pub(crate) fn forget_directory(&mut self, cluster: u32) {
        self.directories.remove(&cluster);
    }

    /// Writes every change made since the last sync to the device.
    ///
    /// Each dirty stretch of the allocation table is written once per copy
    /// the volume carries, and the free-space hints are refreshed. Until
    /// this is called, allocations and truncations exist only in memory.
    ///
    /// Directory entries are *not* deferred — see the ordering notes on
    /// [`create_with_size`](FileSystem::create_with_size) and
    /// [`truncate`](FileSystem::truncate). They go to the device when they
    /// are written, because their order relative to table changes is what
    /// decides what a power cut leaves behind.
    ///
    /// # The dirty flag
    ///
    /// Finishing clears the volume's dirty flag, so a volume synced before
    /// being put away mounts clean and one dropped without a sync does not.
    /// That flag is the only way an unclean shutdown is detectable at all;
    /// nothing else on the volume records that a writer was interrupted.
    ///
    /// Only the flag *this* mount set is cleared. A volume that arrived
    /// dirty and was never written to keeps its warning: this crate has not
    /// repaired anything, and clearing a flag it did not set would throw
    /// away the one signal that something needs looking at.
    ///
    /// A sync with nothing to write does nothing at all, so calling it more
    /// often than necessary costs nothing.
    pub fn sync(&mut self) -> Result<(), D::Error> {
        if !self.marked_dirty {
            return Ok(());
        }
        self.flush_fat()?;
        self.write_fs_info()?;
        // Last, and only once everything it was warning about has landed.
        self.marked_dirty = false;
        self.set_dirty_flag(false)
    }

    /// Syncs, and gives the block device back.
    ///
    /// The tidy way to finish with a volume. `FileSystem` cannot do this
    /// when it is dropped, because writing to a device can fail and a drop
    /// has nowhere to report that to — so an unsynced volume that is simply
    /// dropped stays marked dirty, which is the correct outcome rather than
    /// a silent one.
    pub fn unmount(mut self) -> Result<D, D::Error> {
        self.sync()?;
        Ok(self.device)
    }

    /// Writes the dirty stretches of the allocation table, to every copy.
    ///
    /// Called by the file operations at the point their ordering requires
    /// it, not only by [`sync`](Self::sync) — and that is the whole reason
    /// it is separate.
    ///
    /// Deferring table changes while writing directory entries straight
    /// through would make the order they reach the *device* the reverse of
    /// the order they happen in memory. An entry naming clusters the
    /// on-disk table still shows as free is precisely the cross-link this
    /// crate's ordering rules exist to prevent: the next allocation would
    /// hand those clusters to another file.
    ///
    /// Batching still does its job, because this collapses everything one
    /// operation touched into one write per stretch per copy. Linking a
    /// thousand-cluster chain is a handful of sectors, not a thousand
    /// writes.
    pub(crate) fn flush_fat(&mut self) -> Result<(), D::Error> {
        let runs = self.fat.dirty_runs();
        if runs.is_empty() {
            return Ok(());
        }
        let starts: Vec<u32> = self.fat.copy_starts().collect();
        for (sector, count) in runs {
            let bytes = self.fat.sector_bytes(sector, count);
            for start in &starts {
                self.write_blocks(u64::from(start + sector), &bytes)?;
            }
        }
        self.fat.clear_dirty();
        Ok(())
    }

    /// Refreshes the volume's free-space hints.
    ///
    /// Best effort by design: these are hints, and a reader that trusts
    /// them over the table is wrong to. Writing them keeps other
    /// implementations from rescanning, and nothing here depends on them.
    fn write_fs_info(&mut self) -> Result<(), D::Error> {
        let Some(sector) = self.boot.fs_info_sector else {
            return Ok(());
        };
        let mut block = [0u8; BLOCK_SIZE];
        self.read_blocks(u64::from(sector), &mut block)?;

        const LEAD_SIGNATURE: u32 = 0x4161_5252;
        const STRUCT_SIGNATURE: u32 = 0x6141_7272;
        let lead = u32::from_le_bytes([block[0], block[1], block[2], block[3]]);
        let structure =
            u32::from_le_bytes([block[0x1E4], block[0x1E5], block[0x1E6], block[0x1E7]]);
        if lead != LEAD_SIGNATURE || structure != STRUCT_SIGNATURE {
            return Ok(());
        }

        block[0x1E8..0x1EC].copy_from_slice(&self.fat.free_clusters().to_le_bytes());
        block[0x1EC..0x1F0].copy_from_slice(&self.fat.next_free_hint().to_le_bytes());
        self.write_blocks(u64::from(sector), &block)?;

        self.fs_info = FsInfo {
            free_clusters: Some(self.fat.free_clusters()),
            next_free: Some(self.fat.next_free_hint()),
        };
        Ok(())
    }

    /// Writes consecutive volume-relative blocks, splitting only where the
    /// device says it must.
    ///
    /// Every change to the volume goes through here, which is why this is
    /// where the dirty flag is set: no operation can forget to.
    pub(crate) fn write_blocks(&mut self, block: u64, from: &[u8]) -> Result<(), D::Error> {
        self.mark_dirty()?;

        let blocks = from.len() / BLOCK_SIZE;
        let cap = usize::try_from(self.device.max_transfer_blocks())
            .unwrap_or(usize::MAX)
            .max(1);

        if blocks <= cap {
            return self
                .device
                .write(self.first_block + block, from)
                .map_err(Error::device);
        }

        debug_assert!(cap < blocks);
        for (batch, chunk) in from.chunks(cap * BLOCK_SIZE).enumerate() {
            self.device
                .write(self.first_block + block + (batch * cap) as u64, chunk)
                .map_err(Error::device)?;
        }
        Ok(())
    }

    /// The block holding directory entry `index` of the directory whose
    /// chain begins at `cluster`, and the entry's offset within it.
    pub(crate) fn entry_location(
        &self,
        cluster: u32,
        index: u32,
    ) -> Result<(u64, usize), D::Error> {
        let block_in_dir = index / SLOTS_PER_BLOCK;
        let offset = (index % SLOTS_PER_BLOCK) as usize * ENTRY_SIZE;

        let sectors_per_cluster = u32::from(self.boot.sectors_per_cluster);
        let cluster_index = block_in_dir / sectors_per_cluster;
        let block_in_cluster = block_in_dir % sectors_per_cluster;

        // Walk the runs rather than the chain: the same information, but
        // already collapsed, so a directory spanning several clusters costs
        // one lookup instead of a step per cluster.
        let runs = self.fat.runs(cluster)?;
        let mut seen = 0u32;
        for run in &runs {
            if cluster_index < seen + run.clusters {
                let target = run.start_cluster + (cluster_index - seen);
                let sector = self.boot.cluster_sector(target) + block_in_cluster;
                return Ok((u64::from(sector), offset));
            }
            seen += run.clusters;
        }
        Err(Error::DirectoryFull)
    }

    /// Reads, changes and writes back one directory entry.
    ///
    /// The whole block is rewritten because a block is the smallest thing a
    /// device will accept — which is also why the read comes first.
    pub(crate) fn edit_entry(
        &mut self,
        directory: u32,
        index: u32,
        edit: impl FnOnce(&mut [u8]),
    ) -> Result<(), D::Error> {
        let (block, offset) = self.entry_location(directory, index)?;
        let mut buffer = [0u8; BLOCK_SIZE];
        self.read_blocks(block, &mut buffer)?;
        edit(&mut buffer[offset..offset + ENTRY_SIZE]);
        self.write_blocks(block, &buffer)?;
        self.directories.remove(&directory);
        Ok(())
    }

    /// Writes a run of consecutive directory slots, starting at `index`.
    ///
    /// One read-modify-write per block the run covers, in ascending order —
    /// which puts the run's *last* entry on the device last, and that is the
    /// ordering the caller needs.
    ///
    /// A name's long-name slots come before the 8.3 entry that terminates
    /// them, so writing in this order means an interruption leaves slots
    /// with no entry after them. Every reader, this one included, treats
    /// those as nothing at all; `fsck.vfat` calls them an orphaned long
    /// name and offers to remove them. The other order would be worse than
    /// untidy: an 8.3 entry written past a still-unwritten run sits after
    /// the directory's end marker, where no reader will look for it.
    pub(crate) fn write_entries(
        &mut self,
        directory: u32,
        index: u32,
        entries: &[[u8; ENTRY_SIZE]],
    ) -> Result<(), D::Error> {
        debug_assert!(!entries.is_empty());
        let last = index + entries.len() as u32 - 1;
        let mut buffer = [0u8; BLOCK_SIZE];

        for block_in_dir in (index / SLOTS_PER_BLOCK)..=(last / SLOTS_PER_BLOCK) {
            let (block, _) = self.entry_location(directory, block_in_dir * SLOTS_PER_BLOCK)?;
            self.read_blocks(block, &mut buffer)?;
            for slot in 0..SLOTS_PER_BLOCK {
                let at = block_in_dir * SLOTS_PER_BLOCK + slot;
                if at < index || at > last {
                    continue;
                }
                let offset = slot as usize * ENTRY_SIZE;
                buffer[offset..offset + ENTRY_SIZE]
                    .copy_from_slice(&entries[(at - index) as usize]);
            }
            self.write_blocks(block, &buffer)?;
        }

        self.directories.remove(&directory);
        Ok(())
    }

    /// Marks slots `first..=last` of `directory` deleted.
    ///
    /// The 8.3 entry at `last` goes first, because that is the one that
    /// makes the file exist; the long-name slots in front of it follow.
    /// Interrupted between the two, the file is gone and its long name is
    /// orphaned — untidy, and reclaimable. The other order would briefly
    /// leave a file whose long name had been deleted out from under it,
    /// which is a file appearing under a name nobody gave it.
    pub(crate) fn delete_entries(
        &mut self,
        directory: u32,
        first: u32,
        last: u32,
    ) -> Result<(), D::Error> {
        // Before the write, so a handle is never usable against a slot that
        // has already been freed on the device. Counted even if the writes
        // below fail: the 8.3 entry goes first, so a failure part-way has
        // already freed the slot that matters.
        *self.deletions.entry(directory).or_insert(0) += 1;

        self.edit_entry(directory, last, crate::dir::mark_deleted)?;
        for index in (first..last).rev() {
            self.edit_entry(directory, index, crate::dir::mark_deleted)?;
        }
        Ok(())
    }

    /// How many entries have been deleted from `directory` under this mount.
    ///
    /// Stamped onto a [`File`](crate::File) when it is opened and compared
    /// when it is written through; see the field this reads.
    pub(crate) fn deletions(&self, directory: u32) -> u32 {
        self.deletions.get(&directory).copied().unwrap_or(0)
    }

    /// The first of `count` consecutive free slots in `directory`, growing
    /// it if it has no run that long.
    ///
    /// The search itself touches no device at all: the free runs were
    /// recorded when the directory was parsed. A long name needs a run of up
    /// to 21 slots, and looking for one by reading the directory back a
    /// block at a time would cost a transfer per block for every file
    /// created.
    pub(crate) fn free_slots(&mut self, directory: u32, count: u32) -> Result<u32, D::Error> {
        for _ in 0..MAX_DIRECTORY_GROWTH {
            if let Some(index) = self.read_dir(directory)?.free_run(count) {
                return Ok(index);
            }
            self.grow_directory(directory)?;
        }
        // One last look, after the final growth. Without it the loop would
        // add a cluster and then give up without checking whether it helped,
        // leaving the directory permanently one cluster larger for nothing.
        self.read_dir(directory)?
            .free_run(count)
            .ok_or(Error::DirectoryFull)
    }

    /// Adds one cluster to a directory's chain.
    ///
    /// The order is load-bearing, and it is allocate, zero, *then* link:
    ///
    ///   * allocating without linking reserves the cluster so nothing else
    ///     can take it, while leaving it reachable from nothing;
    ///   * zeroing it makes it an empty directory rather than whatever the
    ///     last file to own those bytes left behind, since stale bytes in a
    ///     directory read as entries;
    ///   * linking last is what makes it part of the directory.
    ///
    /// Interrupted anywhere, this leaks one cluster. Linking before zeroing
    /// would instead give the directory a cluster full of nonsense, which is
    /// not a leak but corruption.
    ///
    /// # What this does not do
    ///
    /// The link reaches the *device* later, not here: [`Fat::set_entry`]
    /// only marks the table's sector dirty. Publishing is the caller's
    /// [`flush_fat`](Self::flush_fat), and both callers
    /// ([`create_with_size`](FileSystem::create_with_size) and
    /// [`create_dir`](FileSystem::create_dir)) flush before writing the
    /// entry that names the new slots — which is what keeps a directory
    /// entry from pointing into a cluster the on-disk table still shows as
    /// free. A caller added later that grows a directory and writes into it
    /// without flushing between would break that, and this is the note
    /// saying so.
    fn grow_directory(&mut self, directory: u32) -> Result<(), D::Error> {
        let runs = self.fat.runs(directory)?;
        let last = runs
            .last()
            .map(|run| run.start_cluster + run.clusters - 1)
            .ok_or(Error::DirectoryFull)?;

        let added = self.fat.allocate(1)?;
        let new_cluster = added[0].start_cluster;

        let sectors_per_cluster = usize::from(self.boot.sectors_per_cluster);
        let zeros = vec![0u8; sectors_per_cluster * BLOCK_SIZE];
        let sector = self.boot.cluster_sector(new_cluster);
        self.write_blocks(u64::from(sector), &zeros)?;

        self.fat.set_entry(last, new_cluster)?;

        self.directories.remove(&directory);
        Ok(())
    }

    /// The block device this volume sits on.
    pub fn device(&self) -> &D {
        &self.device
    }

    /// The block device, mutably.
    ///
    /// Reaching past the filesystem to its device is a way to corrupt a
    /// mounted volume, so this exists for devices that carry their own
    /// state worth reaching — an instrumented one that counts transfers,
    /// say — rather than for writing blocks behind the filesystem's back.
    pub fn device_mut(&mut self) -> &mut D {
        &mut self.device
    }

    /// Gives the block device back.
    pub fn into_device(self) -> D {
        self.device
    }
}

/// Shifts a device's block numbers by a volume's starting offset.
///
/// Exists so [`Fat::load`] can be written against plain volume-relative
/// block numbers rather than threading the partition offset through every
/// call that reads the table.
struct RelativeDevice<'d, D> {
    device: &'d mut D,
    first_block: u64,
}

impl<'d, D> RelativeDevice<'d, D> {
    fn new(device: &'d mut D, first_block: u64) -> Self {
        Self {
            device,
            first_block,
        }
    }
}

impl<D: BlockDevice> BlockDevice for RelativeDevice<'_, D> {
    type Error = D::Error;

    fn read(&mut self, start_block: u64, blocks: &mut [u8]) -> core::result::Result<(), D::Error> {
        self.device.read(self.first_block + start_block, blocks)
    }

    fn write(&mut self, start_block: u64, blocks: &[u8]) -> core::result::Result<(), D::Error> {
        self.device.write(self.first_block + start_block, blocks)
    }

    fn block_count(&mut self) -> core::result::Result<Option<u64>, D::Error> {
        self.device.block_count()
    }

    /// Forwarded, not defaulted. A wrapper that let this fall back to the
    /// provided implementation would report "no limit" for a device that
    /// has one, and the transfer it then permitted would fail at the
    /// hardware.
    fn max_transfer_blocks(&self) -> u64 {
        self.device.max_transfer_blocks()
    }
}
