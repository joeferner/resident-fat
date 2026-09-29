//! Counting what the device is actually asked to do.
//!
//! A slow write is not a diagnosis. Too many bytes and too many *commands*
//! for the bytes call for different fixes, and on an SD card it is almost
//! always the second: the card charges per command, not per block, so the
//! same file written one block at a time and written as one run can differ
//! by an order of magnitude in wall-clock time with identical data. A
//! stopwatch cannot tell those apart, which is how "the card is slow"
//! survives as an explanation.
//!
//! So [`Counted`] counts calls as well as blocks. Blocks moved is the work
//! that has to happen; calls are the overhead; and blocks per call is the
//! figure a change to how a volume is written actually moves — see
//! [`Counts::blocks_per_call_x10`].
//!
//! ```ignore
//! use resident_fat::counted::{Counted, Counters};
//!
//! static CARD_COUNTS: Counters = Counters::new();
//!
//! let volume = FileSystem::mount(Counted::new(device, &CARD_COUNTS))?;
//! // … later, from wherever the numbers are wanted:
//! let before = CARD_COUNTS.now();
//! write_the_update(&mut volume)?;
//! logln!("update: {}", CARD_COUNTS.since(before));
//! ```
//!
//! # Why the counters are the caller's
//!
//! The code that wants a snapshot is often not the code that holds the
//! volume — a volume shared between tasks is behind a mutex, and a report
//! is written outside it. So the counters are a [`Counters`] the caller
//! puts where it likes, usually a `static`, and the wrapper holds a
//! reference to them. One set per device; nothing global here.
//!
//! They are free-running `u32`s, and [`Counters::since`] subtracts with
//! wrapping arithmetic, so a snapshot pair stays right across a wrap rather
//! than reporting a long operation as a fast one.
//!
//! Only on targets with 32-bit atomics, which every core this crate is
//! built for has; a core without them does not get this module.

use core::fmt;
use core::sync::atomic::{AtomicU32, Ordering};

use crate::blockdev::{BLOCK_SIZE, BlockDevice};

/// Running totals of what a [`Counted`] device has been asked to do.
///
/// Atomics, so a snapshot can be taken from a different task — or a
/// different core — than the one using the device, without either
/// borrowing the other.
#[derive(Debug)]
pub struct Counters {
    read_calls: AtomicU32,
    read_blocks: AtomicU32,
    write_calls: AtomicU32,
    write_blocks: AtomicU32,
}

impl Counters {
    /// All zero. `const`, so it can be a `static`.
    pub const fn new() -> Self {
        Counters {
            read_calls: AtomicU32::new(0),
            read_blocks: AtomicU32::new(0),
            write_calls: AtomicU32::new(0),
            write_blocks: AtomicU32::new(0),
        }
    }

    /// The totals, now.
    ///
    /// Absolute values mean little on their own — mounting and whatever ran
    /// before have already used the device — so take one before an
    /// operation and hand it to [`since`](Self::since) after.
    pub fn now(&self) -> Counts {
        Counts {
            read_calls: self.read_calls.load(Ordering::Relaxed),
            read_blocks: self.read_blocks.load(Ordering::Relaxed),
            write_calls: self.write_calls.load(Ordering::Relaxed),
            write_blocks: self.write_blocks.load(Ordering::Relaxed),
        }
    }

    /// What has happened since `before` was taken from these counters.
    pub fn since(&self, before: Counts) -> Counts {
        let now = self.now();
        Counts {
            read_calls: now.read_calls.wrapping_sub(before.read_calls),
            read_blocks: now.read_blocks.wrapping_sub(before.read_blocks),
            write_calls: now.write_calls.wrapping_sub(before.write_calls),
            write_blocks: now.write_blocks.wrapping_sub(before.write_blocks),
        }
    }

    fn count(calls: &AtomicU32, blocks: &AtomicU32, bytes: usize) {
        calls.fetch_add(1, Ordering::Relaxed);
        // Truncated rather than checked: a single transfer of more than
        // four billion blocks is two terabytes, and wrapping is the counters'
        // arithmetic anyway.
        blocks.fetch_add((bytes / BLOCK_SIZE) as u32, Ordering::Relaxed);
    }
}

impl Default for Counters {
    fn default() -> Self {
        Counters::new()
    }
}

/// What a device was asked to do: a snapshot, or the difference of two.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    /// Calls to the device's `read`.
    pub read_calls: u32,
    /// Blocks those reads covered.
    pub read_blocks: u32,
    /// Calls to the device's `write`.
    pub write_calls: u32,
    /// Blocks those writes covered.
    pub write_blocks: u32,
}

impl Counts {
    /// Blocks per call, reads and writes together, times ten.
    ///
    /// The figure the counting exists for: 10 is a command per block, the
    /// floor a block-at-a-time implementation cannot leave. Scaled rather
    /// than fractional so it needs no floating point, and by ten rather
    /// than rounded, which would show every improvement below 2.0 as none.
    /// Zero when there were no calls.
    pub fn blocks_per_call_x10(&self) -> u32 {
        let calls = u64::from(self.read_calls) + u64::from(self.write_calls);
        if calls == 0 {
            return 0;
        }
        let blocks = u64::from(self.read_blocks) + u64::from(self.write_blocks);
        u32::try_from(blocks * 10 / calls).unwrap_or(u32::MAX)
    }
}

/// `12 reads/4096 blocks, 3 writes/3210 blocks, 487.0 blocks per call`.
impl fmt::Display for Counts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let ratio = self.blocks_per_call_x10();
        write!(
            f,
            "{} reads/{} blocks, {} writes/{} blocks, {}.{} blocks per call",
            self.read_calls,
            self.read_blocks,
            self.write_calls,
            self.write_blocks,
            ratio / 10,
            ratio % 10
        )
    }
}

/// A block device that counts every call made to it into a [`Counters`].
///
/// Transparent otherwise — two relaxed atomic adds per call, nothing next
/// to a card transaction — so it can stay in a shipping build rather than
/// being a harness put back each time a number is wanted.
#[derive(Debug)]
pub struct Counted<'c, D> {
    device: D,
    counters: &'c Counters,
}

impl<'c, D> Counted<'c, D> {
    /// Wraps `device`, counting into `counters`.
    pub fn new(device: D, counters: &'c Counters) -> Self {
        Counted { device, counters }
    }

    /// The wrapped device.
    pub fn get_ref(&self) -> &D {
        &self.device
    }

    /// The wrapped device, mutably. Calls made through this are not
    /// counted.
    pub fn get_mut(&mut self) -> &mut D {
        &mut self.device
    }

    /// The wrapped device, unwrapped.
    pub fn into_inner(self) -> D {
        self.device
    }
}

impl<D: BlockDevice> BlockDevice for Counted<'_, D> {
    type Error = D::Error;

    /// Counts the call, then forwards it unchanged.
    fn read(&mut self, start_block: u64, blocks: &mut [u8]) -> Result<(), Self::Error> {
        let c = self.counters;
        Counters::count(&c.read_calls, &c.read_blocks, blocks.len());
        self.device.read(start_block, blocks)
    }

    /// Counts the call, then forwards it unchanged.
    fn write(&mut self, start_block: u64, blocks: &[u8]) -> Result<(), Self::Error> {
        let c = self.counters;
        Counters::count(&c.write_calls, &c.write_blocks, blocks.len());
        self.device.write(start_block, blocks)
    }

    /// Forwarded, and not counted: it moves nothing.
    fn block_count(&mut self) -> Result<Option<u64>, Self::Error> {
        self.device.block_count()
    }

    /// Forwarded, and it has to be.
    ///
    /// The default is no limit, and taking it would have transfers split to
    /// a ceiling this wrapper invented rather than the one the device has —
    /// so a long write would go straight through and be refused as too many
    /// blocks. This is the method where a wrapper that forgets to pass
    /// something on fails at run time rather than at compile time.
    fn max_transfer_blocks(&self) -> u64 {
        self.device.max_transfer_blocks()
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use std::format;

    use super::*;

    #[test]
    fn a_snapshot_pair_is_right_across_the_wrap() {
        let counters = Counters::new();
        counters.read_calls.store(u32::MAX - 1, Ordering::Relaxed);
        counters.read_blocks.store(u32::MAX, Ordering::Relaxed);
        let before = counters.now();
        Counters::count(&counters.read_calls, &counters.read_blocks, 3 * BLOCK_SIZE);
        Counters::count(&counters.read_calls, &counters.read_blocks, 2 * BLOCK_SIZE);
        let since = counters.since(before);
        assert_eq!(since.read_calls, 2);
        assert_eq!(since.read_blocks, 5);
    }

    #[test]
    fn blocks_per_call() {
        assert_eq!(Counts::default().blocks_per_call_x10(), 0, "no calls");
        let one_each = Counts {
            read_calls: 3,
            read_blocks: 3,
            write_calls: 1,
            write_blocks: 1,
        };
        assert_eq!(one_each.blocks_per_call_x10(), 10, "the floor");
        let runs = Counts {
            read_calls: 2,
            read_blocks: 7,
            write_calls: 0,
            write_blocks: 0,
        };
        assert_eq!(runs.blocks_per_call_x10(), 35);
        // Sums that would overflow a `u32` do not.
        let huge = Counts {
            read_calls: 1,
            read_blocks: u32::MAX,
            write_calls: 1,
            write_blocks: u32::MAX,
        };
        assert_eq!(huge.blocks_per_call_x10(), u32::MAX);
    }

    #[test]
    fn the_console_line() {
        let counts = Counts {
            read_calls: 12,
            read_blocks: 4096,
            write_calls: 3,
            write_blocks: 3210,
        };
        assert_eq!(
            format!("{counts}"),
            "12 reads/4096 blocks, 3 writes/3210 blocks, 487.0 blocks per call"
        );
    }
}
