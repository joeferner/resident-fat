# Changelog

Notable changes to `resident-fat`, in the format of
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/). This crate
follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html), with
the usual pre-1.0 caveat that a `0.x` release bumps the *minor* for a
breaking change.

**One kind of change semver does not cover, and this crate has to:** the
bytes written to the card. A change to allocation policy, write ordering, or
which directory-entry fields get populated is invisible to the compiler and
to every downstream build, and can still be the most consequential thing in
a release — the previous version's volumes are the compatibility surface.
Anything in that category gets an entry here whether or not the Rust API
moved.

## [0.1.0] - 2026-09-01

First release. There is no earlier version to have changed from, so what the
crate does is left to the [README](README.md) and the [API
documentation](https://docs.rs/resident-fat) rather than restated as a list
of additions here. Entries proper begin at 0.2.0.

Read the version number as an early one. A FAT32 volume can be mounted,
walked, read, written, grown and truncated; long names and directories are
both read and created; and the on-disk result is checked against `fsck.vfat`
and `mtools`, which are independent implementations. What is missing is
native adapters for the block devices real hardware provides — the
`embedded-sdmmc` bridge covers those in the meantime — and the API will
change.

[0.1.0]: https://github.com/joeferner/resident-fat/releases/tag/v0.1.0
