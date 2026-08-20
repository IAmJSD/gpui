# Upstream

This repository is a fork of [GPUI](https://gpui.rs), the GPU-accelerated UI
framework developed by [Zed Industries](https://github.com/zed-industries/zed).
GPUI is Apache-2.0 licensed; see `LICENSE-APACHE`. All copyright in the
upstream code remains with its original authors.

## Provenance

The baseline commit (`Vendor gpui 0.2.2 verbatim from crates.io`) is the
published `gpui` 0.2.2 crate, unpacked exactly as `cargo` unpacks it and
committed without a single edit. Per the `.cargo_vcs_info.json` that ships in
that package, it was cut from `zed-industries/zed` at commit
`69e2130295c2649963eb639fc70b4f2ee8ea1624`, path `crates/gpui`.

Every commit after that baseline is this fork's own work, so
`git diff <baseline>..HEAD` is an exact statement of what has been changed.

## Why the crates.io sources rather than a GitHub fork of `zed`

GPUI lives inside the Zed monorepo and its in-tree `Cargo.toml` (preserved here
as `Cargo.toml.orig`) has ~80 `workspace = true` / `path = ...` entries plus
workspace-level `[patch.crates-io]` overrides. Cargo ignores a git dependency's
`[patch]` table, so consuming GPUI straight from a fork of the monorepo is both
fragile and a ~500 MB clone for every downstream user. The crates.io manifest
in place here is already normalised to registry dependencies, resolves
standalone, and is the exact configuration known to build. The tradeoff is that
this repository does not share history with `zed-industries/zed`, so upstream
merges are a re-vendor rather than a `git merge`.

## Re-vendoring a newer GPUI

1. `cargo download gpui==<version>` (or fetch the `.crate` from crates.io) and
   unpack it.
2. Copy the unpacked tree over a clean checkout, keeping `.gitignore`,
   `UPSTREAM.md` and this fork's `Cargo.toml` metadata
   (`publish = false`, the fork's `repository` URL, and the `[[test]]` entry
   for `tests/pinch.rs`).
3. Commit that as the new baseline.
4. Re-apply the fork's commits with `git cherry-pick`, or by hand from the diff
   against the previous baseline.
5. `cargo test --features test-support --test pinch` must pass.

## Changes in this fork

- **Pinch/magnify gesture support.** See `README.md` for the API and the
  per-platform support matrix.
