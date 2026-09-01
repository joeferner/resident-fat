#!/usr/bin/env python3
"""Report the on-disk layout of a FAT32 image: geometry, and the cluster
runs each file occupies.

This exists because neither oracle can answer the question the fixtures
need answered. `fsck.vfat` says whether a volume is *consistent*; `mtools`
says which files it *contains*. Neither says where a file's clusters
actually are — so without this, a fixture that claims to hold a
deliberately fragmented file would be claiming it on faith, and the
tests asserting how many device calls a read costs would have no ground
truth to check against.

The trade-off is worth naming: this is a second, partial FAT reader in the
repository, and a test oracle that shares a bug with the code under test
proves nothing. Two things keep that risk small. It is read-only and about
sixty lines — it walks a chain and reports it, with none of the allocation,
directory-writing or long-name machinery where the real bugs live. And it
is not the correctness oracle: `fsck.vfat` is, and it is an independent
implementation that this script never touches. This one only describes
layout, which `fsck` has no opinion about.

Standard library only, so it needs no environment of its own.
"""

import json
import os
import struct
import sys

DIR_ENTRY = 32
ATTR_LFN = 0x0F
ATTR_VOLUME_ID = 0x08
ATTR_DIRECTORY = 0x10
FREE = 0x00
DELETED = 0xE5
EOC = 0x0FFFFFF8


class Fat32:
    """Read-only view of a FAT32 image, opened lazily so a sparse
    multi-gigabyte fixture costs no memory."""

    def __init__(self, path):
        self.file = open(path, "rb")
        boot = self.file.read(512)
        if boot[510:512] != b"\x55\xaa":
            raise ValueError(f"{path}: no boot signature; not a FAT image")

        self.bytes_per_sector = struct.unpack_from("<H", boot, 0x0B)[0]
        self.sectors_per_cluster = boot[0x0D]
        reserved = struct.unpack_from("<H", boot, 0x0E)[0]
        self.num_fats = boot[0x10]
        self.sectors_per_fat = struct.unpack_from("<I", boot, 0x24)[0]
        self.root_cluster = struct.unpack_from("<I", boot, 0x2C)[0]
        # Total sectors lives in one of two fields: the 16-bit one, or the
        # 32-bit one when the count doesn't fit. A volume small enough to
        # use the 16-bit field leaves the 32-bit one zero -- which is not a
        # hypothetical, since the fragmentation fixture is deliberately
        # small enough to hit it.
        self.total_sectors = struct.unpack_from("<H", boot, 0x13)[0]
        if self.total_sectors == 0:
            self.total_sectors = struct.unpack_from("<I", boot, 0x20)[0]

        if self.sectors_per_fat == 0:
            raise ValueError(f"{path}: FAT size is zero; FAT12/16, not FAT32")
        if self.total_sectors == 0:
            raise ValueError(f"{path}: both total-sector fields are zero")

        self.fat_start = reserved
        self.data_start = reserved + self.num_fats * self.sectors_per_fat
        self.cluster_size = self.sectors_per_cluster * self.bytes_per_sector
        # Clamped by what the FAT can actually address, not just by what the
        # volume claims. The two disagree on corrupt or mis-formatted media,
        # and the table's capacity is the one that bounds a walk -- which is
        # what keeps `chain` terminating on a corrupt image, and the same
        # rule the crate itself applies.
        data_sectors = self.total_sectors - self.data_start
        addressable = self.sectors_per_fat * self.bytes_per_sector // 4 - 2
        self.cluster_count = min(data_sectors // self.sectors_per_cluster, addressable)

    def _read(self, offset, length):
        self.file.seek(offset)
        return self.file.read(length)

    def next_cluster(self, cluster):
        """The FAT entry for `cluster`, masked to its 28 significant bits."""
        offset = self.fat_start * self.bytes_per_sector + cluster * 4
        return struct.unpack("<I", self._read(offset, 4))[0] & 0x0FFFFFFF

    def cluster_offset(self, cluster):
        sector = self.data_start + (cluster - 2) * self.sectors_per_cluster
        return sector * self.bytes_per_sector

    def chain(self, start):
        """Every cluster in `start`'s chain, bounded so a cyclic or corrupt
        FAT reports an error rather than spinning."""
        clusters = []
        cluster = start
        while 2 <= cluster < EOC:
            clusters.append(cluster)
            if len(clusters) > self.cluster_count:
                raise ValueError(f"cluster chain from {start} exceeds the volume")
            cluster = self.next_cluster(cluster)
        return clusters

    def runs(self, start):
        """The chain as `[first_cluster, length]` runs. One run means the
        file is contiguous, which is what lets it be read in one transfer
        and is the property the read tests assert on."""
        runs = []
        for cluster in self.chain(start):
            if runs and runs[-1][0] + runs[-1][1] == cluster:
                runs[-1][1] += 1
            else:
                runs.append([cluster, 1])
        return runs

    def read_dir(self, start):
        """Short-name directory entries under `start`, long-name slots and
        deleted entries skipped. Names come back in 8.3 form: this reports
        layout, and `mdir` is what reports long names."""
        entries = []
        for cluster in self.chain(start):
            data = self._read(self.cluster_offset(cluster), self.cluster_size)
            for at in range(0, len(data), DIR_ENTRY):
                entry = data[at : at + DIR_ENTRY]
                if entry[0] == FREE:
                    return entries
                if entry[0] == DELETED:
                    continue
                attrs = entry[0x0B]
                if attrs == ATTR_LFN or attrs & ATTR_VOLUME_ID:
                    continue
                base = entry[0:8].decode("ascii", "replace").rstrip()
                ext = entry[8:11].decode("ascii", "replace").rstrip()
                first = struct.unpack_from("<H", entry, 0x14)[0] << 16
                first |= struct.unpack_from("<H", entry, 0x1A)[0]
                entries.append(
                    {
                        "name": f"{base}.{ext}" if ext else base,
                        "is_dir": bool(attrs & ATTR_DIRECTORY),
                        "size": struct.unpack_from("<I", entry, 0x1C)[0],
                        "first_cluster": first,
                    }
                )
        return entries

    def walk(self, start=None, path=""):
        """Every file and subdirectory in the volume, depth first, with its
        runs.

        Directories are reported as well as descended into: a directory has
        a cluster chain like any other file, and tests that assert on how
        many transfers reading one costs need to know how many runs it
        occupies. The root is not reported, having no entry of its own.
        """
        found = []
        for entry in self.read_dir(self.root_cluster if start is None else start):
            if entry["name"] in (".", ".."):
                continue
            full = f"{path}/{entry['name']}"
            runs = self.runs(entry["first_cluster"]) if entry["first_cluster"] else []
            found.append(
                {
                    "path": full,
                    "is_dir": entry["is_dir"],
                    "size": entry["size"],
                    "runs": runs,
                    "contiguous": len(runs) <= 1,
                }
            )
            if entry["is_dir"]:
                found.extend(self.walk(entry["first_cluster"], full))
        return found


def describe(path):
    """One image's geometry and file layout."""
    image = Fat32(path)
    return {
        "geometry": {
            "bytes_per_sector": image.bytes_per_sector,
            "sectors_per_cluster": image.sectors_per_cluster,
            "cluster_bytes": image.cluster_size,
            "fat_start_sector": image.fat_start,
            "sectors_per_fat": image.sectors_per_fat,
            "fat_count": image.num_fats,
            "data_start_sector": image.data_start,
            "root_cluster": image.root_cluster,
            "cluster_count": image.cluster_count,
            "total_sectors": image.total_sectors,
        },
        "files": sorted(image.walk(), key=lambda f: f["path"]),
    }


def main(argv):
    if len(argv) < 2:
        print("usage: fatmap.py <image> [image...]", file=sys.stderr)
        return 2
    # Always keyed by image name, even for one image, so consumers of the
    # manifest have one shape to parse rather than two.
    images = {os.path.basename(path): describe(path) for path in argv[1:]}
    print(json.dumps({"images": images}, indent=2))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))