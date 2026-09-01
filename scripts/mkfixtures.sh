#!/usr/bin/env bash
#
# Build the FAT32 test images the host test suite runs against, plus a
# manifest describing what ended up where.
#
# Everything here is deterministic: the same script produces byte-identical
# images on any machine, at any time. That is not incidental, it is the
# point -- a fixture that differs between runs turns a real regression and a
# clock into the same failure.
#
# The three things that would otherwise vary, and what pins each:
#
#   * The volume ID, which `mkfs.vfat` seeds from the current time by
#     default. `-i` sets it explicitly, and each image gets a distinct one
#     so a test can tell which fixture it was handed.
#   * File timestamps, which `mcopy` stamps as "now" by default -- so two
#     runs a minute apart produce different images. `-m` preserves the
#     source file's mtime instead, and the source tree's mtimes are set to
#     a fixed date below.
#   * File contents, which are generated from their own offsets rather than
#     from anything random. See `content.py`.
#   * The volume label's own directory entry, which `mkfs.vfat -n` writes
#     into the root with the time of day. It is the *only* thing in a
#     freshly built image that still varies once the two above are pinned --
#     four bytes, in entry zero of the root directory -- so
#     `stamp_volume_label` overwrites its timestamps afterwards. Dropping
#     `-n` would fix it too, but the label is worth keeping: it is an
#     `ATTR_VOLUME_ID` entry sitting first in the root, which any directory
#     reader has to skip, and a fixture that contains one is how that gets
#     tested.
#
# Images are sparse. The 32K fixture is nominally 2.1 GB and occupies a few
# hundred KB on disk.

set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(dirname "$here")"
out="$root/tests/fixtures"

# `mcopy` otherwise refuses images whose geometry it finds surprising, which
# includes a plain file with no partition table -- exactly what these are.
export MTOOLS_SKIP_CHECK=1

# Glob expansion feeds `mcopy` its argument order, which becomes the order
# entries land in a directory. A locale-dependent sort would make the images
# differ between machines.
export LC_ALL=C

# Any date works as long as it never changes. This one is arbitrary and
# deliberately not "now".
readonly FIXED_DATE='2026-01-02 03:04:05'

# A 1.6 MB file is the size the OTA case cares about, and the size the
# contiguous-read assertion is written against.
readonly BIG_BYTES=$((1600 * 1024))

# Enough long-named entries that a linear directory scan is visibly worse
# than a map lookup, which is the workload the ROM picker represents.
readonly ROM_COUNT=300

fail() { echo "mkfixtures: $*" >&2; exit 1; }

# Spelled out rather than written as `[ ... ] || [ ... ] && force=1`, which
# works here but only because of how `set -e` treats an `&&` list -- not
# something a reader should have to derive to know whether the script exits.
# Parsed after `fail` is defined, since bash resolves a function at call
# time: the other order breaks only on a bad argument, which is exactly when
# a clear message matters.
force=0
case "${1:-}" in
  --force | -f) force=1 ;;
  "") ;;
  *) fail "unknown argument: $1 (only --force is accepted)" ;;
esac

require_tools() {
  local missing=()
  for tool in mkfs.vfat fsck.vfat mcopy mdel minfo python3; do
    command -v "$tool" >/dev/null 2>&1 || missing+=("$tool")
  done
  if [ ${#missing[@]} -gt 0 ]; then
    fail "missing: ${missing[*]}
Install them with:  sudo apt-get install dosfstools mtools   (Debian/Ubuntu)
                    sudo pacman -S dosfstools mtools         (Arch)
The tests fail rather than skip when these are absent -- a suite that
silently stops checking the on-disk result is worse than one that stops."
  fi
}

# Everything the images are built from, assembled once and copied into each
# image with a single recursive `mcopy`. Building the tree once rather than
# issuing one `mcopy` per file is what keeps this quick: 300 files times
# four images is 1200 invocations otherwise.
build_source_tree() {
  local tree="$1"
  mkdir -p "$tree/ROMS" "$tree/EDGE"

  printf 'resident-fat fixture\n' > "$tree/HELLO.TXT"

  # Position-encoded rather than random: every 512-byte block spells out its
  # own index, so a read that lands at the wrong offset reports *which*
  # block it got instead of just "bytes differ".
  python3 "$here/content.py" "$BIG_BYTES" > "$tree/BIG.BIN"

  # Long names, in the volume of a real ROM directory. The index is padded
  # so the 8.3 aliases collide on their first six characters, which is what
  # exercises the `~1`/`~2` numeric-tail path on read.
  local i
  for ((i = 0; i < ROM_COUNT; i++)); do
    printf 'rom %03d\n' "$i" > "$(printf '%s/ROMS/Game Title %03d (USA, Europe).nes' "$tree" "$i")"
  done

  # One file per short-name behaviour worth having ground truth for. The
  # comments say what each is here to prove; `mdir` on the built image
  # confirms all of them.
  printf 'a\n' > "$tree/EDGE/UPPER.TXT"                 # pure 8.3: no LFN entries at all
  printf 'b\n' > "$tree/EDGE/lower.txt"                 # all-lowercase 8.3: lcase byte, still no LFN
  printf 'c\n' > "$tree/EDGE/MixedCase.Txt"             # mixed case: needs LFN despite fitting 8.3
  printf 'd\n' > "$tree/EDGE/twelve.chars"              # extension longer than three
  printf 'e\n' > "$tree/EDGE/name+with=odd,chars.bin"   # replace-class characters become _
  printf 'f\n' > "$tree/EDGE/Super Mario Bros. 3 (USA).nes"  # spaces and an interior dot
  printf 'g\n' > "$tree/EDGE/a-name-long-enough-to-need-three-lfn-slots-for-sure.dat"

  # One mtime for everything, so `mcopy -m` has a fixed value to preserve.
  find "$tree" -exec touch -d "$FIXED_DATE" {} +
}

# Rewrite the volume label entry's timestamps to `FIXED_DATE`. See the
# header for why this is needed and why the label is kept. The entry is
# always index zero of the root directory, and `fsck.vfat` is run afterwards
# either way, so a mistake here does not pass silently.
stamp_volume_label() {
  local image="$1" offset="${2:-0}"
  python3 - "$image" "$FIXED_DATE" "$offset" <<'EOF'
import struct, sys, datetime

path, fixed = sys.argv[1], sys.argv[2]
# Byte offset of the volume within the file, which is nonzero only for the
# partitioned fixture. Every sector number below is relative to it.
base = int(sys.argv[3])
when = datetime.datetime.strptime(fixed, "%Y-%m-%d %H:%M:%S")
date = ((when.year - 1980) << 9) | (when.month << 5) | when.day
time = (when.hour << 11) | (when.minute << 5) | (when.second // 2)

with open(path, "r+b") as image:
    image.seek(base)
    boot = image.read(512)
    bytes_per_sector = struct.unpack_from("<H", boot, 0x0B)[0]
    sectors_per_cluster = boot[0x0D]
    reserved = struct.unpack_from("<H", boot, 0x0E)[0]
    fats = boot[0x10]
    sectors_per_fat = struct.unpack_from("<I", boot, 0x24)[0]
    root_cluster = struct.unpack_from("<I", boot, 0x2C)[0]

    data_start = reserved + fats * sectors_per_fat
    root = base + (data_start + (root_cluster - 2) * sectors_per_cluster) * bytes_per_sector

    image.seek(root)
    entry = bytearray(image.read(32))
    if entry[0x0B] & 0x08 == 0:
        sys.exit(f"{path}: root entry 0 is not the volume label")

    entry[0x0D] = 0                                  # creation time, 10ms units
    struct.pack_into("<H", entry, 0x0E, time)        # creation time
    struct.pack_into("<H", entry, 0x10, date)        # creation date
    struct.pack_into("<H", entry, 0x12, date)        # last access date
    struct.pack_into("<H", entry, 0x16, time)        # last write time
    struct.pack_into("<H", entry, 0x18, date)        # last write date

    image.seek(root)
    image.write(entry)
EOF
}

# Runs `fsck.vfat`, showing its report only when it objects. It writes to
# stderr even on a clean pass -- the fragmented fixture's cluster-count
# warning, for one -- and printing that unannounced makes a successful build
# look broken.
verify() {
  local image="$1" name="$2" report
  report="$(fsck.vfat -n "$image" 2>&1)" \
    || fail "$name: fsck rejected a freshly built image:
$report"
}

# mkfs + populate + verify. Sizes are chosen so each volume clears the
# 65525-cluster floor below which a volume is not legally FAT32 -- under it
# `mkfs.vfat` warns, and the fixtures would stop representing real cards.
build_image() {
  local name="$1" cluster_kb="$2" size_mb="$3" volume_id="$4" tree="$5"
  local image="$out/$name"

  rm -f "$image"
  truncate -s "${size_mb}M" "$image"
  mkfs.vfat -F 32 -s $((cluster_kb * 2)) -i "$volume_id" \
    -n "RFAT${cluster_kb}K" "$image" >/dev/null
  stamp_volume_label "$image"

  # A sorted glob, not `"$tree"` itself: the order entries land in the
  # directory follows the order `mcopy` is given them, and an unsorted glob
  # would make that depend on the source filesystem's readdir order.
  mcopy -s -m -i "$image" "$tree"/* ::/
  verify "$image" "$name"
  echo "  $name  (${cluster_kb}K clusters, ${size_mb} MB)"
}

# A volume inside an MBR partition, which is what a card written by an
# imaging tool actually looks like.
#
# Every other fixture is a bare volume starting at block 0. That is the
# convenient shape and not the common one: a Raspberry Pi boot card, and any
# card a disk imager wrote, has a partition table in block 0 and the
# filesystem some way after it. Mounting one means knowing the offset and
# treating every block number as relative to it, which is a different code
# path from mounting at zero and was not otherwise exercised.
#
# 8192 is the offset `sfdisk` and most imaging tools choose -- 4 MiB in,
# which aligns the volume to any plausible erase block.
#
# `sfdisk` seeds the disk identifier from the clock, so `label-id` pins it
# for the same reason `mkfs.vfat -i` is pinned above.
readonly PARTITION_START=8192

# 512 MiB, which clears the 65525-cluster FAT32 floor with 4K clusters. A
# 256 MB partition lands just under it once the metadata is accounted for,
# and this fixture is meant to represent an ordinary card rather than to
# double as an edge case -- `fat32-frag.img` already covers an undersized
# volume.
readonly PARTITION_SECTORS=1048576

build_partitioned_image() {
  local name="$1" volume_id="$2" tree="$3"
  local image="$out/$name"
  local offset=$((PARTITION_START * 512))

  rm -f "$image"
  truncate -s $(((PARTITION_START + PARTITION_SECTORS) * 512)) "$image"
  printf 'label: dos\nlabel-id: 0x52464154\nstart=%d, size=%d, type=c, bootable\n' \
    "$PARTITION_START" "$PARTITION_SECTORS" | sfdisk "$image" >/dev/null 2>&1

  # The trailing count is in 1024-byte blocks and must match the partition
  # exactly, so it is half the sector count. Letting `mkfs.vfat` size the
  # volume from the file instead would leave the partition table and the
  # volume disagreeing about where the volume ends.
  mkfs.vfat -F 32 -s 8 -i "$volume_id" -n RFATPART \
    --offset "$PARTITION_START" "$image" $((PARTITION_SECTORS / 2)) >/dev/null
  stamp_volume_label "$image" "$offset"

  # `@@` gives mtools a byte offset into the file, which is how it addresses
  # a partition without being told about the table.
  mcopy -s -m -i "$image@@$offset" "$tree"/BIG.BIN "$tree"/HELLO.TXT ::/
  verify_partition "$image" "$name" "$offset"
  echo "  $name  (partition at block $PARTITION_START)"
}

# `fsck.vfat` has no offset option, so the partition is checked by copying it
# out to a bare image first. That copy is also what the test suite compares
# against: the same volume read through the offset and read on its own must
# give identical answers.
verify_partition() {
  local image="$1" name="$2" offset="$3" bare
  bare="$(mktemp)"
  dd if="$image" of="$bare" bs=512 skip="$PARTITION_START" \
    count="$PARTITION_SECTORS" status=none
  verify "$bare" "$name"
  rm -f "$bare"
}

# A deliberately fragmented file, which takes a recipe rather than a flag.
#
# The obvious approach -- write filler, delete every other file, write the
# real one into the holes -- does not work: mtools allocates at the
# high-water mark and walks straight past freed clusters. It only reuses
# them when there is nothing left at the end, so the volume has to be filled
# completely first.
#
# That is why this image is small, and why it is the one fixture that does
# not clear the 65525-cluster FAT32 floor: filling a spec-legal volume means
# writing at least 256 MB. `mkfs.vfat` warns about the cluster count and
# `fsck.vfat` accepts the result, which makes this fixture do double duty --
# it is also a volume marked FAT32 that is small enough for some tools to
# consider FAT16-sized, and the crate has to decide what to do with one.
build_fragmented_image() {
  local name="$1" tree="$2"
  local image="$out/$name"
  local pad="$tree/../pad.bin" frag="$tree/../frag.bin"

  rm -f "$image"
  truncate -s 16M "$image"
  mkfs.vfat -F 32 -s 8 -i FEEDFACE -n RFATFRAG "$image" 2>/dev/null >/dev/null
  stamp_volume_label "$image"

  head -c 4096 /dev/zero | tr '\0' 'p' > "$pad"
  python3 "$here/content.py" $((8 * 4096)) > "$frag"
  touch -d "$FIXED_DATE" "$pad" "$frag"

  # One `mcopy` per pad file is the obvious way to fill the volume and takes
  # six seconds for four thousand of them -- almost all of it process
  # startup, and worse on a CI runner than on a workstation. Copying them in
  # batches instead takes a third of a second.
  #
  # The pad files are hardlinks to a single 4 KB file, made in one Python
  # call: `mcopy` names each destination after its source, so they need
  # distinct names but not distinct contents, and four thousand `ln`
  # processes would reintroduce exactly the cost being removed.
  local pads="$tree/../pads"
  mkdir -p "$pads"
  python3 - "$pad" "$pads" <<'EOF'
import os, sys
source, into = sys.argv[1], sys.argv[2]
# Comfortably more than a 16 MB volume at 4 KB clusters can hold; the fill
# loop stops when the volume is full, not when it runs out of these.
for n in range(1, 4300):
    os.link(source, os.path.join(into, f"P{n}.PAD"))
EOF

  # Fill to the last cluster, asking the volume how much room is left rather
  # than inferring it: the root directory grows as entries are added, so the
  # number of pad files that fit is not the free-cluster count at the start.
  local next=1 free previous=-1 batch k
  local files=()
  while :; do
    free="$(minfo -i "$image" 2>/dev/null | sed -n 's/^free clusters=//p')"
    [ -n "$free" ] || fail "$name: could not read the free cluster count"
    [ "$free" -gt 0 ] || break
    [ "$free" != "$previous" ] || fail "$name: filling stalled with $free clusters free"
    previous="$free"

    batch=$((free < 500 ? free : 500))
    files=()
    for ((k = 0; k < batch; k++)); do
      files+=("$pads/P$((next + k)).PAD")
    done
    next=$((next + batch))
    # A batch that overruns the last free cluster fails partway, having
    # copied what fitted. That is the intended path, not an error -- the
    # loop re-reads the free count and finishes.
    mcopy -m -i "$image" "${files[@]}" ::/ 2>/dev/null || true
  done

  # Scattered holes, three apart, so the file that fills them lands in
  # single-cluster runs rather than a couple of long ones.
  for i in 2 5 8 11 14 17 20 23; do
    mdel -i "$image" "::/P$i.PAD"
  done

  mcopy -m -i "$image" "$frag" ::/FRAG.BIN
  verify "$image" "$name"

  # The whole point of this image. If mtools' allocator ever changes, this
  # is where it gets caught -- rather than as a baffling failure in a test
  # that expected several device calls and got one.
  python3 - "$image" "$here" <<'EOF' || fail "FRAG.BIN is not fragmented; the recipe above no longer works"
import sys
sys.path.insert(0, sys.argv[2])
from fatmap import Fat32
image = Fat32(sys.argv[1])
frag = [f for f in image.walk() if f["path"] == "/FRAG.BIN"]
sys.exit(0 if frag and len(frag[0]["runs"]) > 1 else 1)
EOF
  echo "  $name  (4K clusters, 16 MB, deliberately fragmented)"
}

# A FAT12 and a FAT16 volume, so "this crate refuses them by name" is a
# tested claim rather than an untested branch. Deliberately tiny: their only
# job is to be classified correctly and turned away, so nothing is written
# into them.
build_other_format() {
  local name="$1" bits="$2" size_mb="$3" volume_id="$4"
  local image="$out/$name"

  rm -f "$image"
  truncate -s "${size_mb}M" "$image"
  mkfs.vfat -F "$bits" -i "$volume_id" -n "RFAT$bits" "$image" 2>/dev/null >/dev/null
  verify "$image" "$name"
  echo "  $name  (FAT$bits, ${size_mb} MB)"
}

require_tools

if [ "$force" -eq 0 ] && [ -f "$out/manifest.json" ] \
   && [ "$out/manifest.json" -nt "${BASH_SOURCE[0]}" ] \
   && [ "$out/manifest.json" -nt "$here/content.py" ] \
   && [ "$out/manifest.json" -nt "$here/fatmap.py" ]; then
  echo "mkfixtures: fixtures are current (--force to rebuild)"
  exit 0
fi

mkdir -p "$out"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

echo "mkfixtures: building the source tree ($ROM_COUNT ROM names, $((BIG_BYTES / 1024)) KB BIG.BIN)"
build_source_tree "$work/tree"

echo "mkfixtures: building images"
build_image fat32-4k.img   4     272 11110001 "$work/tree"
build_image fat32-16k.img  16   1076 11110002 "$work/tree"
build_image fat32-32k.img  32   2152 11110003 "$work/tree"
build_fragmented_image fat32-frag.img "$work/tree"
build_partitioned_image fat32-mbr.img 11110004 "$work/tree"
build_other_format fat16.img 16 32 11110016
build_other_format fat12.img 12  2 11110012

# FAT32 images only: `fatmap.py` reads FAT32 and says so plainly rather than
# half-understanding the others, which is the same line this crate draws.
#
# Named rather than globbed, because not every FAT32 fixture has a volume at
# block 0 any more: `fat32-mbr.img` starts with a partition table, and a
# reader pointed at block 0 finds boot code where the geometry should be.
# The tests that use it check it against a volume mounted at the offset and
# against `expected_content`, neither of which needs a manifest entry.
echo "mkfixtures: writing the manifest"
python3 "$here/fatmap.py" \
  "$out/fat32-4k.img" "$out/fat32-16k.img" "$out/fat32-32k.img" \
  "$out/fat32-frag.img" > "$out/manifest.json"

echo "mkfixtures: done -> $out"
