//! The file allocation table, held in memory.
//!
//! This is the module the crate is named after. Everything else follows
//! from the table being an array rather than something to be fetched a
//! sector at a time.
//!
//! # Changes are batched, but not indefinitely
//!
//! Changing an entry marks its sector dirty and nothing more. A dirty
//! stretch reaches the device once, per copy of the table, when something
//! flushes it.
//!
//! That batching is a large part of why this design is quick to write to.
//! Allocating a megabyte-sized file rewrites the same handful of sectors
//! repeatedly as the chain is linked; an implementation that wrote through
//! would issue a device write per entry, doubled because a volume normally
//! carries two copies of the table.
//!
//! What batching cannot do is outlast an ordering boundary. Directory
//! entries are written straight through, so leaving table changes in memory
//! across one would make the order they reach the *device* the reverse of
//! the order they happened — publishing an entry that names clusters the
//! on-disk table still shows as free. The file operations therefore flush
//! at those points; see
//! [`FileSystem::flush_fat`](crate::FileSystem::sync). Batching within an
//! operation is kept, which is where nearly all of the saving is.

use alloc::vec;
use alloc::vec::Vec;

use crate::blockdev::{BLOCK_SIZE, BlockDevice};
use crate::boot::{BootSector, FIRST_CLUSTER};
use crate::error::{Error, FatError};

/// Entries at or above this value end a chain.
const END_OF_CHAIN: u32 = 0x0FFF_FFF8;
/// What this crate writes to end a chain.
const END_OF_CHAIN_MARK: u32 = 0x0FFF_FFFF;
/// A cluster the medium reported as unusable.
const BAD_CLUSTER: u32 = 0x0FFF_FFF7;
/// An unallocated entry.
const FREE: u32 = 0;
/// Only the low 28 bits of a FAT32 entry are the cluster number. The top
/// four are reserved, and are preserved across a write rather than
/// normalised away — see [`Fat::load`].
const ENTRY_MASK: u32 = 0x0FFF_FFFF;

/// Blocks per device read while loading the table.
///
/// The table is read in a few large transfers rather than one enormous one
/// so the transient buffer stays bounded — a 32 GB volume with 4 KB
/// clusters has a 32 MB table, and doubling that at mount to hold both the
/// bytes and the parsed entries is avoidable. 2048 blocks is 1 MB per
/// transfer, which is far into the region where per-command overhead has
/// stopped mattering.
const LOAD_CHUNK_BLOCKS: usize = 2048;

/// One contiguous stretch of a cluster chain.
///
/// The unit the rest of the crate moves data in. A file occupying a single
/// run is read or written in one device transfer whatever its size, which
/// is the entire point of keeping the table resident: the run is only
/// discoverable because walking the chain costs nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Run {
    /// Index of this run's first cluster within the file, counting from 0.
    pub file_cluster: u32,
    /// The first cluster on the volume.
    pub start_cluster: u32,
    /// How many consecutive clusters the run covers.
    pub clusters: u32,
}

impl Run {
    /// The run's length in bytes, given the volume's cluster size.
    pub fn bytes(&self, cluster_bytes: u32) -> u64 {
        u64::from(self.clusters) * u64::from(cluster_bytes)
    }
}

/// The file allocation table, resident in memory.
///
/// Indexed by cluster number, so entries 0 and 1 exist but are reserved and
/// never part of a chain.
///
/// # Reading, not writing
///
/// The public surface here is the read side — walking a chain, collapsing it
/// into runs, asking how much room is left. That is what
/// [`FileSystem::fat`](crate::FileSystem::fat) hands out, and it is enough to
/// answer questions about a volume's layout without going near the device.
///
/// Allocating, freeing and linking are deliberately not public. They change
/// what the volume records as in use without touching a single directory
/// entry, so the two can be driven out of agreement — and the write-back
/// that would make such a change durable is not public either, which would
/// leave a caller holding a table it had edited and could not sync. The file
/// operations on [`FileSystem`](crate::FileSystem) are where allocation
/// belongs, because they own both halves.
#[derive(Debug, Clone)]
pub struct Fat {
    /// Raw entries, reserved bits and all.
    entries: Vec<u32>,
    cluster_count: u32,
    /// Which sectors of the table have changed since the last sync.
    dirty: Vec<bool>,
    /// Where to start looking for a free cluster. Next-fit: allocation
    /// resumes where the last one stopped rather than rescanning from the
    /// beginning, which keeps consecutive allocations adjacent and so
    /// keeps files contiguous.
    next_free: u32,
    free_count: u32,
    entries_per_sector: u32,
    bytes_per_sector: u32,
    sectors_per_fat: u32,
    fat_count: u8,
    fat_start: u32,
}

impl Fat {
    /// Reads the whole table from the volume.
    ///
    /// Only as many entries as the volume has clusters, which is usually
    /// fewer than the table has room for — the tail is padding, and
    /// reading it would cost time and memory for entries no valid cluster
    /// number can reach.
    ///
    /// Entries are stored **raw**. The top four bits of a FAT32 entry are
    /// reserved, and masking them off here would mean writing zeros back
    /// over whatever the formatter put there. Interpretation masks
    /// instead; see [`Self::entry`].
    pub fn load<D: BlockDevice>(
        device: &mut D,
        boot: &BootSector,
    ) -> crate::error::Result<Self, D::Error> {
        let entry_count = boot.cluster_count + FIRST_CLUSTER;
        let bytes_needed = u64::from(entry_count) * 4;
        let blocks_needed = bytes_needed.div_ceil(BLOCK_SIZE as u64);

        // A volume whose table will not fit in memory is a legitimate
        // outcome on a small host, and it should say so rather than aborting
        // the process. `OutOfMemory` and not `BadGeometry`: the volume is
        // fine, this host is small, and the two send whoever reads the
        // message in opposite directions -- one to reformat a card that
        // never needed it, the other to a board with more RAM.
        let mut entries: Vec<u32> = Vec::new();
        entries
            .try_reserve_exact(entry_count as usize)
            .map_err(|_| Error::OutOfMemory {
                bytes: bytes_needed,
            })?;

        // Never more than the device will move in one call. Without this,
        // mounting a volume on hardware with a real transfer limit fails
        // before any of the rest of the crate is reached.
        let chunk_blocks = usize::try_from(device.max_transfer_blocks())
            .unwrap_or(usize::MAX)
            .clamp(1, LOAD_CHUNK_BLOCKS);

        let mut buffer = vec![0u8; chunk_blocks * BLOCK_SIZE];
        let mut block = 0u64;
        while block < blocks_needed {
            let blocks = chunk_blocks.min((blocks_needed - block) as usize);
            let chunk = &mut buffer[..blocks * BLOCK_SIZE];
            device
                .read(u64::from(boot.fat_start) + block, chunk)
                .map_err(Error::device)?;

            for word in chunk.chunks_exact(4) {
                if entries.len() == entry_count as usize {
                    break;
                }
                entries.push(u32::from_le_bytes([word[0], word[1], word[2], word[3]]));
            }
            block += blocks as u64;
        }

        let free_count = entries[FIRST_CLUSTER as usize..]
            .iter()
            .filter(|&&entry| entry & ENTRY_MASK == FREE)
            .count() as u32;

        // One flag per sector of the table, and `sectors_per_fat` is a
        // 32-bit field off the volume that nothing above bounds tightly --
        // only that the tables end before the data does. A boot sector
        // claiming a table far larger than its cluster count needs gets
        // past every check in `parse` and lands here, so this allocation is
        // as attacker-shaped as the entries above and is made the same way.
        let dirty = crate::error::try_filled(false, u64::from(boot.sectors_per_fat))?;

        Ok(Fat {
            entries,
            cluster_count: boot.cluster_count,
            dirty,
            next_free: FIRST_CLUSTER,
            free_count,
            entries_per_sector: u32::from(boot.bytes_per_sector) / 4,
            bytes_per_sector: u32::from(boot.bytes_per_sector),
            sectors_per_fat: boot.sectors_per_fat,
            fat_count: boot.fat_count,
            fat_start: boot.fat_start,
        })
    }

    /// How many clusters the volume has.
    pub fn cluster_count(&self) -> u32 {
        self.cluster_count
    }

    /// Whether `cluster` is one this volume can address.
    pub fn is_valid_cluster(&self, cluster: u32) -> bool {
        cluster >= FIRST_CLUSTER && cluster < FIRST_CLUSTER + self.cluster_count
    }

    /// The entry for `cluster`, masked to its significant bits.
    ///
    /// The narrow read accessor everything else goes through. Keeping table
    /// access to one read and one write is what would make a 12- or 16-bit
    /// on-disk format a matter of packing rather than a change threaded
    /// through the whole crate.
    pub fn entry(&self, cluster: u32) -> Result<u32, FatError> {
        if !self.is_valid_cluster(cluster) {
            return Err(FatError::BadCluster { cluster });
        }
        Ok(self.entries[cluster as usize] & ENTRY_MASK)
    }

    /// Sets the entry for `cluster`, preserving its reserved bits.
    ///
    /// The narrow write accessor. Marks the sector dirty; nothing reaches
    /// the device until sync.
    pub(crate) fn set_entry(&mut self, cluster: u32, value: u32) -> Result<(), FatError> {
        if !self.is_valid_cluster(cluster) {
            return Err(FatError::BadCluster { cluster });
        }
        let at = cluster as usize;
        let was_free = self.entries[at] & ENTRY_MASK == FREE;
        let now_free = value & ENTRY_MASK == FREE;

        self.entries[at] = (self.entries[at] & !ENTRY_MASK) | (value & ENTRY_MASK);
        self.mark_dirty(cluster);

        match (was_free, now_free) {
            (true, false) => self.free_count -= 1,
            (false, true) => self.free_count += 1,
            _ => {}
        }
        Ok(())
    }

    /// The cluster after `cluster`, or `None` at the end of the chain.
    pub fn next(&self, cluster: u32) -> Result<Option<u32>, FatError> {
        let entry = self.entry(cluster)?;
        if entry >= END_OF_CHAIN {
            return Ok(None);
        }
        if entry == BAD_CLUSTER || !self.is_valid_cluster(entry) {
            return Err(FatError::BadCluster { cluster: entry });
        }
        Ok(Some(entry))
    }

    /// Walks the chain beginning at `start`, collapsing it into runs.
    ///
    /// Bounded by the volume's cluster count, so a chain that loops errors
    /// rather than spinning. That bound matters more here than it would in
    /// an implementation that reads the table from the device: there is no
    /// I/O to slow a runaway walk down.
    pub fn runs(&self, start: u32) -> Result<Vec<Run>, FatError> {
        let mut runs: Vec<Run> = Vec::new();
        let mut cluster = start;
        let mut index = 0u32;

        loop {
            if !self.is_valid_cluster(cluster) {
                return Err(FatError::BadCluster { cluster });
            }
            if index > self.cluster_count {
                return Err(FatError::ChainLoop { start });
            }

            match runs.last_mut() {
                Some(run) if run.start_cluster + run.clusters == cluster => run.clusters += 1,
                _ => runs.push(Run {
                    file_cluster: index,
                    start_cluster: cluster,
                    clusters: 1,
                }),
            }
            index += 1;

            let entry = self.entries[cluster as usize] & ENTRY_MASK;
            if entry >= END_OF_CHAIN {
                return Ok(runs);
            }
            if entry == FREE {
                // Not the end of the file. A chain ends with a marker, so a
                // free entry inside one means the table and the directory
                // disagree about what is allocated.
                return Err(FatError::FreeClusterInChain { start, cluster });
            }
            if entry == BAD_CLUSTER || !self.is_valid_cluster(entry) {
                return Err(FatError::BadCluster { cluster: entry });
            }
            cluster = entry;
        }
    }

    /// How many clusters the chain from `start` occupies.
    pub fn chain_length(&self, start: u32) -> Result<u32, FatError> {
        Ok(self.runs(start)?.iter().map(|run| run.clusters).sum())
    }

    /// How many clusters are free.
    ///
    /// Maintained as entries change rather than counted on demand, so
    /// answering "is there room" needs no scan.
    pub fn free_clusters(&self) -> u32 {
        self.free_count
    }

    // -----------------------------------------------------------------
    // Allocation
    // -----------------------------------------------------------------

    /// Allocates `count` clusters as a linked chain, ending in an
    /// end-of-chain marker.
    ///
    /// Prefers a single contiguous run, which is what makes a file written
    /// in one go readable in one device transfer. Falls back to as few runs
    /// as the free space allows.
    ///
    /// Nothing is linked to an existing chain here — see [`Self::append`],
    /// which does link and therefore does have to undo itself. A partial
    /// failure is not reachable from this one: the only way [`set_entry`] can
    /// fail is a cluster outside the volume, and both finders below return
    /// clusters they located by scanning inside it. Should that ever stop
    /// being true, what is left behind is clusters marked used and reachable
    /// from nothing — a leak, which is the failure this whole ordering
    /// prefers.
    ///
    /// [`set_entry`]: Self::set_entry
    pub(crate) fn allocate(&mut self, count: u32) -> Result<Vec<Run>, FatError> {
        if count == 0 {
            return Ok(Vec::new());
        }
        if self.free_count < count {
            return Err(FatError::DiskFull {
                wanted: count,
                free: self.free_count,
            });
        }

        let runs = match self.find_contiguous(count) {
            Some(start) => vec![Run {
                file_cluster: 0,
                start_cluster: start,
                clusters: count,
            }],
            None => self.find_scattered(count)?,
        };

        // Reserve before linking, so a cluster cannot be handed out twice
        // and so the chain is never visible pointing at free space.
        for run in &runs {
            for cluster in run.start_cluster..run.start_cluster + run.clusters {
                self.set_entry(cluster, END_OF_CHAIN_MARK)?;
            }
        }

        // Then link, leaving the last entry as the marker it already is.
        let mut previous: Option<u32> = None;
        for run in &runs {
            for cluster in run.start_cluster..run.start_cluster + run.clusters {
                if let Some(before) = previous {
                    self.set_entry(before, cluster)?;
                }
                previous = Some(cluster);
            }
        }

        // Where the next search resumes. Wrapped rather than left one past
        // the last cluster: the searches below treat it as a starting point
        // and cope either way, but `next_free_hint` is written to the
        // volume's free-space hints, and a cluster number outside the volume
        // is not a legal value to leave there for another implementation to
        // read.
        let end = FIRST_CLUSTER + self.cluster_count;
        self.next_free = match runs.last() {
            Some(run) if run.start_cluster + run.clusters < end => run.start_cluster + run.clusters,
            _ => FIRST_CLUSTER,
        };
        Ok(runs)
    }

    /// Allocates `count` more clusters and links them onto the chain ending
    /// at `last_cluster`.
    ///
    /// The link is the **last** thing written. Until it is, the new
    /// clusters are allocated and terminated but reachable from nothing, so
    /// an interruption leaks them rather than leaving a chain that points
    /// into space the allocator still thinks is free.
    pub(crate) fn append(&mut self, last_cluster: u32, count: u32) -> Result<Vec<Run>, FatError> {
        let runs = self.allocate(count)?;
        if let Some(first) = runs.first() {
            if let Err(error) = self.set_entry(last_cluster, first.start_cluster) {
                // Give back what this call took: the caller's chain is
                // untouched, so leaving these allocated would leak them for
                // no reason.
                for run in &runs {
                    for cluster in run.start_cluster..run.start_cluster + run.clusters {
                        let _ = self.set_entry(cluster, FREE);
                    }
                }
                return Err(error);
            }
        }
        Ok(runs)
    }

    /// Frees every cluster in the chain beginning at `start`.
    ///
    /// Returns how many were freed.
    pub(crate) fn free_chain(&mut self, start: u32) -> Result<u32, FatError> {
        let runs = self.runs(start)?;
        let mut freed = 0;
        for run in &runs {
            for cluster in run.start_cluster..run.start_cluster + run.clusters {
                self.set_entry(cluster, FREE)?;
                freed += 1;
            }
            self.next_free = self.next_free.min(run.start_cluster);
        }
        Ok(freed)
    }

    /// Shortens the chain from `start` to `keep` clusters, freeing the rest.
    ///
    /// The new end of chain is marked **before** the tail is freed, so an
    /// interruption between the two leaves clusters that belong to nothing
    /// — a leak — rather than a chain reaching into clusters the allocator
    /// has already given away.
    pub(crate) fn truncate_chain(&mut self, start: u32, keep: u32) -> Result<(), FatError> {
        if keep == 0 {
            self.free_chain(start)?;
            return Ok(());
        }

        let runs = self.runs(start)?;
        let total: u32 = runs.iter().map(|run| run.clusters).sum();
        if keep >= total {
            return Ok(());
        }

        // Locate the cluster that becomes the last, and the one after it.
        //
        // `keep` is at least 1 and fewer than the chain holds, so the cluster
        // at index `keep - 1` is always found and `last` is always
        // overwritten below. It starts at `start`, which is that cluster when
        // `keep` is 1 — so the initial value is the right answer rather than
        // a placeholder standing in for an error that cannot happen.
        let mut walked = 0u32;
        let mut last = start;
        let mut first_freed = None;
        for run in &runs {
            for offset in 0..run.clusters {
                let cluster = run.start_cluster + offset;
                if walked == keep - 1 {
                    last = cluster;
                } else if walked == keep {
                    first_freed = Some(cluster);
                }
                walked += 1;
            }
        }

        self.set_entry(last, END_OF_CHAIN_MARK)?;
        if let Some(tail) = first_freed {
            self.free_chain(tail)?;
        }
        Ok(())
    }

    /// The first cluster of a free run of `count` consecutive clusters.
    ///
    /// Next-fit with one wrap: the search starts where the last allocation
    /// ended, so files written one after another end up adjacent, and only
    /// falls back to the beginning of the volume when that fails.
    fn find_contiguous(&self, count: u32) -> Option<u32> {
        let end = FIRST_CLUSTER + self.cluster_count;
        for (from, to) in [(self.next_free, end), (FIRST_CLUSTER, self.next_free)] {
            let mut run_start = None;
            let mut run_len = 0u32;
            for cluster in from..to {
                if self.entries[cluster as usize] & ENTRY_MASK == FREE {
                    run_start.get_or_insert(cluster);
                    run_len += 1;
                    if run_len == count {
                        return run_start;
                    }
                } else {
                    run_start = None;
                    run_len = 0;
                }
            }
        }
        None
    }

    /// As many runs as it takes to gather `count` free clusters.
    ///
    /// Only reached when no single run is long enough. Takes the free
    /// clusters it finds in volume order, coalescing neighbours, which
    /// yields the fewest runs available without a search for the best fit
    /// — a search that would cost a second pass to save fragments that the
    /// next allocation would use anyway.
    fn find_scattered(&self, count: u32) -> Result<Vec<Run>, FatError> {
        let end = FIRST_CLUSTER + self.cluster_count;
        let mut runs: Vec<Run> = Vec::new();
        let mut taken = 0u32;

        for (from, to) in [(self.next_free, end), (FIRST_CLUSTER, self.next_free)] {
            for cluster in from..to {
                if self.entries[cluster as usize] & ENTRY_MASK != FREE {
                    continue;
                }
                match runs.last_mut() {
                    Some(run) if run.start_cluster + run.clusters == cluster => run.clusters += 1,
                    _ => runs.push(Run {
                        file_cluster: taken,
                        start_cluster: cluster,
                        clusters: 1,
                    }),
                }
                taken += 1;
                if taken == count {
                    return Ok(runs);
                }
            }
        }

        Err(FatError::DiskFull {
            wanted: count,
            free: taken,
        })
    }

    // -----------------------------------------------------------------
    // Writing back
    // -----------------------------------------------------------------

    fn mark_dirty(&mut self, cluster: u32) {
        let sector = cluster / self.entries_per_sector;
        if let Some(flag) = self.dirty.get_mut(sector as usize) {
            *flag = true;
        }
    }

    /// Whether the table holds changes that have not reached the device.
    ///
    /// Not to be confused with
    /// [`FileSystem::is_dirty`](crate::FileSystem::is_dirty), which answers a
    /// different question: whether the *volume* carries the on-disk flag
    /// saying some writer did not finish. This one is about memory and clears
    /// on every flush; that one is about the card and survives power loss.
    /// The names used to be the same, which made the pair a trap.
    pub fn has_unsynced_changes(&self) -> bool {
        self.dirty.iter().any(|&flag| flag)
    }

    /// Dirty sectors, coalesced into `(first sector, count)` stretches.
    ///
    /// Coalescing is what turns a scattered set of changes into a few large
    /// writes. Linking a chain touches the same sector repeatedly and
    /// neighbouring sectors in order, so in practice a large allocation
    /// comes out as one stretch.
    pub(crate) fn dirty_runs(&self) -> Vec<(u32, u32)> {
        let mut runs: Vec<(u32, u32)> = Vec::new();
        for (sector, &flag) in self.dirty.iter().enumerate() {
            if !flag {
                continue;
            }
            let sector = sector as u32;
            match runs.last_mut() {
                Some((start, count)) if *start + *count == sector => *count += 1,
                _ => runs.push((sector, 1)),
            }
        }
        runs
    }

    /// Serialises `count` sectors of the table, starting at `sector`.
    pub(crate) fn sector_bytes(&self, sector: u32, count: u32) -> Vec<u8> {
        let mut out = vec![0u8; (count * self.bytes_per_sector) as usize];
        let first = sector * self.entries_per_sector;
        for n in 0..count * self.entries_per_sector {
            let entry = self
                .entries
                .get((first + n) as usize)
                .copied()
                .unwrap_or(FREE);
            let at = (n * 4) as usize;
            out[at..at + 4].copy_from_slice(&entry.to_le_bytes());
        }
        out
    }

    /// Forgets which sectors were dirty, after they have been written.
    pub(crate) fn clear_dirty(&mut self) {
        self.dirty.fill(false);
    }

    /// Where each copy of the table starts, in volume-relative sectors.
    pub(crate) fn copy_starts(&self) -> impl Iterator<Item = u32> + '_ {
        (0..u32::from(self.fat_count)).map(move |copy| self.fat_start + copy * self.sectors_per_fat)
    }

    /// Where to resume looking for free space, for the volume's hints.
    pub(crate) fn next_free_hint(&self) -> u32 {
        self.next_free
    }
}
