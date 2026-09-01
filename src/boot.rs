//! The boot sector, and the geometry derived from it.

use crate::blockdev::BLOCK_SIZE;
use crate::error::{BootError, Format, Geometry};

/// Offsets into the boot sector. Named rather than inlined because several
/// of them are only meaningful for one format, and the names are what say
/// which.
mod field {
    /// Bytes per logical sector.
    pub const BYTES_PER_SECTOR: usize = 0x0B;
    /// Sectors per cluster.
    pub const SECTORS_PER_CLUSTER: usize = 0x0D;
    /// Sectors before the first file allocation table.
    pub const RESERVED_SECTORS: usize = 0x0E;
    /// How many copies of the table the volume carries.
    pub const FAT_COUNT: usize = 0x10;
    /// Root directory entries. Zero on FAT32, where the root is a chain.
    pub const ROOT_ENTRIES: usize = 0x11;
    /// Total sectors, when the count fits in 16 bits.
    pub const TOTAL_SECTORS_16: usize = 0x13;
    /// Sectors per table on FAT12/FAT16. **Zero exactly when the volume is
    /// FAT32**, which is what makes it the classifier.
    pub const SECTORS_PER_FAT_16: usize = 0x16;
    /// Total sectors, when the 16-bit field is zero.
    pub const TOTAL_SECTORS_32: usize = 0x20;
    /// Sectors per table on FAT32. Meaningless on the other formats.
    pub const SECTORS_PER_FAT_32: usize = 0x24;
    /// First cluster of the root directory. FAT32 only.
    pub const ROOT_CLUSTER: usize = 0x2C;
    /// Sector holding the free-space hints. FAT32 only.
    pub const FS_INFO_SECTOR: usize = 0x30;
    /// Volume state, carrying the dirty flag. FAT32 only.
    pub const STATE: usize = 0x41;
    /// Volume serial number. FAT32 only.
    pub const VOLUME_ID: usize = 0x43;
    /// The two-byte signature every boot sector ends with.
    pub const SIGNATURE: usize = 0x1FE;
}

/// Set in the volume state byte when the volume was not cleanly unmounted.
const STATE_DIRTY: u8 = 0x01;

/// The first two entries of every table are reserved, so cluster numbering
/// starts here.
pub const FIRST_CLUSTER: u32 = 2;

/// Largest cluster number FAT32 can address. Values above it are reserved
/// for the end-of-chain and bad-cluster markers.
const MAX_FAT32_CLUSTERS: u32 = 0x0FFF_FFF5;

/// Below this many clusters a volume is FAT12, and below the next one it is
/// FAT16 — *if* its structure has not already said otherwise. See
/// [`BootSector::parse`] on why the structure wins.
const MAX_FAT12_CLUSTERS: u32 = 4084;
/// Upper bound of the FAT16 cluster range.
const MAX_FAT16_CLUSTERS: u32 = 65524;

/// A validated FAT32 boot sector, plus what falls out of it.
///
/// Every field here has been range-checked, so the rest of the crate can do
/// arithmetic on them without re-validating.
///
/// `#[non_exhaustive]` because this is handed out and never taken in: it
/// comes from [`parse`](Self::parse) and nothing else, so forbidding a
/// struct literal downstream costs nobody anything, while leaving room to
/// expose another field later. FAT12 and FAT16 are out of scope but not out
/// of mind — see the crate's Scope section — and either would bring fields
/// this struct does not have.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootSector {
    /// Bytes in a logical sector. Always [`BLOCK_SIZE`]; a volume claiming
    /// anything else is refused by [`parse`](Self::parse).
    pub bytes_per_sector: u16,
    /// Sectors in a cluster; a power of two.
    pub sectors_per_cluster: u8,
    /// Sectors before the first table.
    pub reserved_sectors: u16,
    /// Copies of the table on the volume.
    pub fat_count: u8,
    /// Sectors occupied by one copy of the table.
    pub sectors_per_fat: u32,
    /// First cluster of the root directory.
    pub root_cluster: u32,
    /// Sectors in the volume.
    pub total_sectors: u32,
    /// Sector holding the free-space hints, if the volume names one.
    pub fs_info_sector: Option<u16>,
    /// The volume's serial number.
    pub volume_id: u32,
    /// Whether the volume was marked dirty — not cleanly unmounted.
    ///
    /// Detects an unclean shutdown; it does not repair one. The likely
    /// damage under this crate's write ordering is leaked clusters, which
    /// are harmless until the volume fills.
    pub dirty: bool,
    /// First sector of the first table.
    pub fat_start: u32,
    /// First sector of the data area.
    pub data_start: u32,
    /// Clusters the volume actually has, clamped by what the table can
    /// address. See [`BootSector::parse`].
    pub cluster_count: u32,
}

impl BootSector {
    /// Parses and validates a boot sector.
    ///
    /// # Deciding the format
    ///
    /// The 16-bit sectors-per-table field is the classifier, not the
    /// cluster count. Two things force that order, and getting either
    /// wrong misreads real volumes:
    ///
    /// * On FAT12 and FAT16 the 32-bit sectors-per-table field does not
    ///   exist — that offset holds boot code — so reading it first yields
    ///   an enormous number that looks like a plausible FAT32 table.
    /// * A FAT32 volume may hold fewer clusters than the FAT12 threshold.
    ///   `mkfs.vfat -F 32` will build one, warning as it goes, and
    ///   `fsck.vfat` accepts the result. Classifying by cluster count
    ///   would call such a volume FAT12 and refuse a volume every other
    ///   tool mounts.
    ///
    /// So: structure decides the format, and the cluster count is used only
    /// to name which of FAT12 and FAT16 is being declined.
    ///
    /// A count fitting neither is not given a name at all. The structure
    /// says FAT12 or FAT16 and the arithmetic says neither, which makes the
    /// boot sector inconsistent rather than merely unsupported, and
    /// [`BootError::BadGeometry`] rather than
    /// [`BootError::UnsupportedFormat`] is what says so — the two call for
    /// different responses, since reformatting answers only one of them.
    ///
    /// # Clamping
    ///
    /// The cluster count is the smaller of what the volume's size implies
    /// and what the table can address. The two disagree on corrupt or
    /// mis-formatted media, and the table's capacity is the one that
    /// bounds the resident array — so it is the one that decides which
    /// cluster numbers are valid.
    pub fn parse(block: &[u8]) -> Result<Self, BootError> {
        // Checked rather than asserted, and checked in release too: every
        // read below indexes a fixed offset, so a short slice would panic
        // instead of failing. A `debug_assert!` here would have caught it in
        // testing and left the panic in the build that runs on hardware.
        if block.len() < BLOCK_SIZE {
            return Err(BootError::ShortBlock { len: block.len() });
        }

        if read_u16(block, field::SIGNATURE) != 0xAA55 {
            return Err(BootError::NotFat);
        }

        // Exactly BLOCK_SIZE, not merely one of the four sizes the format
        // allows. Everything downstream -- `cluster_sector`, `fat_start`,
        // `data_start`, every dirty-sector number the table hands to
        // `flush_fat` -- is a *sector* number that reaches the device as a
        // *block* number, and the two are the same number only at 512.
        // Accepting 4096 here would mount the volume, read plausible bytes
        // from an eighth of the intended offset, and write eight times the
        // intended region on the first flush.
        let bytes_per_sector = read_u16(block, field::BYTES_PER_SECTOR);
        if usize::from(bytes_per_sector) != BLOCK_SIZE {
            return Err(BootError::BadGeometry(Geometry::SectorSize(
                bytes_per_sector,
            )));
        }

        let sectors_per_cluster = block[field::SECTORS_PER_CLUSTER];
        if !sectors_per_cluster.is_power_of_two() {
            return Err(BootError::BadGeometry(Geometry::SectorsPerCluster(
                sectors_per_cluster,
            )));
        }

        let reserved_sectors = read_u16(block, field::RESERVED_SECTORS);
        if reserved_sectors == 0 {
            return Err(BootError::BadGeometry(Geometry::ReservedSectors(
                reserved_sectors,
            )));
        }

        let fat_count = block[field::FAT_COUNT];
        if fat_count == 0 {
            return Err(BootError::BadGeometry(Geometry::FatCount(fat_count)));
        }

        // A count that fits in 16 bits lives in the 16-bit field, and the
        // 32-bit one is then zero. Small volumes really do use it: the
        // fragmented test fixture is one.
        let total_sectors = match read_u16(block, field::TOTAL_SECTORS_16) {
            0 => read_u32(block, field::TOTAL_SECTORS_32),
            small => u32::from(small),
        };
        if total_sectors == 0 {
            return Err(BootError::BadGeometry(Geometry::TotalSectors(
                total_sectors,
            )));
        }

        // The classifier. See this function's documentation.
        let sectors_per_fat_16 = read_u16(block, field::SECTORS_PER_FAT_16);
        if sectors_per_fat_16 != 0 {
            return Err(classify_small(
                block,
                bytes_per_sector,
                sectors_per_cluster,
                reserved_sectors,
                fat_count,
                u32::from(sectors_per_fat_16),
                total_sectors,
            ));
        }

        let sectors_per_fat = read_u32(block, field::SECTORS_PER_FAT_32);
        if sectors_per_fat == 0 {
            return Err(BootError::BadGeometry(Geometry::FatLength(sectors_per_fat)));
        }

        let fat_start = u32::from(reserved_sectors);
        let data_start = fat_start + u32::from(fat_count) * sectors_per_fat;
        if data_start >= total_sectors {
            return Err(BootError::BadGeometry(Geometry::DataBeyondVolume {
                data_start,
                total_sectors,
            }));
        }

        let by_volume = (total_sectors - data_start) / u32::from(sectors_per_cluster);
        // Entries the table holds, less the two reserved ones.
        let by_table = sectors_per_fat
            .saturating_mul(u32::from(bytes_per_sector) / 4)
            .saturating_sub(FIRST_CLUSTER);
        let cluster_count = by_volume.min(by_table);

        if cluster_count > MAX_FAT32_CLUSTERS {
            return Err(BootError::BadGeometry(Geometry::ClusterCount(
                cluster_count,
            )));
        }

        let fs_info_sector = match read_u16(block, field::FS_INFO_SECTOR) {
            0 | 0xFFFF => None,
            sector => Some(sector),
        };

        Ok(BootSector {
            bytes_per_sector,
            sectors_per_cluster,
            reserved_sectors,
            fat_count,
            sectors_per_fat,
            root_cluster: read_u32(block, field::ROOT_CLUSTER),
            total_sectors,
            fs_info_sector,
            volume_id: read_u32(block, field::VOLUME_ID),
            dirty: block[field::STATE] & STATE_DIRTY != 0,
            fat_start,
            data_start,
            cluster_count,
        })
    }

    /// Bytes in a cluster.
    pub fn cluster_bytes(&self) -> u32 {
        u32::from(self.sectors_per_cluster) * u32::from(self.bytes_per_sector)
    }

    /// The first sector of `cluster`.
    ///
    /// Callers are expected to have validated the cluster number; an
    /// invalid one produces a sector inside the volume rather than a
    /// panic, which is why validation is not optional.
    pub fn cluster_sector(&self, cluster: u32) -> u32 {
        self.data_start + (cluster - FIRST_CLUSTER) * u32::from(self.sectors_per_cluster)
    }

    /// Whether `cluster` is one this volume can address.
    pub fn is_valid_cluster(&self, cluster: u32) -> bool {
        cluster >= FIRST_CLUSTER && cluster < FIRST_CLUSTER + self.cluster_count
    }
}

/// Says why a non-FAT32 volume is being refused.
///
/// Only reached once the structure has already ruled out FAT32, so the
/// cluster count is being used to *name* the format rather than to decide
/// it.
///
/// Naming it is not always possible. A count above the FAT16 ceiling means
/// the structure says FAT12 or FAT16 and the arithmetic says neither, which
/// is a corrupt boot sector rather than a format this crate declines to
/// support. [`Format`] exists precisely to keep those two apart — only one
/// of them is answered by reformatting — so calling such a volume FAT16
/// would be the one claim this function must not make. It comes back as bad
/// geometry instead, naming the count that fits nothing.
///
/// That case is reported rather than asserted. The count can exceed the
/// ceiling on any corrupt or hostile boot sector, and these bytes came off a
/// card: a debug build has no business aborting a bare-metal target over
/// what a card claimed, and there is no unwinder there to catch it if it
/// did. See [`Error::BufferLength`](crate::Error::BufferLength), which is
/// the same argument for a milder input.
#[allow(clippy::too_many_arguments)]
fn classify_small(
    block: &[u8],
    bytes_per_sector: u16,
    sectors_per_cluster: u8,
    reserved_sectors: u16,
    fat_count: u8,
    sectors_per_fat: u32,
    total_sectors: u32,
) -> BootError {
    // FAT12 and FAT16 put the root directory in a fixed region between the
    // tables and the data, sized by an entry count. It has to be stepped
    // over to find where the data starts.
    let root_entries = u32::from(read_u16(block, field::ROOT_ENTRIES));
    let root_sectors = (root_entries * 32).div_ceil(u32::from(bytes_per_sector).max(1));

    let data_start =
        u32::from(reserved_sectors) + u32::from(fat_count) * sectors_per_fat + root_sectors;
    let clusters = total_sectors
        .saturating_sub(data_start)
        .checked_div(u32::from(sectors_per_cluster))
        .unwrap_or(0);

    if clusters <= MAX_FAT12_CLUSTERS {
        BootError::UnsupportedFormat(Format::Fat12)
    } else if clusters <= MAX_FAT16_CLUSTERS {
        BootError::UnsupportedFormat(Format::Fat16)
    } else {
        BootError::BadGeometry(Geometry::ClusterCount(clusters))
    }
}

/// Free-space hints, which are hints and nothing more.
///
/// Nothing here is trusted. A count larger than the volume, or a next-free
/// pointer outside it, is discarded rather than clamped into something
/// plausible — a wrong number that looks right is worse than none, because
/// it is the one that gets believed.
///
/// `#[non_exhaustive]` for the same reason as [`BootSector`]: it is parsed,
/// never constructed by a caller.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FsInfo {
    /// Free clusters, if the volume's claim is even possible.
    pub free_clusters: Option<u32>,
    /// Where to start looking for a free cluster, if the hint is in range.
    pub next_free: Option<u32>,
}

impl FsInfo {
    /// Parses the hints, discarding any that cannot be true.
    ///
    /// A block shorter than a sector comes back as [`Default`] — the same
    /// answer as a block whose signatures are wrong, and for the same reason.
    /// Nothing here is trusted anyway, so there is no failure to report: the
    /// absence of usable hints is a normal outcome, and this returns no
    /// `Result` precisely because it cannot fail.
    pub fn parse(block: &[u8], boot: &BootSector) -> Self {
        const LEAD_SIGNATURE: u32 = 0x4161_5252;
        const STRUCT_SIGNATURE: u32 = 0x6141_7272;
        const UNKNOWN: u32 = 0xFFFF_FFFF;

        if block.len() < BLOCK_SIZE {
            return FsInfo::default();
        }
        if read_u32(block, 0x000) != LEAD_SIGNATURE || read_u32(block, 0x1E4) != STRUCT_SIGNATURE {
            return FsInfo::default();
        }

        let free_clusters = match read_u32(block, 0x1E8) {
            UNKNOWN => None,
            count if count > boot.cluster_count => None,
            count => Some(count),
        };
        let next_free = match read_u32(block, 0x1EC) {
            UNKNOWN => None,
            cluster if !boot.is_valid_cluster(cluster) => None,
            cluster => Some(cluster),
        };

        FsInfo {
            free_clusters,
            next_free,
        }
    }
}

fn read_u16(block: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([block[at], block[at + 1]])
}

fn read_u32(block: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([block[at], block[at + 1], block[at + 2], block[at + 3]])
}
