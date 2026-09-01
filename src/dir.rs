//! Directories: entries, long names, and the parsed form kept in memory.
//!
//! A directory is read once, in as few transfers as its cluster chain has
//! runs, and kept parsed. Enumeration is then a slice walk and lookup is a
//! map hit — which is the difference between opening a file in a
//! three-hundred-entry directory costing nothing and costing a rescan of
//! the whole directory from the card.
//!
//! # Tolerance
//!
//! Long-name parsing here is deliberately forgiving, because these volumes
//! are written by cameras, phones, imaging tools and other people's
//! filesystem code. A damaged long-name run costs its own file's long
//! name; it does not cost the rest of the directory, and it does not hide
//! the file. The one thing not tolerated is an allocated entry after the
//! end marker, because that is invisible data rather than a cosmetic
//! fault.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use crate::error::{Error, Result};
use crate::time::{DateTime, Packed};

/// Bytes in one directory entry.
pub const ENTRY_SIZE: usize = 32;

/// Marks a slot whose file was deleted.
const DELETED: u8 = 0xE5;
/// Marks a slot that has never been used. Nothing allocated follows it.
const END_OF_DIRECTORY: u8 = 0x00;
/// A name byte of 0x05 stands in for 0xE5, which would otherwise read as
/// the deleted marker.
const ESCAPED_E5: u8 = 0x05;
/// Set in a long-name slot's sequence byte on the run's first slot.
const LAST_LONG_SLOT: u8 = 0x40;
/// The most slots a long name can occupy: 255 characters, 13 per slot.
const MAX_LONG_SLOTS: u8 = 20;
/// Characters carried by one long-name slot.
const CHARS_PER_SLOT: usize = 13;

/// A directory entry's attribute bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Attributes(u8);

impl Attributes {
    /// No attributes at all.
    pub const NONE: Attributes = Attributes(0x00);
    /// The file should not be written to.
    pub const READ_ONLY: Attributes = Attributes(0x01);
    /// The file is hidden from ordinary listings.
    pub const HIDDEN: Attributes = Attributes(0x02);
    /// The file belongs to the operating system.
    pub const SYSTEM: Attributes = Attributes(0x04);
    /// The entry carries the volume label rather than a file.
    pub const VOLUME_ID: Attributes = Attributes(0x08);
    /// The entry is a directory.
    pub const DIRECTORY: Attributes = Attributes(0x10);
    /// The file has changed since it was last archived.
    pub const ARCHIVE: Attributes = Attributes(0x20);
    /// All four of the bits that together mark a long-name slot.
    ///
    /// Spelled out from the four rather than written as `|`, because a
    /// `const` cannot call a trait method and [`BitOr`](core::ops::BitOr) is
    /// one.
    pub const LONG_NAME: Attributes =
        Attributes(Self::READ_ONLY.0 | Self::HIDDEN.0 | Self::SYSTEM.0 | Self::VOLUME_ID.0);

    /// Attributes from raw bits.
    pub const fn from_bits(bits: u8) -> Self {
        Attributes(bits)
    }

    /// The raw bits, as a directory entry stores them.
    pub const fn bits(self) -> u8 {
        self.0
    }

    /// Whether every bit of `other` is set here.
    ///
    /// The general form of the questions below. `contains` on an empty set
    /// is vacuously true, which is the usual convention and the one that
    /// makes `contains(Attributes::NONE)` uninteresting rather than wrong.
    pub const fn contains(self, other: Attributes) -> bool {
        self.0 & other.0 == other.0
    }

    /// Whether the entry is a directory.
    pub const fn is_directory(self) -> bool {
        self.contains(Self::DIRECTORY)
    }

    /// Whether the entry carries the volume label.
    ///
    /// Such an entry sits first in the root directory on most volumes and
    /// is not a file, so every reader has to step over it.
    pub const fn is_volume_id(self) -> bool {
        self.contains(Self::VOLUME_ID)
    }

    /// Whether the entry is a long-name slot rather than a file.
    ///
    /// The test is equality, not [`contains`](Self::contains): the four bits
    /// are set *together*, and nothing else may be set alongside them,
    /// precisely so that an implementation which does not understand long
    /// names sees a read-only hidden system volume label and skips it.
    pub const fn is_long_name(self) -> bool {
        self.0 == Self::LONG_NAME.0
    }
}

impl core::ops::BitOr for Attributes {
    type Output = Attributes;

    /// Combines two sets, so `READ_ONLY | HIDDEN` reads the way it looks.
    fn bitor(self, other: Attributes) -> Attributes {
        Attributes(self.0 | other.0)
    }
}

impl core::ops::BitOrAssign for Attributes {
    /// Sets the bits of `other` in place.
    fn bitor_assign(&mut self, other: Attributes) {
        self.0 |= other.0;
    }
}

/// The lower-case flags a short name can carry.
pub(crate) mod lcase {
    /// The base is stored upper case but should be shown lower case.
    pub const BASE: u8 = 0x08;
    /// The extension is stored upper case but should be shown lower case.
    pub const EXTENSION: u8 = 0x10;
}

/// An 8.3 short name, exactly as the directory entry stores it.
///
/// Kept as the raw eleven bytes rather than as a string, because that is
/// what the checksum binding a long name to its entry is computed over, and
/// recomputing it from a formatted name would not survive the characters
/// that formatting changes.
///
/// "Exactly as stored" includes the escape: a name whose first byte is
/// really 0xE5 is held here as 0x05, because that is what is on the volume
/// and therefore what the checksum has to be taken over. Only display
/// undoes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShortName {
    raw: [u8; 11],
    case_flags: u8,
}

impl ShortName {
    /// Reads a short name out of a directory entry.
    ///
    /// # Panics
    ///
    /// If `entry` is shorter than [`ENTRY_SIZE`]. Kept a panic rather than
    /// made an `Option`, unlike the block parsers on
    /// [`BootSector`](crate::BootSector) and `mbr::PartitionTable`: those are
    /// handed whatever a device read produced, whereas a directory entry is a
    /// 32-byte unit a caller has already carved out, and there is no
    /// half-name to report as absent. Every caller in this crate takes
    /// `entry` from a `chunks_exact(ENTRY_SIZE)` walk, so the length is a
    /// property of the loop rather than of each call.
    ///
    /// (`PartitionTable` is named rather than linked because a link to it
    /// does not resolve when the `mbr` feature is off.)
    pub fn from_entry(entry: &[u8]) -> Self {
        let mut raw = [0u8; 11];
        raw.copy_from_slice(&entry[0..11]);
        ShortName {
            raw,
            case_flags: entry[0x0C],
        }
    }

    /// A short name from bytes that are about to be stored.
    ///
    /// Applies the escape, so a caller building a name may pass the byte it
    /// means and get the byte the format requires.
    pub(crate) fn from_raw(mut raw: [u8; 11], case_flags: u8) -> Self {
        if raw[0] == DELETED {
            raw[0] = ESCAPED_E5;
        }
        ShortName { raw, case_flags }
    }

    /// The eleven stored bytes: eight of base, three of extension, space
    /// padded.
    ///
    /// A leading 0x05 stands for 0xE5; see the note on this type.
    pub fn as_bytes(&self) -> &[u8; 11] {
        &self.raw
    }

    /// The case bits, saying whether the base and the extension should be
    /// shown lower case.
    pub fn case_flags(&self) -> u8 {
        self.case_flags
    }

    /// The checksum a long name's slots carry to bind themselves to this
    /// entry.
    ///
    /// A rotate-and-add over all eleven bytes **as stored**, which is why
    /// this type keeps them that way: taking it over an unescaped leading
    /// 0xE5 would produce a checksum no other implementation agrees with,
    /// and the long name would be dropped as belonging to something else.
    pub fn checksum(&self) -> u8 {
        self.raw
            .iter()
            .fold(0u8, |sum, &byte| sum.rotate_right(1).wrapping_add(byte))
    }

    /// Builds a short name from a string that is already 8.3, or `None`.
    ///
    /// Exact, and deliberately so: nothing is mangled, nothing gains a `~1`
    /// tail, and the case bits are set when a field is uniformly cased.
    /// Names that need aliasing are handled where a directory is available
    /// to make the alias unique in — that is, by
    /// [`create`](crate::FileSystem::create) — since an alias cannot be
    /// chosen without knowing what is already there.
    ///
    /// `None` rather than an error because there is nothing an error could
    /// add: the only way this fails is that `name` has no exact 8.3 form,
    /// and the caller is holding `name`. Returning
    /// [`crate::Error`] would also have made this generic over a
    /// device error it cannot produce, so a caller would have had to write
    /// `ShortName::from_8_3::<()>(name)` to name a type that never arrives.
    pub fn from_8_3(name: &str) -> Option<Self> {
        crate::name::exact_8_3(name, &crate::codepage::Codepage::ASCII)
    }

    /// The name in `BASE.EXT` form, with the stored case applied, read as
    /// ASCII.
    pub fn to_display_string(&self) -> String {
        self.to_display_string_with(&crate::codepage::Codepage::ASCII)
    }

    /// The name in `BASE.EXT` form, decoded through `codepage`.
    ///
    /// A byte the codepage does not cover comes back as the replacement
    /// character rather than as a guess; the raw bytes stay available
    /// through [`as_bytes`](Self::as_bytes) so nothing is lost.
    pub fn to_display_string_with(&self, codepage: &crate::codepage::Codepage) -> String {
        let mut raw = self.raw;
        if raw[0] == ESCAPED_E5 {
            raw[0] = DELETED;
        }

        let mut name = String::with_capacity(12);
        push_decoded(
            &mut name,
            &raw[0..8],
            self.case_flags & lcase::BASE != 0,
            codepage,
        );
        let extension_at = name.len();
        push_decoded(
            &mut name,
            &raw[8..11],
            self.case_flags & lcase::EXTENSION != 0,
            codepage,
        );
        if name.len() > extension_at {
            name.insert(extension_at, '.');
        }
        name
    }
}

/// Whether a byte may appear in a short name.
///
/// The reserved set, plus the characters that are legal in a long name and
/// have to be replaced in an alias. Space is excluded because it is the
/// padding, so a name containing one could not be told from a shorter one.
///
/// Bytes at or above 0x80 are permitted: which characters those are depends
/// on the volume's codepage, and none of the format's reserved characters
/// live up there.
pub(crate) fn is_valid_short_name_byte(byte: u8) -> bool {
    if byte < 0x20 {
        return false;
    }
    !matches!(
        byte,
        b'"' | b'*'
            | b'+'
            | b','
            | b'.'
            | b'/'
            | b':'
            | b';'
            | b'<'
            | b'='
            | b'>'
            | b'?'
            | b'['
            | b'\\'
            | b']'
            | b'|'
            | b' '
    )
}

/// Appends a space-padded field, lower-casing it if the entry says to.
fn push_decoded(
    into: &mut String,
    field: &[u8],
    lower: bool,
    codepage: &crate::codepage::Codepage,
) {
    for &byte in field {
        if byte == b' ' {
            continue;
        }
        let character = codepage.decode(byte).unwrap_or('\u{FFFD}');
        if lower {
            into.extend(character.to_lowercase());
        } else {
            into.push(character);
        }
    }
}

/// One file or directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    short_name: ShortName,
    /// The 8.3 name formatted through the volume's codepage, kept rather
    /// than recomputed: it is wanted for every lookup key and every
    /// listing, and formatting it allocates.
    short_display: String,
    long_name: Option<String>,
    attributes: Attributes,
    first_cluster: u32,
    size: u32,
    index: u32,
    first_slot: u32,
    created: DateTime,
    modified: DateTime,
    accessed: DateTime,
}

impl DirEntry {
    /// The name to show: the long one when the entry has a usable one, and
    /// the 8.3 name otherwise.
    pub fn name(&self) -> &str {
        match &self.long_name {
            Some(name) => name,
            None => &self.short_display,
        }
    }

    /// The long name, if this entry has one that checked out.
    pub fn long_name(&self) -> Option<&str> {
        self.long_name.as_deref()
    }

    /// The 8.3 name, which every entry has.
    pub fn short_name(&self) -> &ShortName {
        &self.short_name
    }

    /// The entry's attribute bits.
    pub fn attributes(&self) -> Attributes {
        self.attributes
    }

    /// Whether this is a directory.
    pub fn is_directory(&self) -> bool {
        self.attributes.is_directory()
    }

    /// The first cluster of the entry's data.
    ///
    /// Zero for an empty file, and also for the `..` entry of a directory
    /// whose parent is the root — the format writes 0 there rather than
    /// the root's own cluster number.
    pub fn first_cluster(&self) -> u32 {
        self.first_cluster
    }

    /// The file's length in bytes. Zero for a directory, whose length is
    /// its cluster chain.
    pub fn len(&self) -> u32 {
        self.size
    }

    /// Whether the file is empty.
    pub fn is_empty(&self) -> bool {
        self.size == 0
    }

    /// Which 32-byte slot of its directory this entry occupies.
    ///
    /// Kept so that changing a file's length or starting cluster is a
    /// rewrite of one known slot rather than a fresh search of the
    /// directory for the name.
    pub fn index(&self) -> u32 {
        self.index
    }

    /// When the file was created.
    ///
    /// The only one of the three stored to better than two seconds: the
    /// creation timestamp has an extra field carrying the odd second and
    /// hundredths, and the other two do not. See [`crate::time`].
    pub fn created(&self) -> DateTime {
        self.created
    }

    /// When the file was last written, to the nearest two seconds.
    pub fn modified(&self) -> DateTime {
        self.modified
    }

    /// When the file was last read, to the nearest day.
    ///
    /// FAT records access as a date with no time at all. Many
    /// implementations never update it, this one included — a read that
    /// wrote to the volume would turn browsing a card into wearing it out.
    pub fn accessed(&self) -> DateTime {
        self.accessed
    }

    /// The first slot this entry occupies, counting the long-name slots in
    /// front of it.
    ///
    /// Equal to [`index`](Self::index) for an entry with no long name.
    /// Deleting a file has to free this whole range: leaving the slots
    /// behind produces the orphaned long-name parts `fsck.vfat` reports,
    /// and they would go on consuming directory space nothing can reuse.
    ///
    /// The run counted is the contiguous one physically in front of the
    /// entry, whether or not its checksum agreed. Slots that failed the
    /// check belong to no other entry either — nothing else can reach them
    /// — so they go when this entry does.
    pub fn first_slot(&self) -> u32 {
        self.first_slot
    }
}

/// A directory, read and parsed.
#[derive(Debug, Clone, Default)]
pub struct Directory {
    entries: Vec<DirEntry>,
    /// Upper-cased name to index. Holds both the long and the short name
    /// of every entry, so a lookup by either finds it.
    index: BTreeMap<String, usize>,
    /// Stretches of consecutive unused slots, as `(first slot, length)`.
    ///
    /// Recorded during the same walk that parses the entries, so finding
    /// room for a new file is a look at this list rather than a re-read of
    /// the directory from the device. That matters more than it looks: a
    /// long name needs a *run* of slots, and searching for one by reading
    /// blocks back would cost a transfer per block of the directory for
    /// every file created.
    free: Vec<(u32, u32)>,
}

impl Directory {
    /// Every entry, in the order the directory stores them.
    pub fn iter(&self) -> core::slice::Iter<'_, DirEntry> {
        self.entries.iter()
    }

    /// How many entries the directory holds.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the directory has no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Looks an entry up by name, case-insensitively.
    ///
    /// Either name works: FAT names are matched without regard to case,
    /// and a file with a long name is just as findable by its 8.3 alias.
    /// This is a map lookup rather than a scan, which is the whole reason
    /// the parsed directory is kept.
    pub fn get(&self, name: &str) -> Option<&DirEntry> {
        let key = name.to_uppercase();
        self.index.get(&key).map(|&at| &self.entries[at])
    }

    /// The entry occupying slot `index`, if one does.
    ///
    /// The inverse of [`DirEntry::index`], and the way to ask whether the
    /// entry a slot held is still the one that was there — which is what
    /// tells a [`File`](crate::File) handle from a stale one.
    ///
    /// A binary search rather than a scan: entries are parsed in slot order,
    /// so the list is already sorted by the key.
    pub fn at(&self, index: u32) -> Option<&DirEntry> {
        self.entries
            .binary_search_by_key(&index, |entry| entry.index)
            .ok()
            .map(|at| &self.entries[at])
    }

    /// A run of `count` consecutive unused slots, if the directory has one.
    ///
    /// A memory lookup: the runs were recorded when the directory was
    /// parsed.
    pub(crate) fn free_run(&self, count: u32) -> Option<u32> {
        self.free
            .iter()
            .find(|&&(_, length)| length >= count)
            .map(|&(at, _)| at)
    }

    /// Parses a directory out of the bytes of its cluster chain.
    pub(crate) fn parse<E>(data: &[u8], codepage: &crate::codepage::Codepage) -> Result<Self, E> {
        let mut entries: Vec<DirEntry> = Vec::new();
        let mut free: Vec<(u32, u32)> = Vec::new();
        let mut pending: Option<PendingLongName> = None;
        // Where the run of long-name slots immediately in front of the next
        // entry begins. Tracked separately from `pending`, which a bad
        // sequence number abandons: the slots are still physically there
        // and still have to be freed with whatever follows them.
        let mut long_run: Option<u32> = None;
        let mut ended = false;

        for (index, entry) in data.chunks_exact(ENTRY_SIZE).enumerate() {
            let index = index as u32;
            match entry[0] {
                END_OF_DIRECTORY => {
                    // Everything past here should be untouched. The scan
                    // continues rather than stopping, because the whole
                    // directory is already in memory: checking costs
                    // nothing and catches entries no reader would list.
                    ended = true;
                    pending = None;
                    long_run = None;
                    note_free(&mut free, index);
                    continue;
                }
                DELETED => {
                    pending = None;
                    long_run = None;
                    note_free(&mut free, index);
                    continue;
                }
                _ => {}
            }

            if ended {
                return Err(Error::EntryAfterEnd {
                    index: index as usize,
                });
            }

            let attributes = Attributes(entry[0x0B]);
            if attributes.is_long_name() {
                absorb_long_name_slot(entry, &mut pending);
                long_run.get_or_insert(index);
                continue;
            }
            if attributes.is_volume_id() {
                pending = None;
                long_run = None;
                continue;
            }

            let short_name = ShortName::from_entry(entry);
            // A run whose checksum disagrees with this entry loses its long
            // name and nothing else — the file stays listed under its 8.3
            // name rather than disappearing.
            let long_name = pending
                .take()
                .and_then(|run| run.finish(short_name.checksum()));

            entries.push(DirEntry {
                short_display: short_name.to_display_string_with(codepage),
                short_name,
                long_name,
                attributes,
                first_cluster: (u32::from(read_u16(entry, 0x14)) << 16)
                    | u32::from(read_u16(entry, 0x1A)),
                size: read_u32(entry, 0x1C),
                index,
                first_slot: long_run.take().unwrap_or(index),
                created: DateTime::unpack(Packed {
                    date: read_u16(entry, 0x10),
                    time: read_u16(entry, 0x0E),
                    hundredths: entry[0x0D],
                }),
                // Access has a date and no time, and modification has no
                // hundredths field — the extra precision belongs to
                // creation alone.
                accessed: DateTime::unpack(Packed {
                    date: read_u16(entry, 0x12),
                    time: 0,
                    hundredths: 0,
                }),
                modified: DateTime::unpack(Packed {
                    date: read_u16(entry, 0x18),
                    time: read_u16(entry, 0x16),
                    hundredths: 0,
                }),
            });
        }

        let mut index = BTreeMap::new();
        for (at, entry) in entries.iter().enumerate() {
            // Short name first, so a long name wins a collision between the
            // two — that is the name a caller is more likely to have.
            index
                .entry(entry.short_display.to_uppercase())
                .or_insert(at);
            if let Some(long) = &entry.long_name {
                index.insert(long.to_uppercase(), at);
            }
        }

        Ok(Directory {
            entries,
            index,
            free,
        })
    }
}

/// Records that slot `index` is unused, extending the run in progress when
/// it is the one that just ended.
fn note_free(free: &mut Vec<(u32, u32)>, index: u32) {
    match free.last_mut() {
        Some((at, length)) if *at + *length == index => *length += 1,
        _ => free.push((index, 1)),
    }
}

/// Fills in a 32-byte short-name directory entry.
///
/// All three timestamps — creation, modification and access — are set from
/// `at`. A single instant is what a clock would give anyway, and writing the
/// same one to each is what every implementation does when it creates a
/// file.
///
/// Passing [`DateTime::EPOCH`] is the right move for a device with no clock,
/// and is not the same as leaving the fields zero: zero is month 0 of day 0,
/// which is not a date and which listings render as `1980-00-00`. See
/// [`crate::time`].
pub(crate) fn write_entry(
    into: &mut [u8],
    name: &ShortName,
    attributes: Attributes,
    first_cluster: u32,
    size: u32,
    at: DateTime,
) {
    let stamp = at.pack();

    into[..ENTRY_SIZE].fill(0);
    into[0..11].copy_from_slice(name.as_bytes());
    into[0x0B] = attributes.bits();
    into[0x0C] = name.case_flags;
    into[0x0D] = stamp.hundredths;
    into[0x0E..0x10].copy_from_slice(&stamp.time.to_le_bytes());
    into[0x10..0x12].copy_from_slice(&stamp.date.to_le_bytes());
    // Access has a date but no time: the format records when a file was
    // last read only to the day.
    into[0x12..0x14].copy_from_slice(&stamp.date.to_le_bytes());
    into[0x14..0x16].copy_from_slice(&((first_cluster >> 16) as u16).to_le_bytes());
    into[0x16..0x18].copy_from_slice(&stamp.time.to_le_bytes());
    into[0x18..0x1A].copy_from_slice(&stamp.date.to_le_bytes());
    into[0x1A..0x1C].copy_from_slice(&((first_cluster & 0xFFFF) as u16).to_le_bytes());
    into[0x1C..0x20].copy_from_slice(&size.to_le_bytes());
}

/// The `.` and `..` entries every directory but the root begins with.
///
/// `parent` is the parent's first cluster, **except** when the parent is the
/// root, where it must be 0. The format writes 0 there rather than the
/// root's own cluster number, which is the one place where the cluster an
/// entry names and the cluster the directory's data lives at are different
/// numbers. Writing the root's real cluster produces a volume that mounts
/// and reads correctly and that `fsck.vfat` rejects.
pub(crate) fn write_dot_entries(into: &mut [u8], cluster: u32, parent: u32, at: DateTime) {
    let attributes = Attributes::DIRECTORY;
    let dot = ShortName::from_raw(*b".          ", 0);
    let dot_dot = ShortName::from_raw(*b"..         ", 0);
    write_entry(&mut into[0..ENTRY_SIZE], &dot, attributes, cluster, 0, at);
    write_entry(
        &mut into[ENTRY_SIZE..ENTRY_SIZE * 2],
        &dot_dot,
        attributes,
        parent,
        0,
        at,
    );
}

/// Rewrites just the first cluster and size of an existing entry.
///
/// Everything else — the name, the attributes, the case flags — is left
/// exactly as it was, so updating a file cannot lose fields this crate does
/// not model.
pub(crate) fn update_entry(into: &mut [u8], first_cluster: u32, size: u32, at: DateTime) {
    let stamp = at.pack();
    into[0x14..0x16].copy_from_slice(&((first_cluster >> 16) as u16).to_le_bytes());
    into[0x16..0x18].copy_from_slice(&stamp.time.to_le_bytes());
    into[0x18..0x1A].copy_from_slice(&stamp.date.to_le_bytes());
    into[0x1A..0x1C].copy_from_slice(&((first_cluster & 0xFFFF) as u16).to_le_bytes());
    into[0x1C..0x20].copy_from_slice(&size.to_le_bytes());
}

/// Marks an entry deleted, leaving the rest of it readable.
pub(crate) fn mark_deleted(into: &mut [u8]) {
    into[0] = DELETED;
}

/// A long-name run being assembled.
struct PendingLongName {
    /// Characters, laid out by slot rather than in arrival order.
    chars: Vec<u16>,
    /// The sequence number the next slot must carry. Counts down to zero.
    expected: u8,
    /// The checksum every slot in the run agreed on.
    checksum: u8,
}

impl PendingLongName {
    fn new(slots: u8, checksum: u8) -> Self {
        PendingLongName {
            chars: vec![0; slots as usize * CHARS_PER_SLOT],
            expected: slots,
            checksum,
        }
    }

    /// Copies one slot's thirteen characters into their place.
    fn absorb(&mut self, sequence: u8, entry: &[u8]) {
        let at = (sequence as usize - 1) * CHARS_PER_SLOT;
        let mut chars = [0u16; CHARS_PER_SLOT];
        for (n, offset) in (0x01..0x0B).step_by(2).enumerate() {
            chars[n] = read_u16(entry, offset);
        }
        for (n, offset) in (0x0E..0x1A).step_by(2).enumerate() {
            chars[5 + n] = read_u16(entry, offset);
        }
        for (n, offset) in (0x1C..0x20).step_by(2).enumerate() {
            chars[11 + n] = read_u16(entry, offset);
        }
        self.chars[at..at + CHARS_PER_SLOT].copy_from_slice(&chars);
        self.expected = sequence - 1;
    }

    /// The finished name, or `None` if the run is incomplete or belongs to
    /// a different entry.
    fn finish(self, checksum: u8) -> Option<String> {
        if self.expected != 0 || self.checksum != checksum {
            return None;
        }
        // A name that exactly fills its slots has no terminator, so the
        // end is whichever comes first: a NUL, the 0xFFFF padding, or the
        // end of the buffer.
        let end = self
            .chars
            .iter()
            .position(|&c| c == 0x0000 || c == 0xFFFF)
            .unwrap_or(self.chars.len());

        let name: String = char::decode_utf16(self.chars[..end].iter().copied())
            .map(|c| c.unwrap_or('\u{FFFD}'))
            .collect();
        if name.is_empty() { None } else { Some(name) }
    }
}

/// Folds one long-name slot into the run being assembled.
///
/// A slot that does not follow on — wrong sequence number, or a checksum
/// disagreeing with the run so far — abandons the run rather than the
/// directory. If that slot is itself a run's first, it starts a new one,
/// which is what stops one damaged name from consuming the names after it.
fn absorb_long_name_slot(entry: &[u8], pending: &mut Option<PendingLongName>) {
    let sequence = entry[0] & !LAST_LONG_SLOT;
    let checksum = entry[0x0D];

    if entry[0] & LAST_LONG_SLOT != 0 {
        if sequence == 0 || sequence > MAX_LONG_SLOTS {
            *pending = None;
            return;
        }
        *pending = Some(PendingLongName::new(sequence, checksum));
    } else {
        let follows_on = matches!(
            pending.as_ref(),
            Some(run) if run.expected == sequence && run.checksum == checksum && sequence != 0
        );
        if !follows_on {
            *pending = None;
            return;
        }
    }

    if let Some(run) = pending.as_mut() {
        run.absorb(sequence, entry);
    }
}

fn read_u16(entry: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([entry[at], entry[at + 1]])
}

fn read_u32(entry: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([entry[at], entry[at + 1], entry[at + 2], entry[at + 3]])
}
