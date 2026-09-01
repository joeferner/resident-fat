#!/usr/bin/env python3
"""Write deterministic file content to stdout: `content.py <bytes>`.

Every 512-byte block spells out its own index, repeated to fill the block.
That is chosen over random bytes on purpose. A read that lands at the wrong
offset then reports *which* block it actually got, so the failure names its
own cause -- "expected block 3127, got block 3128" is an off-by-one in run
arithmetic, while "bytes differ at offset 1600512" is the start of a
debugging session.

Being generated rather than stored also means the expected content is
computable by the test, so nothing has to ship a hash or a copy of the file.

Standard library only, so it needs no environment of its own.
"""

import sys

BLOCK = 512


def block(index):
    """One block: its index in ASCII, repeated and cut to length."""
    stamp = b"block %08d " % index
    return (stamp * (BLOCK // len(stamp) + 1))[:BLOCK]


def main(argv):
    if len(argv) != 2:
        print("usage: content.py <bytes>", file=sys.stderr)
        return 2
    total = int(argv[1])
    written = 0
    index = 0
    out = sys.stdout.buffer
    while written < total:
        chunk = block(index)[: total - written]
        out.write(chunk)
        written += len(chunk)
        index += 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))