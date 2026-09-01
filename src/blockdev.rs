//! The block device this filesystem is layered on.
//!
//! # Why a byte slice rather than a block type
//!
//! Transfers are `&[u8]` whose length is a multiple of [`BLOCK_SIZE`], not
//! a slice of some 512-byte newtype. That is the whole reason this trait
//! exists rather than reusing an established one.
//!
//! A newtype forces every caller to own its data in that shape. Since a
//! filesystem's callers hand it arbitrary buffers — a slice of a firmware
//! image sitting at some offset inside an upload, say — the data has to be
//! copied into blocks, or reinterpreted through a cast that is only sound
//! if the newtype pins its representation. A byte slice has neither
//! problem: a caller's buffer goes to the device untouched.
//!
//! It also makes the unit of transfer a *run* rather than a block, which is
//! what this crate is built to exploit. Reading a contiguous megabyte is
//! one call with a long slice, not two thousand calls with short ones.

/// The only block size this crate supports.
///
/// Every SD card, and effectively every disk a FAT volume is found on,
/// uses 512-byte sectors. A volume whose boot sector claims otherwise is
/// rejected at mount rather than handled.
pub const BLOCK_SIZE: usize = 512;

/// A readable and writable array of fixed-size blocks.
///
/// Implementations are expected to be cheap to call with *large* slices —
/// that is where this crate's performance comes from. An implementation
/// that internally splits a long transfer into one command per block gives
/// up most of the benefit of using this crate at all.
pub trait BlockDevice {
    /// What can go wrong at the device level.
    ///
    /// `Debug` and nothing more, so a plain `#[derive(Debug)]` enum is a
    /// complete implementation. This crate's [`Error`](crate::Error) prints
    /// it with `{:?}`, which is what lets that type stay printable for every
    /// device error the bound admits rather than only for those that also
    /// implement `Display` — see [`Error::Device`](crate::Error::Device).
    type Error: core::fmt::Debug;

    /// Fills `blocks` from consecutive device blocks starting at
    /// `start_block`.
    ///
    /// `blocks.len()` is a multiple of [`BLOCK_SIZE`]; implementations may
    /// panic or return an error otherwise. A read that runs past the end of
    /// the device is an error, not a short read.
    fn read(&mut self, start_block: u64, blocks: &mut [u8]) -> Result<(), Self::Error>;

    /// Writes `blocks` to consecutive device blocks starting at
    /// `start_block`.
    ///
    /// The same length rule as [`read`](Self::read) applies. A successful
    /// return means the device accepted the data; whether it is durable
    /// depends on the device, which is why the filesystem has an explicit
    /// sync rather than assuming.
    fn write(&mut self, start_block: u64, blocks: &[u8]) -> Result<(), Self::Error>;

    /// How many blocks the device holds, or `None` if it cannot say.
    ///
    /// Used to sanity-check a volume's own claims about its size: a boot
    /// sector describing more sectors than the device has is reported at
    /// mount rather than as a puzzling read error later.
    ///
    /// # Why `None` is allowed
    ///
    /// Because plenty of real drivers do not know. An SD card's capacity
    /// lives in its CSD register, which a driver has to issue a command to
    /// read and which a driver written only to move blocks has no other
    /// reason to fetch — so a great many of them never do. Refusing to mount
    /// a perfectly good volume because the *device* is reticent would be the
    /// filesystem punishing a caller for a limitation one layer down.
    ///
    /// Nothing that matters is lost. The check is defence in depth, not a
    /// safety bound: the resident table is already sized by the allocation
    /// table's own capacity rather than by this number, and a read that runs
    /// off the end of a device fails at the device. Returning `None` costs a
    /// clear error at mount and nothing else.
    ///
    /// `None` and an error mean different things and should not be
    /// substituted for one another: `None` is "I do not know", an error is
    /// "I tried and the device failed".
    fn block_count(&mut self) -> Result<Option<u64>, Self::Error>;

    /// The most blocks this device will move in one call.
    ///
    /// Override it when the hardware has a real limit. An SD controller
    /// counts blocks in a 16-bit field, for instance, so it can express a
    /// run of at most 65535 — and a filesystem that hands it more gets an
    /// error rather than a split transfer.
    ///
    /// Saying so here rather than splitting internally keeps the cost
    /// visible: this crate exists to issue few large transfers, and a
    /// device that quietly divided them would make that claim untestable
    /// from the outside. The default of no limit suits anything
    /// memory-backed.
    fn max_transfer_blocks(&self) -> u64 {
        u64::MAX
    }
}
