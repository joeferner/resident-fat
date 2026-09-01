# Build/lint orchestration for resident-fat.
#
# Unlike the bare-metal crates this one is built beside, there is no
# `.cargo/config.toml` here and no target to pin: the library is ordinary
# portable Rust that happens to be `no_std`, and the host is where its
# tests run. The `no-std` target below is what keeps that "happens to be"
# honest -- see its comment.

.PHONY: fmt fmt-check clippy fixtures fixtures-force fixtures-clean test no-std doc package pre-commit clean

fmt:
	cargo fmt

fmt-check:
	cargo fmt -- --check

# Both feature configurations, because the `embedded-sdmmc` bridge is off by
# default and a plain lint therefore never compiles it.
clippy:
	cargo clippy --all-targets -- -D warnings
	cargo clippy --all-targets --all-features -- -D warnings

# The FAT32 images the test suite runs against, built by `mkfs.vfat` and
# populated by `mtools`. Deterministic: the same script produces
# byte-identical images on any machine at any time, so a rebuild is a no-op
# and a fixture can be trusted as a stable reference rather than merely
# regenerable. See `scripts/mkfixtures.sh` for what pins each source of
# variance.
#
# Cheap to skip and cheap to run: the script no-ops when the fixtures are
# newer than the scripts that generate them, and a full rebuild is about
# eight seconds. The images are sparse -- nominally 3.5 GB, about 28 MB on
# disk -- and are not tracked, so a fresh clone builds them once.
fixtures:
	./scripts/mkfixtures.sh

fixtures-force:
	./scripts/mkfixtures.sh --force

fixtures-clean:
	rm -rf tests/fixtures

# Tests run on the host against those images, with `fsck.vfat` and `mtools`
# as the oracles: independent implementations, so a check written against
# our own understanding of FAT cannot agree with our own bugs. A missing
# oracle has to fail the test rather than silently skip it, or the suite
# reports success for checks it never ran.
test: fixtures
	cargo test
	cargo test --all-features

# A host `cargo build` proves nothing about `no_std`: `std` is available, so
# an accidental dependency on it compiles clean and is only discovered by a
# consumer. These two targets have no `std` at all. One is a generic
# microcontroller target and one is what the first real consumers actually
# build for; the pair is what backs the claim that the crate is portable
# rather than quietly shaped around a single platform.
#
# `alloc` is required, not optional, and rustup ships a precompiled `core`
# and `alloc` for both targets -- so this needs no `-Zbuild-std` and no
# nightly.
no-std:
	cargo build --release --all-features --target thumbv7em-none-eabi
	cargo build --release --all-features --target aarch64-unknown-none-softfloat

# `-D warnings` is the point: a plain doc build almost never fails, so
# without it this catches nothing. Broken intra-doc links are the main
# reason to build documentation here at all.
#
# `--cfg docsrs` mirrors what docs.rs passes (see `[package.metadata.docs.rs]`
# in Cargo.toml), so the nightly-only `doc_cfg` path lib.rs gates behind that
# cfg is exercised here rather than first failing on the docs.rs builder
# after a release is already published. That gate is why this line needs a
# nightly toolchain even though nothing else in the crate does; `cargo doc`
# without it works on stable and simply renders no feature annotations.
#
# Built twice, because the feature set changes which links resolve. A doc
# link to a feature-gated item is fine with `--all-features` and a warning
# without it -- and it is the second build a consumer gets, since neither
# feature is on by default. Only the first needs nightly.
doc:
	RUSTDOCFLAGS="-D warnings --cfg docsrs" cargo +nightly doc --no-deps --all-features
	RUSTDOCFLAGS="-D warnings" cargo doc --no-deps

# `cargo package` finishes by building the packaged tarball, which catches a
# class of "works here, broken on crates.io" problems that nothing else
# here does: a file `exclude` drops but the build needs, or a path that only
# resolves in this working copy.
#
# Kept out of `pre-commit`: it reaches the network to update the crates.io
# index, which a local commit check shouldn't need.
#
# One thing this does not check, so it belongs in whatever runs the release:
# `CHANGELOG.md`'s top heading carries a literal `ReleaseDate` placeholder
# (the `cargo-release` convention) that has to become the actual date before
# publishing. `grep ReleaseDate CHANGELOG.md` is the check, and
# `.github/workflows/release.yml` runs it.
package:
	cargo package

pre-commit: fmt clippy test no-std doc

clean:
	cargo clean