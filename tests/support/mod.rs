//! Shared harness for the host tests: block devices over the fixture
//! images, a recorder that counts what reaches them, and thin wrappers
//! around the external oracles.
//!
//! Nothing here is part of the crate. It exists so a test can say what it
//! means — "this read cost one device call", "`fsck` is happy with what we
//! wrote" — without repeating the plumbing.

#![allow(dead_code)] // Each test binary uses a different part of this.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use resident_fat::blockdev::{BLOCK_SIZE, BlockDevice};

/// Where `scripts/mkfixtures.sh` puts the images.
pub fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// Path to one fixture image, with a message worth reading if it is
/// missing — a fresh clone has no fixtures until they are built, and
/// "No such file or directory" does not say so.
pub fn fixture(name: &str) -> PathBuf {
    let path = fixtures_dir().join(name);
    assert!(
        path.exists(),
        "fixture {name} is missing. Run `make fixtures` (or ./scripts/mkfixtures.sh) to build it."
    );
    path
}

/// A scratch directory that cargo cleans up, for images a test builds or
/// mutates. Never the fixtures themselves: those are shared, and a test
/// that writes to one corrupts every test after it.
pub fn scratch_dir() -> PathBuf {
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    std::fs::create_dir_all(&path).expect("could not create the scratch directory");
    path
}

// ---------------------------------------------------------------------------
// Block devices
// ---------------------------------------------------------------------------

/// A block device over an image file.
///
/// File-backed rather than reading the image into memory: the fixtures are
/// sparse and nominally gigabytes, so a `Vec` would turn 28 MB on disk into
/// 3.5 GB of RAM.
pub struct FileImage {
    file: File,
    blocks: u64,
}

impl FileImage {
    /// Opens an image read-only.
    pub fn open(path: impl AsRef<Path>) -> std::io::Result<Self> {
        Self::from_file(File::open(path)?)
    }

    /// Opens an image for reading and writing.
    pub fn open_rw(path: impl AsRef<Path>) -> std::io::Result<Self> {
        Self::from_file(File::options().read(true).write(true).open(path)?)
    }

    fn from_file(file: File) -> std::io::Result<Self> {
        let blocks = file.metadata()?.len() / BLOCK_SIZE as u64;
        Ok(Self { file, blocks })
    }
}

impl BlockDevice for FileImage {
    type Error = std::io::Error;

    fn read(&mut self, start_block: u64, blocks: &mut [u8]) -> Result<(), Self::Error> {
        check_length(blocks.len());
        self.file
            .seek(SeekFrom::Start(start_block * BLOCK_SIZE as u64))?;
        self.file.read_exact(blocks)
    }

    fn write(&mut self, start_block: u64, blocks: &[u8]) -> Result<(), Self::Error> {
        check_length(blocks.len());
        self.file
            .seek(SeekFrom::Start(start_block * BLOCK_SIZE as u64))?;
        self.file.write_all(blocks)
    }

    fn block_count(&mut self) -> Result<Option<u64>, Self::Error> {
        Ok(Some(self.blocks))
    }
}

/// A block device over a `Vec<u8>`, for images small enough to hold in
/// memory — chiefly deliberately corrupted copies, which must never be
/// written back to a shared fixture.
pub struct MemoryImage {
    bytes: Vec<u8>,
}

impl MemoryImage {
    /// Reads a whole image into memory. Only sensible for the small ones.
    pub fn load(path: impl AsRef<Path>) -> std::io::Result<Self> {
        Ok(Self {
            bytes: std::fs::read(path)?,
        })
    }

    /// An image over bytes a test already has, usually a doctored copy.
    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        Self { bytes }
    }

    /// The image's bytes.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// The image's bytes, for a test that wants to corrupt something.
    pub fn bytes_mut(&mut self) -> &mut [u8] {
        &mut self.bytes
    }

    /// Writes the image out, so an external oracle can be pointed at it.
    pub fn save(&self, path: impl AsRef<Path>) -> std::io::Result<()> {
        std::fs::write(path, &self.bytes)
    }
}

impl BlockDevice for MemoryImage {
    type Error = std::io::Error;

    fn read(&mut self, start_block: u64, blocks: &mut [u8]) -> Result<(), Self::Error> {
        check_length(blocks.len());
        let at = start_block as usize * BLOCK_SIZE;
        let end = at + blocks.len();
        if end > self.bytes.len() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                format!(
                    "read of blocks {start_block}..{} runs past the image",
                    end / BLOCK_SIZE
                ),
            ));
        }
        blocks.copy_from_slice(&self.bytes[at..end]);
        Ok(())
    }

    fn write(&mut self, start_block: u64, blocks: &[u8]) -> Result<(), Self::Error> {
        check_length(blocks.len());
        let at = start_block as usize * BLOCK_SIZE;
        let end = at + blocks.len();
        if end > self.bytes.len() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                format!(
                    "write of blocks {start_block}..{} runs past the image",
                    end / BLOCK_SIZE
                ),
            ));
        }
        self.bytes[at..end].copy_from_slice(blocks);
        Ok(())
    }

    fn block_count(&mut self) -> Result<Option<u64>, Self::Error> {
        Ok(Some((self.bytes.len() / BLOCK_SIZE) as u64))
    }
}

// Written out rather than derived, for both devices and the recorder: a
// derived `Debug` on `MemoryImage` would print sixteen million bytes into
// the first panic message that mentions it.

impl std::fmt::Debug for FileImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileImage")
            .field("blocks", &self.blocks)
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for MemoryImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryImage")
            .field("blocks", &(self.bytes.len() / BLOCK_SIZE))
            .finish_non_exhaustive()
    }
}

impl<D: std::fmt::Debug> std::fmt::Debug for Recorder<D> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Recorder")
            .field("inner", &self.inner)
            .field("calls", &self.accesses.len())
            .finish()
    }
}

fn check_length(len: usize) {
    assert!(
        len != 0 && len % BLOCK_SIZE == 0,
        "transfer of {len} bytes is not a non-zero multiple of {BLOCK_SIZE}"
    );
}

// ---------------------------------------------------------------------------
// Recording
// ---------------------------------------------------------------------------

/// Whether an access read from or wrote to the device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// A read.
    Read,
    /// A write.
    Write,
}

/// One device access, as the filesystem issued it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Access {
    /// Read or write.
    pub direction: Direction,
    /// First block touched.
    pub start_block: u64,
    /// How many blocks moved in this one call.
    pub blocks: u64,
}

/// Wraps a block device and records every access.
///
/// This is the crate's central claim made testable. "A contiguous file is
/// read in one device call" is not something a timing measurement can
/// establish — it is a statement about how many calls were issued and how
/// long each one was, which is exactly what this collects. A test asserting
/// on these counts fails loudly when a change quietly reintroduces
/// per-block I/O, which is the regression that matters most here and the
/// one least likely to show up any other way.
pub struct Recorder<D> {
    inner: D,
    accesses: Vec<Access>,
}

impl<D: BlockDevice> Recorder<D> {
    /// Starts recording accesses to `inner`.
    pub fn new(inner: D) -> Self {
        Self {
            inner,
            accesses: Vec::new(),
        }
    }

    /// Every access so far, in order.
    pub fn accesses(&self) -> &[Access] {
        &self.accesses
    }

    /// Just the reads, in order.
    pub fn reads(&self) -> Vec<Access> {
        self.filter(Direction::Read)
    }

    /// Just the writes, in order.
    pub fn writes(&self) -> Vec<Access> {
        self.filter(Direction::Write)
    }

    fn filter(&self, direction: Direction) -> Vec<Access> {
        self.accesses
            .iter()
            .copied()
            .filter(|a| a.direction == direction)
            .collect()
    }

    /// How many device calls have been made.
    pub fn call_count(&self) -> usize {
        self.accesses.len()
    }

    /// Total blocks moved, across every call. Paired with
    /// [`call_count`](Self::call_count) this distinguishes "one big
    /// transfer" from "many small ones moving the same data", which is the
    /// distinction the whole design turns on.
    pub fn blocks_moved(&self) -> u64 {
        self.accesses.iter().map(|a| a.blocks).sum()
    }

    /// Forgets everything recorded so far, so a test can measure one
    /// operation without the setup that preceded it.
    pub fn reset(&mut self) {
        self.accesses.clear();
    }

    /// The wrapped device.
    pub fn inner_mut(&mut self) -> &mut D {
        &mut self.inner
    }
}

impl<D: BlockDevice> BlockDevice for Recorder<D> {
    type Error = D::Error;

    fn read(&mut self, start_block: u64, blocks: &mut [u8]) -> Result<(), Self::Error> {
        self.accesses.push(Access {
            direction: Direction::Read,
            start_block,
            blocks: (blocks.len() / BLOCK_SIZE) as u64,
        });
        self.inner.read(start_block, blocks)
    }

    fn write(&mut self, start_block: u64, blocks: &[u8]) -> Result<(), Self::Error> {
        self.accesses.push(Access {
            direction: Direction::Write,
            start_block,
            blocks: (blocks.len() / BLOCK_SIZE) as u64,
        });
        self.inner.write(start_block, blocks)
    }

    fn block_count(&mut self) -> Result<Option<u64>, Self::Error> {
        self.inner.block_count()
    }

    /// Forwarded rather than defaulted, so wrapping a limited device in a
    /// recorder does not hide its limit.
    fn max_transfer_blocks(&self) -> u64 {
        self.inner.max_transfer_blocks()
    }
}

/// A device that refuses transfers longer than `cap` blocks.
///
/// Stands in for hardware with a real limit — an SD controller counts
/// blocks in a 16-bit field, so it cannot express a run longer than 65535.
/// The refusal is the point: a filesystem that ignored
/// [`BlockDevice::max_transfer_blocks`] would fail here rather than
/// silently working because the test device happened to be forgiving.
pub struct Capped<D> {
    inner: D,
    cap: u64,
}

impl<D: BlockDevice> Capped<D> {
    /// Wraps `inner`, refusing anything longer than `cap` blocks.
    pub fn new(inner: D, cap: u64) -> Self {
        Self { inner, cap }
    }

    fn check(&self, blocks: usize) {
        assert!(
            blocks as u64 <= self.cap,
            "transfer of {blocks} blocks exceeds the device's limit of {}",
            self.cap
        );
    }
}

impl<D: BlockDevice> BlockDevice for Capped<D> {
    type Error = D::Error;

    fn read(&mut self, start_block: u64, blocks: &mut [u8]) -> Result<(), Self::Error> {
        self.check(blocks.len() / BLOCK_SIZE);
        self.inner.read(start_block, blocks)
    }

    fn write(&mut self, start_block: u64, blocks: &[u8]) -> Result<(), Self::Error> {
        self.check(blocks.len() / BLOCK_SIZE);
        self.inner.write(start_block, blocks)
    }

    fn block_count(&mut self) -> Result<Option<u64>, Self::Error> {
        self.inner.block_count()
    }

    fn max_transfer_blocks(&self) -> u64 {
        self.cap
    }
}

impl<D: std::fmt::Debug> std::fmt::Debug for Capped<D> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Capped")
            .field("inner", &self.inner)
            .field("cap", &self.cap)
            .finish()
    }
}

/// A device that does not know how big it is.
///
/// Stands in for a driver that never reads the card's capacity: the CSD
/// register holds the answer, and a driver written only to move blocks has
/// no other reason to fetch it. This is not a contrived case — it is the
/// ordinary shape of an SD driver, and such a device could not be mounted at
/// all until the trait allowed the answer to be "I do not know".
pub struct Reticent<D>(pub D);

impl<D: BlockDevice> BlockDevice for Reticent<D> {
    type Error = D::Error;

    fn read(&mut self, start_block: u64, blocks: &mut [u8]) -> Result<(), Self::Error> {
        self.0.read(start_block, blocks)
    }

    fn write(&mut self, start_block: u64, blocks: &[u8]) -> Result<(), Self::Error> {
        self.0.write(start_block, blocks)
    }

    /// The point of the type.
    fn block_count(&mut self) -> Result<Option<u64>, Self::Error> {
        Ok(None)
    }

    fn max_transfer_blocks(&self) -> u64 {
        self.0.max_transfer_blocks()
    }
}

/// A device that fails every write after the first `allowed`.
///
/// Stands in for a card yanked mid-operation. The ordering rules in the
/// plan exist entirely for this case, so a rule nothing has interrupted is
/// a rule nothing has tested: what these failures must leave behind is a
/// volume `fsck.vfat` still accepts, with clusters leaked at worst.
///
/// Reads keep working, because a card that stops accepting writes has not
/// necessarily stopped answering reads, and failing both would mostly test
/// that the code gives up early.
pub struct FailAfter<D> {
    inner: D,
    remaining: std::cell::Cell<u32>,
}

impl<D: BlockDevice> FailAfter<D> {
    /// Wraps `inner`, allowing `allowed` writes before every one fails.
    pub fn new(inner: D, allowed: u32) -> Self {
        Self {
            inner,
            remaining: std::cell::Cell::new(allowed),
        }
    }
}

impl<D: BlockDevice<Error = std::io::Error>> BlockDevice for FailAfter<D> {
    type Error = std::io::Error;

    fn read(&mut self, start_block: u64, blocks: &mut [u8]) -> Result<(), Self::Error> {
        self.inner.read(start_block, blocks)
    }

    fn write(&mut self, start_block: u64, blocks: &[u8]) -> Result<(), Self::Error> {
        let left = self.remaining.get();
        if left == 0 {
            return Err(std::io::Error::other("injected write failure"));
        }
        self.remaining.set(left - 1);
        self.inner.write(start_block, blocks)
    }

    fn block_count(&mut self) -> Result<Option<u64>, Self::Error> {
        self.inner.block_count()
    }

    fn max_transfer_blocks(&self) -> u64 {
        self.inner.max_transfer_blocks()
    }
}

impl<D: std::fmt::Debug> std::fmt::Debug for FailAfter<D> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FailAfter")
            .field("inner", &self.inner)
            .field("writes_left", &self.remaining.get())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// The other block device trait
// ---------------------------------------------------------------------------

/// An `embedded-sdmmc` block device over an image file, counting its calls.
///
/// Deliberately implements the *other* crate's trait and not ours, so the
/// bridge is exercised as a consumer would exercise it: with a device that
/// knows nothing about this crate. The counts are what the bridge has to
/// justify — a wrapper that quietly issued one command per block would read
/// the same bytes and be worthless.
#[cfg(feature = "embedded-sdmmc")]
pub struct SdmmcImage {
    file: std::cell::RefCell<File>,
    blocks: u64,
    calls: std::cell::Cell<usize>,
    blocks_moved: std::cell::Cell<usize>,
}

#[cfg(feature = "embedded-sdmmc")]
impl SdmmcImage {
    /// Opens an image for reading and writing.
    pub fn open_rw(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let file = File::options().read(true).write(true).open(path)?;
        let blocks = file.metadata()?.len() / BLOCK_SIZE as u64;
        Ok(Self {
            file: std::cell::RefCell::new(file),
            blocks,
            calls: std::cell::Cell::new(0),
            blocks_moved: std::cell::Cell::new(0),
        })
    }

    /// How many calls the device has been given.
    pub fn calls(&self) -> usize {
        self.calls.get()
    }

    /// How many blocks those calls covered.
    pub fn blocks_moved(&self) -> usize {
        self.blocks_moved.get()
    }

    /// Blocks per call so far, which is the figure the bridge exists to
    /// keep above one.
    pub fn blocks_per_call(&self) -> usize {
        match self.calls.get() {
            0 => 0,
            calls => self.blocks_moved.get() / calls,
        }
    }

    /// Forgets the counts, so one operation can be measured without the
    /// mount that preceded it.
    pub fn reset(&self) {
        self.calls.set(0);
        self.blocks_moved.set(0);
    }

    fn note(&self, blocks: usize) {
        self.calls.set(self.calls.get() + 1);
        self.blocks_moved.set(self.blocks_moved.get() + blocks);
    }
}

#[cfg(feature = "embedded-sdmmc")]
impl embedded_sdmmc::BlockDevice for SdmmcImage {
    type Error = std::io::Error;

    fn read(
        &self,
        blocks: &mut [embedded_sdmmc::Block],
        start_block_idx: embedded_sdmmc::BlockIdx,
    ) -> Result<(), Self::Error> {
        self.note(blocks.len());
        let mut file = self.file.borrow_mut();
        file.seek(SeekFrom::Start(
            u64::from(start_block_idx.0) * BLOCK_SIZE as u64,
        ))?;
        for block in blocks {
            file.read_exact(&mut block.contents)?;
        }
        Ok(())
    }

    fn write(
        &self,
        blocks: &[embedded_sdmmc::Block],
        start_block_idx: embedded_sdmmc::BlockIdx,
    ) -> Result<(), Self::Error> {
        self.note(blocks.len());
        let mut file = self.file.borrow_mut();
        file.seek(SeekFrom::Start(
            u64::from(start_block_idx.0) * BLOCK_SIZE as u64,
        ))?;
        for block in blocks {
            file.write_all(&block.contents)?;
        }
        Ok(())
    }

    fn num_blocks(&self) -> Result<embedded_sdmmc::BlockCount, Self::Error> {
        Ok(embedded_sdmmc::BlockCount(self.blocks as u32))
    }
}

// ---------------------------------------------------------------------------
// The fixture manifest
// ---------------------------------------------------------------------------

/// One file's on-disk layout, as `scripts/fatmap.py` found it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileLayout {
    /// Path within the volume, in 8.3 form.
    pub path: String,
    /// Size from the directory entry.
    pub size: u64,
    /// `(first cluster, length)` per contiguous run.
    pub runs: Vec<(u32, u32)>,
}

impl FileLayout {
    /// The chain's first cluster — where a walk of it starts.
    pub fn first_cluster(&self) -> u32 {
        self.runs.first().expect("a file with no clusters").0
    }
}

/// What the manifest says about one image.
#[derive(Debug, Clone)]
pub struct ImageLayout {
    /// Bytes in a cluster.
    pub cluster_bytes: u32,
    /// Clusters the volume holds.
    pub cluster_count: u32,
    /// First cluster of the root directory.
    pub root_cluster: u32,
    /// Every file, sorted by path.
    pub files: Vec<FileLayout>,
}

impl ImageLayout {
    /// One file's layout by path, or a failure naming what is there.
    pub fn file(&self, path: &str) -> &FileLayout {
        self.files
            .iter()
            .find(|f| f.path == path)
            .unwrap_or_else(|| panic!("{path} is not in the manifest"))
    }
}

/// Loads the layout the fixture generator recorded for `image`.
///
/// The ground truth for every chain assertion, and deliberately produced
/// outside this crate: a test that worked out where the clusters should be
/// by calling the code under test would agree with that code's bugs.
pub fn layout(image: &str) -> ImageLayout {
    let path = fixtures_dir().join("manifest.json");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "could not read {}: {e}. Run `make fixtures`.",
            path.display()
        )
    });
    let json: serde_json::Value =
        serde_json::from_str(&text).expect("the fixture manifest is not valid JSON");

    let entry = &json["images"][image];
    assert!(
        !entry.is_null(),
        "{image} is not in the fixture manifest; run `make fixtures --force`"
    );
    let geometry = &entry["geometry"];

    let files = entry["files"]
        .as_array()
        .expect("manifest files is not an array")
        .iter()
        .map(|file| FileLayout {
            path: file["path"].as_str().expect("path").to_owned(),
            size: file["size"].as_u64().expect("size"),
            runs: file["runs"]
                .as_array()
                .expect("runs")
                .iter()
                .map(|run| {
                    let pair = run.as_array().expect("run is not a pair");
                    (
                        pair[0].as_u64().expect("run start") as u32,
                        pair[1].as_u64().expect("run length") as u32,
                    )
                })
                .collect(),
        })
        .collect();

    ImageLayout {
        cluster_bytes: geometry["cluster_bytes"].as_u64().expect("cluster_bytes") as u32,
        cluster_count: geometry["cluster_count"].as_u64().expect("cluster_count") as u32,
        root_cluster: geometry["root_cluster"].as_u64().expect("root_cluster") as u32,
        files,
    }
}

/// The content `scripts/content.py` generates, for `bytes` bytes.
///
/// Recomputed rather than stored: every 512-byte block spells out its own
/// index, so a comparison failure names the block that was actually read
/// instead of reporting an offset.
pub fn expected_content(bytes: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes);
    let mut index = 0u64;
    while out.len() < bytes {
        let stamp = format!("block {index:08} ");
        let mut block = Vec::with_capacity(BLOCK_SIZE);
        while block.len() < BLOCK_SIZE {
            block.extend_from_slice(stamp.as_bytes());
        }
        block.truncate(BLOCK_SIZE);
        let wanted = (bytes - out.len()).min(BLOCK_SIZE);
        out.extend_from_slice(&block[..wanted]);
        index += 1;
    }
    out
}

// ---------------------------------------------------------------------------
// External oracles
// ---------------------------------------------------------------------------

/// Runs a command, or explains why it could not.
///
/// A missing oracle fails rather than skipping. A suite that quietly stops
/// checking the on-disk result still reports success, which is worse than
/// one that stops.
fn run(program: &str, args: &[&str]) -> std::process::Output {
    Command::new(program)
        .args(args)
        .env("MTOOLS_SKIP_CHECK", "1")
        .output()
        .unwrap_or_else(|e| {
            panic!(
                "could not run `{program}`: {e}\n\
                 The host tests need dosfstools and mtools:\n  \
                 sudo apt-get install dosfstools mtools   (Debian/Ubuntu)\n  \
                 sudo pacman -S dosfstools mtools         (Arch)"
            )
        })
}

/// What `fsck.vfat` made of an image.
#[derive(Debug)]
pub struct FsckReport {
    /// True when `fsck` found nothing to complain about.
    pub clean: bool,
    /// Everything it printed, for a failure message worth reading.
    pub output: String,
}

/// Checks an image with `fsck.vfat -n`, changing nothing.
///
/// This is the correctness oracle for everything this crate writes. It is
/// an independent implementation, which is the entire point — a check
/// written against our own understanding of FAT would agree with our own
/// bugs.
pub fn fsck(image: impl AsRef<Path>) -> FsckReport {
    let path = image.as_ref().to_string_lossy().into_owned();
    let output = run("fsck.vfat", &["-n", &path]);
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    FsckReport {
        clean: output.status.success(),
        output: text,
    }
}

impl FsckReport {
    /// Whether `fsck` found damage that is worse than wasted space.
    ///
    /// After an interrupted write, a leak is the acceptable outcome and a
    /// cross-link is not — so "did `fsck` complain at all" is the wrong
    /// question. `fsck` reports reclaiming leaked clusters as a correction,
    /// which means a clean bill of health is unobtainable for *any* design
    /// that can leak, and demanding one would only test that nothing was
    /// ever interrupted.
    ///
    /// These are the phrases `dosfstools` prints for the failures that
    /// actually lose or share data. Everything else it might say —
    /// reclaiming unused clusters, a stale free-cluster summary, one copy of
    /// the table lagging the other, a file whose recorded length disagrees
    /// with its chain — describes space that is wasted or an operation that
    /// did not finish, not data belonging to two files at once.
    pub fn indicates_corruption(&self) -> Option<&'static str> {
        const CORRUPTION: [&str; 5] = [
            "share clusters",          // two files pointing at the same data
            "Bad start cluster",       // an entry pointing outside the volume
            "Contains a free cluster", // an entry pointing at unallocated space
            "Cluster chain loop",      // a chain that never ends
            "duplicate directory entry",
        ];
        CORRUPTION
            .into_iter()
            .find(|phrase| self.output.contains(phrase))
    }
}

/// Asserts an image is consistent, printing `fsck`'s own report if not.
pub fn assert_fsck_clean(image: impl AsRef<Path>) {
    let image = image.as_ref();
    let report = fsck(image);
    assert!(
        report.clean,
        "fsck.vfat rejected {}:\n{}",
        image.display(),
        report.output
    );
}

/// Lists a directory with `mdir`, returning its raw output.
///
/// Used as the naming oracle: `mdir` reports both the long name and the 8.3
/// alias, which is what a long-name implementation has to agree with.
pub fn mdir(image: impl AsRef<Path>, directory: &str) -> String {
    let path = image.as_ref().to_string_lossy().into_owned();
    let target = format!("::{directory}");
    let output = run("mdir", &["-i", &path, &target]);
    assert!(
        output.status.success(),
        "mdir failed on {directory}:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// One line of `mdir` output: the 8.3 name, and the long name if the entry
/// has one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MdirEntry {
    /// The 8.3 name, formatted `BASE.EXT`, with the case `mdir` shows.
    pub short: String,
    /// The long name, when `mdir` printed one.
    pub long: Option<String>,
}

/// Parses `mdir`'s listing of a directory into names.
///
/// The naming oracle: `mdir` prints both the long name and the 8.3 alias,
/// so it says what a correct long-name implementation has to agree with,
/// from a codebase that is not ours.
///
/// The format is fixed-width — eight characters of base, a space, three of
/// extension, then size, date and time, then the long name. The time is
/// located by the last `:` on the line, which is unambiguous because `:` is
/// not a legal character in a FAT name.
pub fn mdir_entries(image: impl AsRef<Path>, directory: &str) -> Vec<MdirEntry> {
    let listing = mdir(image, directory);
    let mut entries = Vec::new();

    for line in listing.lines() {
        // Entry lines are the ones long enough to hold an 8.3 field and a
        // timestamp; the header, the blank line and the totals are not.
        if line.len() < 22 || !line.is_char_boundary(12) {
            continue;
        }
        let Some(colon) = line.rfind(':') else {
            continue;
        };
        // "H:MM" — two digits follow the colon.
        if colon + 3 > line.len() {
            continue;
        }

        let base = line[0..8].trim_end();
        let extension = line[9..12].trim_end();
        if base.is_empty() || base.starts_with(' ') {
            continue;
        }

        let short = if extension.is_empty() {
            base.to_string()
        } else {
            format!("{base}.{extension}")
        };
        let tail = line[colon + 3..].trim();
        entries.push(MdirEntry {
            short,
            long: if tail.is_empty() {
                None
            } else {
                Some(tail.to_string())
            },
        });
    }
    entries
}

/// Copies a host file into an image with `mcopy`.
pub fn mcopy_in(image: impl AsRef<Path>, source: impl AsRef<Path>, target: &str) {
    let path = image.as_ref().to_string_lossy().into_owned();
    let source = source.as_ref().to_string_lossy().into_owned();
    let target = format!("::{target}");
    let output = run("mcopy", &["-m", "-i", &path, &source, &target]);
    assert!(
        output.status.success(),
        "mcopy into {target} failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Copies a file out of an image with `mcopy`.
pub fn mcopy_out(image: impl AsRef<Path>, source: &str, target: impl AsRef<Path>) {
    let path = image.as_ref().to_string_lossy().into_owned();
    let target = target.as_ref().to_string_lossy().into_owned();
    let source = format!("::{source}");
    let output = run("mcopy", &["-m", "-n", "-i", &path, &source, &target]);
    assert!(
        output.status.success(),
        "mcopy out of {source} failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Builds a fresh FAT32 image with `mkfs.vfat` and returns its path.
///
/// Deliberately not one of the shared fixtures: a test that mutates an
/// image needs one of its own, and a test that wants to prove the oracle
/// works needs a volume nothing in this repository has touched.
pub fn mkfs_image(name: &str, size_mb: u64, cluster_kb: u64) -> PathBuf {
    let path = scratch_dir().join(name);
    let _ = std::fs::remove_file(&path);

    let file = File::create(&path).expect("could not create a scratch image");
    file.set_len(size_mb * 1024 * 1024)
        .expect("could not size a scratch image");
    drop(file);

    let sectors = (cluster_kb * 2).to_string();
    let path_arg = path.to_string_lossy().into_owned();
    let output = run(
        "mkfs.vfat",
        &[
            "-F", "32", "-s", &sectors, "-i", "0BADCAFE", "-n", "SCRATCH", &path_arg,
        ],
    );
    assert!(
        output.status.success(),
        "mkfs.vfat failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    path
}
