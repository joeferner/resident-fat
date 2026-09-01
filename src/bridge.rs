//! Using a device that implements `embedded-sdmmc`'s block device trait.
//!
//! [`FromEmbeddedSdmmc`] wraps anything implementing
//! [`embedded_sdmmc::BlockDevice`] so it can be handed to
//! [`FileSystem::mount`](crate::FileSystem::mount). That covers most block
//! devices already written for embedded Rust — an SD card driver's adapter,
//! most obviously — without either side being changed.
//!
//! # The copy, and why it is here
//!
//! `embedded-sdmmc` transfers a slice of [`Block`], a 512-byte newtype; this
//! crate transfers a plain `&[u8]`. Converting between the two would be free
//! if the slices could simply be reinterpreted, and they cannot: `Block` is
//! declared without a `repr` attribute, so its layout is whatever the
//! compiler chooses. In practice that is 512 bytes with no padding, and a
//! cast would work today — but "works today" is not a property a published
//! crate can rest an `unsafe` block on, and the failure mode if it ever
//! stopped being true is silent data corruption rather than a build error.
//!
//! So the bridge copies, through a staging buffer it owns. That is the cost
//! of the newtype, and it is the concrete case behind the argument in
//! [`crate::blockdev`] for transferring bytes: a design that pins nothing
//! down forces either a copy or an unsound cast on everyone who has to
//! interoperate with it.
//!
//! The copy is cheap in the only terms that matter here. A memory-to-memory
//! copy runs at hundreds of megabytes a second; a card write runs at tens of
//! kilobytes a second per command. The copy is lost in the noise of the
//! transfer it enables.
//!
//! # Why the staging buffer is bounded
//!
//! Sized once, at construction, and never grown. A buffer that matched
//! whatever the filesystem asked for would allocate megabytes on a large
//! read, which is exactly what a bare-metal consumer cannot have happening
//! inside a driver.
//!
//! Instead the wrapper reports its capacity through
//! [`max_transfer_blocks`](crate::blockdev::BlockDevice::max_transfer_blocks)
//! and the filesystem splits longer transfers to fit. So the buffer size is
//! also the longest multi-block command the card will see, which makes it
//! the knob that decides how much of this crate's advantage survives the
//! bridge — see [`FromEmbeddedSdmmc::with_blocks`].

use alloc::vec;
use alloc::vec::Vec;

use embedded_sdmmc::{Block, BlockIdx};

use crate::blockdev::{BLOCK_SIZE, BlockDevice};

/// Blocks staged per transfer by default: 128, or 64 KiB.
///
/// Chosen as the point where the per-command cost has stopped mattering.
/// A write to an SD card is dominated by a fixed cost per command — on the
/// order of tens of milliseconds, whether the command carries one block or a
/// hundred — so what matters is how few commands a transfer becomes, and the
/// returns fall off sharply. Going from 1 block to 128 turns a 1.6 MB write
/// from 3232 commands into 26; going from 128 to 1024 turns 26 into 4, for
/// eight times the memory.
pub const DEFAULT_STAGING_BLOCKS: usize = 128;

/// Adapts an [`embedded_sdmmc::BlockDevice`] to this crate's
/// [`BlockDevice`].
///
/// Owns the wrapped device. Get it back with
/// [`into_inner`](Self::into_inner), or reach it in place with
/// [`inner`](Self::inner) — a device that carries state worth reading, such
/// as one counting transfers, is the reason those exist.
///
/// # Addressing limit
///
/// This crate numbers blocks in a `u64`; `embedded_sdmmc::BlockIdx` is a
/// `u32`. So the bridge reaches the first 2 TiB of a device and no further,
/// and a block number above that is **truncated rather than refused** — the
/// access lands somewhere else on the device instead of failing.
///
/// Not something this bridge can improve on. Reporting it would mean
/// returning an error of the wrapped device's own type, and there is no way
/// to construct one of those from here — `embedded_sdmmc::BlockDevice::Error`
/// is an associated type belonging to the driver. Refusing at construction
/// would not work either, since a device may report no size at all.
///
/// It is also not a limit worth engineering around: the ceiling comes from
/// the trait this exists to accept, and cards that large are not what
/// `embedded-sdmmc` drivers are written for. Implement
/// [`BlockDevice`] directly for a device that needs the full range — it
/// takes a `u64` throughout.
pub struct FromEmbeddedSdmmc<D> {
    device: D,
    staging: Vec<Block>,
}

impl<D: embedded_sdmmc::BlockDevice> FromEmbeddedSdmmc<D> {
    /// Wraps `device`, staging [`DEFAULT_STAGING_BLOCKS`] blocks per
    /// transfer.
    pub fn new(device: D) -> Self {
        Self::with_blocks(device, DEFAULT_STAGING_BLOCKS)
    }

    /// Wraps `device`, staging `blocks` blocks per transfer.
    ///
    /// This is the trade the bridge makes visible: `blocks * 512` bytes of
    /// memory, held for the life of the wrapper, in exchange for transfers
    /// that reach the card as commands of up to that many blocks instead of
    /// one command per block.
    ///
    /// Raising it helps until the card's own sequential throughput becomes
    /// the limit, and past that it buys nothing. Lowering it to 1 gives the
    /// behaviour of a filesystem with no run detection at all, which is a
    /// useful thing to be able to measure against and a bad thing to ship.
    ///
    /// # Panics
    ///
    /// If `blocks` is zero. A staging buffer that holds nothing cannot
    /// transfer anything, and failing at construction beats failing on the
    /// first read.
    pub fn with_blocks(device: D, blocks: usize) -> Self {
        assert!(blocks > 0, "a staging buffer must hold at least one block");
        FromEmbeddedSdmmc {
            device,
            staging: vec![Block::new(); blocks],
        }
    }

    /// The wrapped device.
    pub fn inner(&self) -> &D {
        &self.device
    }

    /// The wrapped device, mutably.
    pub fn inner_mut(&mut self) -> &mut D {
        &mut self.device
    }

    /// Gives the wrapped device back, dropping the staging buffer.
    pub fn into_inner(self) -> D {
        self.device
    }

    /// How many blocks fit in one staged transfer.
    fn capacity(&self) -> usize {
        self.staging.len()
    }
}

impl<D: embedded_sdmmc::BlockDevice> BlockDevice for FromEmbeddedSdmmc<D> {
    type Error = D::Error;

    /// Reads into `blocks` through the staging buffer.
    ///
    /// The filesystem has already split the request to fit
    /// [`max_transfer_blocks`](Self::max_transfer_blocks), so this is
    /// normally one call to the wrapped device. The loop is here because
    /// nothing in the trait *obliges* a caller to respect that, and reading
    /// past the end of the buffer would be worse than looping.
    fn read(&mut self, start_block: u64, blocks: &mut [u8]) -> Result<(), Self::Error> {
        debug_assert_eq!(blocks.len() % BLOCK_SIZE, 0);

        for (batch, chunk) in blocks.chunks_mut(self.capacity() * BLOCK_SIZE).enumerate() {
            let count = chunk.len() / BLOCK_SIZE;
            let at = start_block + (batch * self.capacity()) as u64;
            self.device
                .read(&mut self.staging[..count], BlockIdx(at as u32))?;

            for (block, out) in self.staging[..count]
                .iter()
                .zip(chunk.chunks_mut(BLOCK_SIZE))
            {
                out.copy_from_slice(&block.contents);
            }
        }
        Ok(())
    }

    /// Writes `blocks` through the staging buffer.
    fn write(&mut self, start_block: u64, blocks: &[u8]) -> Result<(), Self::Error> {
        debug_assert_eq!(blocks.len() % BLOCK_SIZE, 0);

        for (batch, chunk) in blocks.chunks(self.capacity() * BLOCK_SIZE).enumerate() {
            let count = chunk.len() / BLOCK_SIZE;
            for (block, source) in self.staging[..count]
                .iter_mut()
                .zip(chunk.chunks(BLOCK_SIZE))
            {
                block.contents.copy_from_slice(source);
            }

            let at = start_block + (batch * self.capacity()) as u64;
            self.device
                .write(&self.staging[..count], BlockIdx(at as u32))?;
        }
        Ok(())
    }

    /// The wrapped device's block count, or `None` if it declined to say.
    ///
    /// An error from the wrapped device is reported as "unknown" rather than
    /// propagated, which is the one place this bridge deliberately loses
    /// information. `embedded-sdmmc`'s trait returns a plain
    /// `Result<BlockCount, _>` with no way to express "I do not know", so a
    /// driver that does not read the card's capacity has nothing to return
    /// but an error — typically an "unsupported" variant standing in for a
    /// query it never expected to be asked.
    ///
    /// Propagating that would make such a device unmountable, which is a
    /// worse answer than skipping a sanity check. A card that genuinely
    /// failed rather than merely declined will say so on the first read, and
    /// mounting reads the boot sector immediately.
    fn block_count(&mut self) -> Result<Option<u64>, Self::Error> {
        Ok(self
            .device
            .num_blocks()
            .ok()
            .map(|count| u64::from(count.0)))
    }

    /// The staging buffer's capacity.
    ///
    /// Not a hardware limit but this wrapper's own, which comes to the same
    /// thing from the filesystem's side: it is the longest transfer that can
    /// be made in one call, so it is what the filesystem must split to.
    fn max_transfer_blocks(&self) -> u64 {
        self.capacity() as u64
    }
}

impl<D> core::fmt::Debug for FromEmbeddedSdmmc<D> {
    /// Written out rather than derived: the wrapped device need not be
    /// `Debug`, and printing the staging buffer would dump 64 KiB of
    /// whatever was last transferred into the first panic message that
    /// mentions it.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("FromEmbeddedSdmmc")
            .field("staging_blocks", &self.staging.len())
            .finish_non_exhaustive()
    }
}
