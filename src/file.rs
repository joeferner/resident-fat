//! Reading files.
//!
//! # Where the bytes go
//!
//! A read is split into three parts: a partial first block, a middle of
//! whole blocks, and a partial last block. The middle is transferred
//! **straight into the caller's buffer** — no intermediate copy, no
//! per-block loop — and only the ragged ends go through a one-block
//! scratch buffer.
//!
//! That split is why the scratch buffer is one block regardless of how much
//! is being read. The obvious alternative, reading every block the request
//! touches into a temporary and copying out of it, is one device call
//! instead of up to three, but it needs a temporary as large as the read
//! and copies every byte twice. For the megabyte-scale reads this crate is
//! built for, two extra calls are cheaper than either.
//!
//! An aligned read — which is what reading a whole file is — has no ragged
//! ends at all, so it is one transfer per run and nothing is copied.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use crate::blockdev::{BLOCK_SIZE, BlockDevice};
use crate::dir::{Attributes, ENTRY_SIZE};
use crate::error::{Error, FatError, Result};
use crate::fs::FileSystem;
use crate::name::StoredName;
use crate::time::DateTime;

/// Everything needed to read a file: where its data starts, and how much of
/// it means anything.
///
/// Plain `Copy` data holding no borrow, so a caller can keep as many of
/// these as it likes and still use the filesystem. There is no handle
/// table to size at compile time and no position to get out of step —
/// offsets are passed to each read instead.
///
/// # Staleness
///
/// The trade for holding no borrow is that a handle cannot be kept up to
/// date. It records where its directory entry lives, and removing a file
/// frees that slot for the next one created in the directory — so a handle
/// outlives the file it names, and the entry it points at can come to belong
/// to something else.
///
/// Every read and every write checks for this and reports
/// [`Error::StaleFile`] rather than acting on the wrong entry. The rule a
/// caller can rely on: **a handle is invalidated by any deletion from its
/// directory**, including the one [`write_file`](FileSystem::write_file)
/// performs when it replaces an existing file. Deleting a sibling therefore
/// invalidates it too, which is a false positive and the deliberate
/// direction to err in — reopening the path costs a lookup in a resident
/// directory, where the alternative is silently rewriting whichever file was
/// given the slot.
///
/// The check costs no device traffic: it compares against the parsed
/// directory, which is already resident.
///
/// Handles do not survive a remount, and nothing makes that a type error —
/// `File` carries no lifetime tying it to the volume it came from. Reusing
/// one against a second mount is caught in the ordinary case and is not
/// something to rely on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct File {
    first_cluster: u32,
    size: u32,
    /// Where the file's directory entry lives, so a write can update the
    /// recorded length and starting cluster without searching for it again.
    directory: u32,
    index: u32,
    /// The first slot the file's name occupies, which is `index` unless it
    /// has a long name in front of it. Deleting has to free the whole run.
    first_slot: u32,
    /// The 8.3 name the entry at `index` held when this handle was made.
    ///
    /// Carried so a write can tell that the slot is still this file's before
    /// rewriting it — see the note on staleness above. The 8.3 name is what
    /// is compared rather than the long one because every entry has one, and
    /// it is the eleven bytes that physically occupy the slot.
    short_name: crate::dir::ShortName,
    /// How many entries had been deleted from `directory` when this handle
    /// was made.
    ///
    /// The name alone cannot detect a file replaced by another of the same
    /// name, which is what happens on every
    /// [`write_file`](FileSystem::write_file) over an existing path. This
    /// moves whenever a slot in the directory is freed, so a handle from
    /// before the replacement does not match one from after it.
    deletions: u32,
}

impl File {
    pub(crate) fn new(
        first_cluster: u32,
        size: u32,
        directory: u32,
        index: u32,
        first_slot: u32,
        short_name: crate::dir::ShortName,
        deletions: u32,
    ) -> Self {
        File {
            first_cluster,
            size,
            directory,
            index,
            first_slot,
            short_name,
            deletions,
        }
    }

    /// The file's 8.3 name, as its directory entry stores it.
    pub fn short_name(&self) -> &crate::dir::ShortName {
        &self.short_name
    }

    /// The first cluster of the file's data, or zero if it has none.
    pub fn first_cluster(&self) -> u32 {
        self.first_cluster
    }

    /// The file's length in bytes.
    ///
    /// The length from the directory entry, not the space the file
    /// occupies: a cluster chain is a whole number of clusters, and the
    /// slack in the last one is not part of the file.
    pub fn len(&self) -> u32 {
        self.size
    }

    /// Whether the file has no content.
    pub fn is_empty(&self) -> bool {
        self.size == 0
    }
}

impl<D: BlockDevice> FileSystem<D> {
    /// Opens the file at `path`.
    ///
    /// `/`-separated, matching either long or short names, case
    /// insensitively — the same resolution
    /// [`open_dir`](FileSystem::open_dir) does.
    pub fn open(&mut self, path: &str) -> Result<File, D::Error> {
        let (parent, name) = split_path(path);
        let cluster = self.resolve_dir(parent)?;
        let directory = self.read_dir(cluster)?;
        let entry = directory.get(name).ok_or_else(|| Error::NotFound {
            name: String::from(name),
        })?;
        if entry.is_directory() {
            return Err(Error::IsADirectory {
                name: String::from(name),
            });
        }
        // Copied out so the borrow of the parsed directory ends here, which
        // is what lets the deletion count below be read from `self`.
        let (first_cluster, size, index, first_slot, short_name) = (
            entry.first_cluster(),
            entry.len(),
            entry.index(),
            entry.first_slot(),
            *entry.short_name(),
        );

        Ok(File::new(
            first_cluster,
            size,
            cluster,
            index,
            first_slot,
            short_name,
            self.deletions(cluster),
        ))
    }

    /// Refuses a handle whose directory slot has stopped being its own.
    ///
    /// Guards every operation that writes. The damage it prevents is not to
    /// the file named by the handle — that one is gone — but to whichever
    /// file has since been given its slot: without this, writing through a
    /// stale handle rewrites that entry's length and starting cluster, and
    /// truncates a file the caller never opened.
    ///
    /// Costs no device traffic in the case that matters. The parsed
    /// directory is resident and was read to produce the handle in the first
    /// place, so this is a binary search in memory — which is why the check
    /// can be unconditional rather than something a caller opts into.
    ///
    /// # Two tests, because one is not enough
    ///
    /// The deletion count catches a slot that was freed and retaken, which
    /// the name cannot when the new occupant has the same name — the case
    /// [`write_file`](Self::write_file) produces every time it replaces a
    /// file.
    ///
    /// The name catches what the count cannot: a handle whose recorded slot
    /// never belonged to it in the first place, which is the shape a handle
    /// carried across mounts has, since the counts of a fresh mount all start
    /// at zero.
    fn check_live(&mut self, file: &File) -> Result<(), D::Error> {
        let deletions = self.deletions(file.directory);
        let live = deletions == file.deletions
            && self
                .read_dir(file.directory)?
                .at(file.index)
                .is_some_and(|entry| entry.short_name().as_bytes() == file.short_name.as_bytes());
        if live {
            return Ok(());
        }
        Err(Error::StaleFile {
            name: file.short_name.to_display_string_with(self.codepage()),
        })
    }

    /// Reads the whole file.
    ///
    /// Exactly [`File::len`] bytes — the slack in the last cluster is not
    /// part of the file and does not come back. One device transfer per
    /// run of the file's chain, so a contiguous file of any size is one
    /// call.
    ///
    /// The whole file is buffered, and FAT32 allows one of nearly 4 GiB, so
    /// this reports [`Error::OutOfMemory`] rather than aborting when the
    /// host cannot hold it. [`read_at`](Self::read_at) is the way to take a
    /// large file in pieces the caller sizes.
    pub fn read_all(&mut self, file: &File) -> Result<Vec<u8>, D::Error> {
        let mut data = crate::error::try_filled(0u8, u64::from(file.size))?;
        let read = self.read_at(file, 0, &mut data)?;
        debug_assert_eq!(read, data.len());
        Ok(data)
    }

    /// Reads from `offset` into `into`, returning how many bytes arrived.
    ///
    /// Short only at the end of the file: a read starting at or past
    /// [`File::len`] returns zero, and one that would run past the end
    /// stops there. Anything else short would be a device error instead.
    pub fn read_at(
        &mut self,
        file: &File,
        offset: u64,
        into: &mut [u8],
    ) -> Result<usize, D::Error> {
        let size = u64::from(file.size);
        if offset >= size || into.is_empty() {
            return Ok(0);
        }
        // A stale handle names another file's clusters, so reading through
        // one hands back that file's bytes under a name the caller supplied
        // — the same disclosure `create_with_size` refuses to make by
        // recording a length over unwritten clusters. Free when the
        // directory is resident, which it is on any path that just opened
        // the file.
        self.check_live(file)?;
        let wanted = into.len().min((size - offset) as usize);
        let into = &mut into[..wanted];

        if file.first_cluster == 0 {
            // A file with a length but no clusters is corrupt; one with
            // neither is empty and was handled above.
            return Err(FatError::BadCluster { cluster: 0 }.into());
        }

        let cluster_bytes = u64::from(self.boot_sector().cluster_bytes());
        let runs = self.runs(file.first_cluster)?;

        let mut done = 0usize;
        let mut at = offset;
        for run in &runs {
            let run_start = u64::from(run.file_cluster) * cluster_bytes;
            let run_len = run.bytes(cluster_bytes as u32);
            let run_end = run_start + run_len;
            if at >= run_end {
                continue;
            }

            let take = (run_end - at).min((wanted - done) as u64) as usize;
            let within_run = at - run_start;
            let first_block = u64::from(self.boot_sector().cluster_sector(run.start_cluster))
                + within_run / BLOCK_SIZE as u64;

            self.read_span(
                first_block,
                (within_run % BLOCK_SIZE as u64) as usize,
                &mut into[done..done + take],
            )?;

            done += take;
            at += take as u64;
            if done == wanted {
                break;
            }
        }

        if done != wanted {
            // The chain ran out before the length in the directory entry
            // did, which means the two disagree.
            return Err(FatError::ChainShorterThanFile {
                start: file.first_cluster,
                size: file.size,
            }
            .into());
        }
        Ok(done)
    }

    /// Reads `into.len()` bytes starting `skip` bytes into `first_block`.
    ///
    /// The three-part split described in this module's header. Callers pass
    /// a range that lies inside one contiguous run, so every block here is
    /// consecutive on the device.
    fn read_span(
        &mut self,
        first_block: u64,
        skip: usize,
        into: &mut [u8],
    ) -> Result<(), D::Error> {
        let mut block = first_block;
        let mut into = into;

        // Leading partial block, when the read does not start on a
        // boundary. One block through the scratch buffer, however large
        // the overall read is.
        if skip != 0 {
            let take = (BLOCK_SIZE - skip).min(into.len());
            let mut scratch = [0u8; BLOCK_SIZE];
            self.read_blocks(block, &mut scratch)?;
            into[..take].copy_from_slice(&scratch[skip..skip + take]);
            into = &mut into[take..];
            block += 1;
        }

        // Whole blocks, straight into the caller's buffer — no scratch, no
        // copy. Split only where the device says it must be.
        let whole = into.len() / BLOCK_SIZE;
        if whole != 0 {
            // Clamped at both ends. The upper bound is not cosmetic: an
            // unlimited device reports `u64::MAX`, and `cap * BLOCK_SIZE`
            // below would overflow.
            let cap = usize::try_from(self.device().max_transfer_blocks())
                .unwrap_or(usize::MAX)
                .clamp(1, whole);
            let (aligned, rest) = into.split_at_mut(whole * BLOCK_SIZE);
            for (batch, chunk) in aligned.chunks_mut(cap * BLOCK_SIZE).enumerate() {
                self.read_blocks(block + (batch * cap) as u64, chunk)?;
            }
            block += whole as u64;
            into = rest;
        }

        // Trailing partial block.
        if !into.is_empty() {
            let mut scratch = [0u8; BLOCK_SIZE];
            self.read_blocks(block, &mut scratch)?;
            let take = into.len();
            into.copy_from_slice(&scratch[..take]);
        }

        Ok(())
    }
}

impl<D: BlockDevice> FileSystem<D> {
    /// Creates an empty file at `path`.
    pub fn create(&mut self, path: &str) -> Result<File, D::Error> {
        self.create_with_size(path, 0)
    }

    /// Works out how a new name will be stored in `directory`, refusing it
    /// if the name is already there.
    ///
    /// Both halves need the parsed directory: the collision check, and
    /// choosing an 8.3 alias nothing else has. With the directory resident
    /// neither costs a device call.
    fn plan_name(&mut self, directory: u32, name: &str) -> Result<StoredName, D::Error> {
        let codepage = *self.codepage();
        let parsed = self.read_dir(directory)?;
        if parsed.get(name).is_some() {
            return Err(Error::AlreadyExists {
                name: String::from(name),
            });
        }
        crate::name::stored_name(name, &codepage, &mut |alias| parsed.get(alias).is_some())
    }

    /// Lays a name's entries out ready to write: the long-name slots, then
    /// the 8.3 entry that terminates them.
    ///
    /// `at` stamps every timestamp on the 8.3 entry, and comes from the
    /// volume's clock — [`DateTime::EPOCH`] when nothing has supplied one,
    /// which is a real date unlike the zeroes an entry would otherwise
    /// carry. Taken as an argument rather than read here so that one
    /// operation reads the clock once: a long name is several entries
    /// written together, and a tick falling between them would leave a
    /// file's own slots disagreeing about when it was made.
    fn name_entries(
        stored: &StoredName,
        attributes: Attributes,
        first_cluster: u32,
        size: u32,
        at: DateTime,
    ) -> Vec<[u8; ENTRY_SIZE]> {
        let mut entries = stored.slots.clone();
        let mut short = [0u8; ENTRY_SIZE];
        crate::dir::write_entry(
            &mut short,
            &stored.short,
            attributes,
            first_cluster,
            size,
            at,
        );
        entries.push(short);
        entries
    }

    /// Creates a file at `path` with room for `size` bytes already
    /// allocated.
    ///
    /// The size is a hint about the whole file, and it is what makes the
    /// result contiguous: the allocator is asked for every cluster at once
    /// and will find a single run if the volume has one, so the file can
    /// afterwards be written — and read — in a single device transfer. A
    /// file grown a write at a time gets whatever the allocator had spare
    /// each time, which is how files become fragmented.
    ///
    /// # `size` reserves space; it does not set the length
    ///
    /// The file comes back **empty**, and its length grows as
    /// [`write_at`](Self::write_at) fills the space in. The reservation is
    /// real — the clusters are allocated and chained, so the contiguity is
    /// already decided — but no length is recorded over bytes nothing has
    /// written yet.
    ///
    /// That is deliberate, and it is the difference between reserving space
    /// and lying about it. Recording `size` here would make the file
    /// readable at its full length before a byte of it existed, and what
    /// came back would be whatever the last file to own those clusters left
    /// behind. FAT does not erase a chain when it frees one, so those bytes
    /// are another file's data, handed to a caller that never opened it.
    ///
    /// Zeroing the clusters instead would fix the disclosure and cost the
    /// write twice over — once with zeros and once with the data — which is
    /// most of what this crate exists to avoid. So the length follows the
    /// data rather than leading it.
    ///
    /// A file reserved and then never written stays zero length while
    /// holding its clusters, until it is written or removed. `fsck.vfat`
    /// describes that state as a file whose size does not account for its
    /// chain and offers to truncate it to zero, which releases the
    /// reservation and is the right repair — wasted space, and nothing
    /// worse. Keeping the reservation attached to a named file is what makes
    /// it visible and removable at all; a reservation held off the directory
    /// entry would leak silently the moment the caller lost interest.
    ///
    /// # Ordering
    ///
    /// The clusters are allocated and chained before the directory entry
    /// naming them is written. Interrupted between the two, the volume has
    /// clusters belonging to nothing — a leak, which `fsck` reclaims. The
    /// other order would publish a directory entry pointing at clusters
    /// the allocator still believes are free, which is how two files come
    /// to share data.
    pub fn create_with_size(&mut self, path: &str, size: u32) -> Result<File, D::Error> {
        let now = self.now();
        let (parent, name) = split_path(path);
        let directory = self.resolve_dir(parent)?;
        let stored = self.plan_name(directory, name)?;

        let cluster_bytes = self.boot_sector().cluster_bytes();
        let clusters = size.div_ceil(cluster_bytes.max(1));
        let runs = self.fat_mut().allocate(clusters)?;
        let first_cluster = runs.first().map(|run| run.start_cluster).unwrap_or(0);

        // Nothing names these clusters yet, so giving them back on the way
        // out of a failure costs nothing and leaks nothing.
        let undo = |volume: &mut Self| {
            if first_cluster != 0 {
                let _ = volume.fat_mut().free_chain(first_cluster);
            }
        };

        let first_slot = match self.free_slots(directory, stored.entries()) {
            Ok(index) => index,
            Err(error) => {
                undo(self);
                return Err(error);
            }
        };

        // The allocation reaches the device before the entry that names it.
        // Interrupted here, the clusters are marked used and belong to
        // nothing, which `fsck` reclaims. The other order would leave an
        // entry pointing at clusters the allocator still offers to the next
        // caller — verified: removing this flush makes `fsck` report
        // "Contains a free cluster" instead of a plain leak.
        if let Err(error) = self.flush_fat() {
            undo(self);
            return Err(error);
        }

        // Length zero, not `size`: the clusters are reserved but nothing has
        // written them, and a length is a promise about content. See this
        // method's documentation.
        let entries = Self::name_entries(&stored, Attributes::ARCHIVE, first_cluster, 0, now);
        if let Err(error) = self.write_entries(directory, first_slot, &entries) {
            undo(self);
            return Err(error);
        }

        let index = first_slot + stored.slots.len() as u32;
        Ok(File::new(
            first_cluster,
            0,
            directory,
            index,
            first_slot,
            stored.short,
            self.deletions(directory),
        ))
    }

    /// Creates a directory at `path`, returning its first cluster.
    ///
    /// The parent must exist; this makes one directory, not a chain of them.
    ///
    /// # Ordering
    ///
    /// The new directory's cluster is allocated, filled in with its `.` and
    /// `..` entries, and linked into the table on the device — all before
    /// the parent gains an entry naming it. Interrupted at any point, the
    /// result is a leaked cluster holding a directory nothing points at. The
    /// other order would publish a directory entry whose cluster still held
    /// whatever the last file to own it left behind, and stale bytes in a
    /// directory read as entries.
    pub fn create_dir(&mut self, path: &str) -> Result<u32, D::Error> {
        let now = self.now();
        let (parent_path, name) = split_path(path);
        let parent = self.resolve_dir(parent_path)?;
        let stored = self.plan_name(parent, name)?;

        let cluster = self.fat_mut().allocate(1)?[0].start_cluster;
        let undo = |volume: &mut Self| {
            let _ = volume.fat_mut().free_chain(cluster);
        };

        let first_slot = match self.free_slots(parent, stored.entries()) {
            Ok(index) => index,
            Err(error) => {
                undo(self);
                return Err(error);
            }
        };

        // `..` names the parent — except when the parent is the root, where
        // the format writes 0. The cluster an entry carries and the cluster
        // the directory lives at are the same number everywhere else, and
        // this is the one place they are not.
        let parent_link = if parent == self.root_cluster() {
            0
        } else {
            parent
        };
        let sectors = usize::from(self.boot_sector().sectors_per_cluster);
        let mut data = vec![0u8; sectors * BLOCK_SIZE];
        crate::dir::write_dot_entries(&mut data, cluster, parent_link, now);

        let sector = u64::from(self.boot_sector().cluster_sector(cluster));
        let published = self
            .write_blocks(sector, &data)
            .and_then(|()| self.flush_fat())
            .and_then(|()| {
                // A directory has no length: its size field is zero and its
                // extent is its cluster chain.
                let entries = Self::name_entries(&stored, Attributes::DIRECTORY, cluster, 0, now);
                self.write_entries(parent, first_slot, &entries)
            });
        if let Err(error) = published {
            undo(self);
            return Err(error);
        }

        Ok(cluster)
    }

    /// Removes the empty directory at `path`.
    ///
    /// Refuses a directory that still holds anything. Removing one would
    /// leave its contents allocated and unreachable — space no `fsck` run
    /// reclaims, because the clusters really are referenced, by entries
    /// nothing can find any more.
    pub fn remove_dir(&mut self, path: &str) -> Result<(), D::Error> {
        let (parent_path, name) = split_path(path);
        let parent = self.resolve_dir(parent_path)?;

        let (cluster, first_slot, index) = {
            let parsed = self.read_dir(parent)?;
            let entry = parsed.get(name).ok_or_else(|| Error::NotFound {
                name: String::from(name),
            })?;
            if !entry.is_directory() {
                return Err(Error::NotADirectory {
                    name: String::from(name),
                });
            }
            (entry.first_cluster(), entry.first_slot(), entry.index())
        };

        let occupied = self
            .read_dir(cluster)?
            .iter()
            .filter(|entry| entry.name() != "." && entry.name() != "..")
            .count();
        if occupied != 0 {
            return Err(Error::DirectoryNotEmpty {
                name: String::from(name),
            });
        }

        // The entry first, then the clusters, as everywhere else: a leak
        // beats a directory entry naming space that has been handed out.
        self.delete_entries(parent, first_slot, index)?;
        self.fat_mut().free_chain(cluster)?;
        self.flush_fat()?;
        self.forget_directory(cluster);
        Ok(())
    }

    /// Writes `data` at `offset`, growing the file if it needs to.
    ///
    /// The file's recorded length is updated when the write extends it.
    /// `file` is updated to match, so it stays usable afterwards.
    ///
    /// # Writing past the end
    ///
    /// A write starting beyond [`File::len`] leaves a gap, and the gap is
    /// filled with zeros before the new length publishes it. The clusters
    /// under it hold whatever the last file to own them left behind — FAT
    /// frees a chain without erasing it — so the alternative is handing a
    /// caller another file's data under a length this crate wrote. The
    /// zeroing costs one pass over the gap, and only a caller that actually
    /// skips ahead pays it.
    pub fn write_at(&mut self, file: &mut File, offset: u64, data: &[u8]) -> Result<(), D::Error> {
        let now = self.now();
        if data.is_empty() {
            return Ok(());
        }
        // Before anything is allocated or written. A stale handle names
        // another file's clusters as well as another file's entry, so the
        // check has to come ahead of `write_chain` and not merely ahead of
        // the entry update.
        self.check_live(file)?;
        let end = offset + data.len() as u64;
        if end > u64::from(u32::MAX) {
            return Err(FatError::DiskFull {
                wanted: u32::MAX,
                free: self.fat().free_clusters(),
            }
            .into());
        }

        self.reserve(file, end as u32)?;

        // Before the data, so that an interruption between the two leaves a
        // gap of zeros rather than a gap of somebody else's file.
        if offset > u64::from(file.size) {
            self.zero_range(file, u64::from(file.size), offset)?;
        }

        self.write_chain(file, offset, data)?;

        if end as u32 > file.size {
            let (first, size) = (file.first_cluster, end as u32);
            self.edit_entry(file.directory, file.index, |entry| {
                crate::dir::update_entry(entry, first, size, now);
            })?;
            file.size = size;
        }
        Ok(())
    }

    /// Creates `path` holding exactly `data`, replacing anything there.
    ///
    /// The shape most callers want, and the one that produces a contiguous
    /// file: the length is known before a byte is written, so the whole
    /// chain is allocated at once.
    pub fn write_file(&mut self, path: &str, data: &[u8]) -> Result<File, D::Error> {
        if self.open(path).is_ok() {
            self.remove(path)?;
        }
        let mut file = self.create_with_size(path, data.len() as u32)?;
        self.write_at(&mut file, 0, data)?;
        Ok(file)
    }

    /// Sets the file's length, freeing or allocating clusters to match.
    ///
    /// Growing zeroes what it adds, the way every filesystem a caller has
    /// met does: the clusters it reaches into hold whatever the last file to
    /// own them left behind, and a length is a promise about content. See
    /// [`write_at`](Self::write_at) on writing past the end, which is the
    /// same gap by another route.
    ///
    /// # Ordering
    ///
    /// Shrinking writes the new length to the directory entry **first**,
    /// and frees the tail of the chain afterwards. Interrupted between the
    /// two, the volume has clusters nothing refers to — a leak. The other
    /// order leaves a directory entry claiming clusters the allocator has
    /// already handed to someone else, and two files sharing data is a
    /// worse outcome than some space going unused until `fsck` runs.
    pub fn truncate(&mut self, file: &mut File, size: u32) -> Result<(), D::Error> {
        let now = self.now();
        // Shrinking frees the tail of the chain, so a stale handle would
        // hand another file's clusters back to the allocator.
        self.check_live(file)?;
        if size > file.size {
            self.reserve(file, size)?;
            // The zeros land before the length that publishes them, so an
            // interruption between the two leaves the file its old length
            // rather than a longer one ending in stale bytes.
            self.zero_range(file, u64::from(file.size), u64::from(size))?;
            let (first, new) = (file.first_cluster, size);
            self.edit_entry(file.directory, file.index, |entry| {
                crate::dir::update_entry(entry, first, new, now);
            })?;
            file.size = new;
            return Ok(());
        }
        if size == file.size {
            return Ok(());
        }

        let cluster_bytes = self.boot_sector().cluster_bytes();
        let keep = size.div_ceil(cluster_bytes.max(1));
        let first_cluster = if keep == 0 { 0 } else { file.first_cluster };

        // The entry first, and only then the chain.
        self.edit_entry(file.directory, file.index, |entry| {
            crate::dir::update_entry(entry, first_cluster, size, now);
        })?;

        if file.first_cluster != 0 {
            self.fat_mut().truncate_chain(file.first_cluster, keep)?;
            // The freed tail reaches the device only after the entry has
            // stopped claiming it.
            self.flush_fat()?;
        }
        file.size = size;
        file.first_cluster = first_cluster;
        Ok(())
    }

    /// Deletes the file at `path`.
    ///
    /// The entry is marked deleted before the chain is freed, for the same
    /// reason truncation shrinks first. A long name's slots go with it:
    /// leaving them behind would produce the orphaned long-name parts
    /// `fsck.vfat` reports, occupying directory space nothing could reuse.
    pub fn remove(&mut self, path: &str) -> Result<(), D::Error> {
        let file = self.open(path)?;
        self.delete_entries(file.directory, file.first_slot, file.index)?;
        if file.first_cluster != 0 {
            self.fat_mut().free_chain(file.first_cluster)?;
            self.flush_fat()?;
        }
        Ok(())
    }

    /// Makes sure the file has clusters for at least `size` bytes.
    fn reserve(&mut self, file: &mut File, size: u32) -> Result<(), D::Error> {
        let now = self.now();
        let cluster_bytes = self.boot_sector().cluster_bytes().max(1);
        let wanted = size.div_ceil(cluster_bytes);
        if wanted == 0 {
            return Ok(());
        }

        if file.first_cluster == 0 {
            let runs = self.fat_mut().allocate(wanted)?;
            let first = runs[0].start_cluster;
            // Table to the device first, then the entry that names it —
            // the same rule as `create_with_size`, for the same reason.
            self.flush_fat()?;
            let size_now = file.size;
            self.edit_entry(file.directory, file.index, |entry| {
                crate::dir::update_entry(entry, first, size_now, now);
            })?;
            file.first_cluster = first;
            return Ok(());
        }

        let have = self.fat().chain_length(file.first_cluster)?;
        if wanted > have {
            let runs = self.fat().runs(file.first_cluster)?;
            let last = runs
                .last()
                .map(|run| run.start_cluster + run.clusters - 1)
                .expect("a chain has at least one cluster");
            self.fat_mut().append(last, wanted - have)?;
            // The longer chain has to be on the device before the entry
            // records a length that depends on it.
            self.flush_fat()?;
        }
        Ok(())
    }

    /// Writes `data` at `offset` into the chain the file already has.
    ///
    /// One [`write_span`](Self::write_span) per run the range crosses, so a
    /// write landing inside one contiguous run is one device transfer
    /// however large it is. Growing the chain is [`reserve`](Self::reserve)'s
    /// job; this fails rather than extending, because a short chain here
    /// means the table and the directory entry disagree.
    fn write_chain(&mut self, file: &File, offset: u64, data: &[u8]) -> Result<(), D::Error> {
        let cluster_bytes = u64::from(self.boot_sector().cluster_bytes());
        let runs = self.runs(file.first_cluster)?;

        let mut done = 0usize;
        let mut at = offset;
        for run in &runs {
            let run_start = u64::from(run.file_cluster) * cluster_bytes;
            let run_end = run_start + run.bytes(cluster_bytes as u32);
            if at >= run_end {
                continue;
            }
            let take = (run_end - at).min((data.len() - done) as u64) as usize;
            let within_run = at - run_start;
            let first_block = u64::from(self.boot_sector().cluster_sector(run.start_cluster))
                + within_run / BLOCK_SIZE as u64;

            self.write_span(
                first_block,
                (within_run % BLOCK_SIZE as u64) as usize,
                &data[done..done + take],
            )?;

            done += take;
            at += take as u64;
            if done == data.len() {
                break;
            }
        }

        if done != data.len() {
            return Err(FatError::ChainShorterThanFile {
                start: file.first_cluster,
                size: (offset + data.len() as u64) as u32,
            }
            .into());
        }
        Ok(())
    }

    /// Writes zeros over `from..to` of the file's existing chain.
    ///
    /// The bytes a file's recorded length covers but nothing has written:
    /// the gap under a write that starts past the end, and the extension a
    /// growing [`truncate`](Self::truncate) publishes. Both would otherwise
    /// hand back whatever the last file to own those clusters left behind.
    ///
    /// The zeros go through a fixed buffer rather than one sized to the
    /// range, so a gap of any size costs the same memory. The device sees
    /// full-length transfers regardless — [`write_span`](Self::write_span)
    /// splits only where the device says it must — so the buffer bounds the
    /// allocation and not the transfer.
    fn zero_range(&mut self, file: &File, from: u64, to: u64) -> Result<(), D::Error> {
        /// Bytes of zeros staged per pass. Large enough that the per-call
        /// overhead of re-walking the chain is lost against the transfer,
        /// small enough to be an unremarkable transient on a small heap.
        const ZERO_CHUNK: usize = 64 * 1024;

        if from >= to {
            return Ok(());
        }
        let zeros = vec![0u8; ZERO_CHUNK.min((to - from) as usize)];

        let mut at = from;
        while at < to {
            let take = ((to - at) as usize).min(zeros.len());
            self.write_chain(file, at, &zeros[..take])?;
            at += take as u64;
        }
        Ok(())
    }

    /// Writes `from` starting `skip` bytes into `first_block`.
    ///
    /// The mirror of the read path's split, with one asymmetry: a partial
    /// block has to be read before it is written, because a device only
    /// accepts whole blocks and the bytes around the written part have to
    /// survive. Whole blocks go straight from the caller's slice.
    fn write_span(&mut self, first_block: u64, skip: usize, from: &[u8]) -> Result<(), D::Error> {
        let mut block = first_block;
        let mut from = from;

        if skip != 0 {
            let take = (BLOCK_SIZE - skip).min(from.len());
            let mut scratch = [0u8; BLOCK_SIZE];
            self.read_blocks(block, &mut scratch)?;
            scratch[skip..skip + take].copy_from_slice(&from[..take]);
            self.write_blocks(block, &scratch)?;
            from = &from[take..];
            block += 1;
        }

        let whole = from.len() / BLOCK_SIZE;
        if whole != 0 {
            let cap = usize::try_from(self.device().max_transfer_blocks())
                .unwrap_or(usize::MAX)
                .clamp(1, whole);
            let (aligned, rest) = from.split_at(whole * BLOCK_SIZE);
            for (batch, chunk) in aligned.chunks(cap * BLOCK_SIZE).enumerate() {
                self.write_blocks(block + (batch * cap) as u64, chunk)?;
            }
            block += whole as u64;
            from = rest;
        }

        if !from.is_empty() {
            let mut scratch = [0u8; BLOCK_SIZE];
            self.read_blocks(block, &mut scratch)?;
            scratch[..from.len()].copy_from_slice(from);
            self.write_blocks(block, &scratch)?;
        }
        Ok(())
    }
}

/// Splits a path into its directory and its final component.
fn split_path(path: &str) -> (&str, &str) {
    match path.rfind('/') {
        Some(at) => (&path[..at], &path[at + 1..]),
        None => ("", path),
    }
}
