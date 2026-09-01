# Releasing

How to cut a release of `resident-fat`. Maintainer-facing; nothing here is
needed to *use* the crate.

A publish to crates.io is permanent — a version can be yanked but never
replaced or deleted, and the version number can never be reused. Most of
what follows exists to make a mistake fail *before* that point.

## One-time setup

Only needed once per repository (or when a token expires).

- **The GitHub repository.** `Cargo.toml`'s `repository` and
  `documentation` fields, the README badges, and this file all name
  `joeferner/resident-fat`. Until that repository exists and is public,
  every one of those is a 404 for anyone reading the crates.io page.

- **crates.io API token.** Create one under Account Settings → API Tokens
  with the **publish-update** scope — plus **publish-new** for the very
  first release of the crate — then store it as a repository secret:

  ```sh
  gh secret set CARGO_REGISTRY_TOKEN
  ```

- **The `crates-io` environment.** `.github/workflows/release.yml` declares
  it. Create it under Settings → Environments and add yourself as a
  **required reviewer**: the tag push then parks the workflow at "waiting
  for approval" and gives one last look before the irreversible step.

- **A ruleset on `main`** requiring a pull request and the CI checks. The
  steps below assume one exists; without it, nothing stops a release going
  in unreviewed.

## Per-release steps

### 1. Decide the version

Semantic versioning, with the usual pre-1.0 caveat that `0.x` bumps the
*minor* for breaking changes. Things that are breaking here and are easy to
miss:

- **Adding a variant to a public enum that is not `#[non_exhaustive]`.**
  Settled at 0.1.0, and settled the same way for all of them: `Error`,
  `BootError`, `FatError`, `Format` and `Geometry` are every public enum
  this crate has, and all five are `#[non_exhaustive]`. So a new error kind
  is a *minor* bump, not a breaking one, and the cost is the wildcard arm
  downstream `match`es already have to carry. Adding a sixth public enum
  means making this choice again, and the default is `#[non_exhaustive]`
  unless the variants are fixed by the on-disk format rather than by this
  crate's judgement.
- **Adding a public field to a struct that is not `#[non_exhaustive]`**, or
  reordering its fields. The same unbreakable-once-published decision as the
  enums, and the one that catches people out, because *applying the
  attribute afterwards is itself the breaking change* — it forbids the
  struct literals and exhaustive patterns downstream already wrote. There is
  no adding it later for free, so the split was made at 0.1.0:

  - `BootSector`, `FsInfo` and `Partition` are `#[non_exhaustive]`. Each is
    handed *out* by a parser and never taken in, so nothing downstream can
    want to construct one, and each can gain a field for a minor bump.
    `BootSector` is the one this is really for: FAT12 and FAT16 are out of
    scope but not out of mind, and either brings fields it does not have.
  - `Run`, `DateTime` and `Packed` are left exhaustive on purpose. A caller
    does construct these — `DateTime` from its own clock, `Packed` to hand
    to `DateTime::unpack` — and their field sets are fixed by the FAT
    format rather than by this crate's judgement, so there is nothing to
    leave room for.
- **Adding a method to the `BlockDevice` trait**, even with a default
  body, if it changes what an implementor must provide. Every consumer
  implements that trait — it is the one part of the API that others write
  rather than call. Note that `type Error` is bounded on `Debug` alone, and
  tightening that bound is breaking — see the reasoning on `Error::Device`
  for why it stays there.
- **Bumping the `embedded-sdmmc` pin.** That crate's `BlockDevice` appears
  in this crate's API when the bridge feature is on, which makes it a
  public dependency: moving from 0.9 to 0.10 is a breaking change here
  even though nothing in this crate's own source changed. See the comment
  on the dependency in `Cargo.toml` for why the pin tracks what providers
  use rather than what is newest.
- **Raising `rust-version`.** An MSRV bump is at least a minor release.

Adding a feature, a public type, or a trait implementation is a minor bump.

**One thing semver does not cover, and this crate has to:** the bytes
written to the card. A change to allocation policy, write ordering, or
which optional directory-entry fields get populated is invisible to the
compiler and to every downstream build, and can still be the most
consequential thing in a release — the previous version's volumes are the
compatibility surface. Anything in that category gets a changelog entry
under **Changed** whether or not the Rust API moved, and is worth a minor
bump on its own.

### 2. Bump the version and update the changelog

On a branch — the `main` ruleset requires a pull request, so nothing goes
in directly:

```sh
git checkout -b release-<version>
```

- `Cargo.toml`: set `version`.
- `cargo check --all-features` — refreshes `Cargo.lock`, which is tracked
  and would otherwise be stale in the published tarball.
- `CHANGELOG.md`: give the changes a version heading —
  `## [<version>] - <YYYY-MM-DD>` — and add a link reference at the bottom
  pointing at `releases/tag/v<version>`. If an `## [Unreleased]` heading is
  sitting there, rename it; if there isn't one, write the version heading
  directly. Both are normal (see "The changelog needs no reopening" below).

The date matters: the release workflow **refuses to publish** while the
literal `ReleaseDate` placeholder is present, so a section left undated
fails the release rather than shipping a changelog that claims the version
was never released.

### 3. Open the PR and let CI run

```sh
gh pr create --fill
```

The ruleset requires the CI checks to pass. Merge with squash:

```sh
gh pr merge --squash --delete-branch
```

### 4. Verify locally, on a clean tree

```sh
git checkout main && git pull
make pre-commit      # fmt, clippy, tests, no_std builds, docs
make package         # what `cargo publish` will verify
```

`make package` refuses a dirty working tree, which is deliberate: what gets
published is the committed state, not what happens to be on disk.

`make pre-commit` needs the test oracles installed — `dosfstools` and
`mtools`, the same two CI installs. Without them the filesystem tests fail
rather than skipping, which is the intended behaviour: a suite that
silently stops checking the on-disk result is worse than one that stops.

### 5. Tag and push

```sh
git tag -a v<version> -m "resident-fat <version>"
git push origin v<version>
```

The tag **must** start with `v` — that is the workflow's trigger pattern,
and a bare `0.2.0` silently does nothing at all. It must also match
`Cargo.toml`'s version, which the workflow checks and fails on.

### 6. Approve and watch

```sh
gh run watch
```

The release job re-runs `make package`, then publishes. If you set up the
required reviewer, approve it in the Actions UI when it parks.

### 7. Verify the publish

```sh
open https://crates.io/crates/resident-fat
open https://docs.rs/crate/resident-fat/<version>/builds
```

The docs.rs build is the one thing CI cannot fully prove. It runs on
docs.rs's own nightly rather than the one `make doc` used, and it is the
only place `[package.metadata.docs.rs]` is actually consulted — locally
that section is inert and the Makefile passes the equivalent flags by
hand. A successful build shows "Available on crate feature
`embedded-sdmmc`" badges on the bridge's items, which is what confirms the
`--cfg docsrs` path and `all-features` both took effect.

### 8. Create the GitHub release

Not decoration: `CHANGELOG.md`'s version links point at
`/releases/tag/v<version>`, which only resolves once a release object
exists.

```sh
gh release create v<version> \
  --title "resident-fat <version>" \
  --notes-file <(awk -v v="## [<version>]" '
    index($0, v) == 1 { inside = 1; next }
    inside && /^## \[/ { exit }
    inside { print }
  ' CHANGELOG.md)
```

The range has to end at the *next* `## [` heading, which is why this is
`awk` and not the obvious `sed -n '/## \[<version>\]/,/^\[<version>\]:/p'`.
That closing address matches the link reference at the bottom of the file,
not anything near the section, so the range runs past every older heading
and the "notes" become the entire changelog. It fails silently — `gh`
accepts whatever it is handed — so the only symptom is an over-long
release page nobody rereads.

That's the release. Nothing further is required.

## The changelog needs no reopening

Keep a Changelog suggests holding an empty `## [Unreleased]` section open at
all times. Don't: with a protected `main`, creating it is a commit and a
pull request whose entire content is a heading with nothing under it.

Instead the section is created by **whichever change first needs it**, in
that change's own pull request — the PR that adds a module adds the heading
above its own bullet. The heading then exists exactly when there is
something to put under it, and step 2 renames it. If a release happens to
contain only changes that warranted no entry, there is no heading to rename
and step 2 writes the version heading directly.

0.1.0 carries no `### Added` list, and that is deliberate rather than an
omission: a changelog records what changed between two versions a reader
might have, and a first release has no earlier one to have changed from.
What the crate does belongs in the README and the API documentation, which
are where a reader looks for it and where it stays current. Entries proper
begin at 0.2.0.

The same reasoning applies to post-release version bumps, which is why
there is no `0.2.0-dev` step here either: `Cargo.toml` carries the last
released version between releases, and step 2 is where it moves.

## What the automation enforces, and how it fails

| Guard | Where | Symptom if it trips |
| --- | --- | --- |
| Tag matches `Cargo.toml` version | `release.yml` | Release job fails before publishing |
| Changelog is dated | `release.yml` | Same |
| Packaged tarball actually builds | `make package`, in both CI and the release job | Same |
| Library builds on the declared MSRV | `msrv` job in `ci.yml` | PR blocked. This is the only thing stopping `rust-version` from rotting into a comment |
| No accidental `std` dependency | `make no-std`, in CI | PR blocked. A host build would not catch it, because `std` is available there |
| PRs required on `main` | Repository ruleset | Direct pushes rejected |

One coupling to know about: the ruleset's required status checks are
matched against the **job names** in `ci.yml`. Renaming a job there leaves
the ruleset waiting on a name that never reports, and every PR blocks until
the ruleset is updated too. It fails closed, which is the safe direction,
but it is a puzzling half hour if you have forgotten why.

## If something goes wrong

- **The publish failed partway.** Nothing was uploaded unless the
  `Publish` step itself succeeded. Fix the cause and re-run the workflow
  from the Actions UI (`workflow_dispatch`) — no need to move the tag.
  Note that a dispatch run skips the tag-match check, since there is no tag
  in its context.
- **A bad version reached crates.io.** It cannot be replaced. Yank it
  (`cargo yank --version <version>`), which leaves existing lockfiles
  working but stops new dependents from selecting it, then release a fix
  under a new version number.
- **A release wrote bad data to real cards.** Yanking stops new dependents
  but does nothing for volumes already written, and a downstream consumer
  reading a corrupted card will not connect it to this crate on its own.
  Say so plainly in the changelog entry for the fix, and say which
  versions wrote it — that entry is the only notice anyone gets.
- **The tag is wrong but nothing is published.** Delete it locally and on
  the remote (`git tag -d v<version>`,
  `git push --delete origin v<version>`) and start again from step 5. Once
  a version *is* published, leave its tag alone.
