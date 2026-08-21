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

## Checking the other platforms

Adding a field to a shared event struct breaks every literal that builds one,
on every platform -- and those literals are spread across `platform/mac`,
`platform/windows` and `platform/linux`, so a clean Linux build proves very
little. This bit us once already: the pressure field landed with `mac` only
half-updated and `windows` not at all.

Windows can be type-checked from Linux. `cargo check` never links, so the
only obstacle is a couple of dependencies with C build scripts, and those
only need to *succeed*:

```sh
rustup target add x86_64-pc-windows-gnu
cargo check --target x86_64-pc-windows-gnu
```

If a C build script fails, point `CC_x86_64_pc_windows_gnu` at a wrapper that
falls back to compiling an empty translation unit. The objects are garbage;
nothing links them.

macOS cannot be checked this way: `gpui_media` runs `bindgen` over the
CoreMedia headers, which needs the Apple SDK. Review it by hand -- and when
adding a field, grep for *every* literal of the struct rather than fixing the
one the compiler happens to name first, because it reports them one file at a
time.

## Changes in this fork

- **Pinch/magnify gesture support.** Backends: macOS (`NSEventTypeMagnify`),
  Wayland (`zwp_pointer_gestures_v1`), X11 (XI 2.4 gesture events) and
  Windows touchscreens (`WM_GESTURE`/`GID_ZOOM`). Windows precision
  touchpads deliver pinches as Ctrl+scroll instead and would need Direct
  Manipulation. See `README.md` for the API and the support matrix.
- **Stylus pressure** on the three mouse events. Backends: macOS
  (`NSEvent.pressure`), X11 (XInput2 "Abs Pressure" valuator) and Windows
  (`WM_POINTER` pen info, carried onto the synthesised legacy mouse
  messages; pen system gestures are disabled per window so pen-down is
  immediate). Wayland still reports 1.0 -- it needs `zwp_tablet_v2`. Also in
  `README.md`.

Of these backends only X11 could be exercised on real input during
development, and only for the mouse (pressure-less) path; Xvfb cannot
synthesise gestures or tablets. macOS, Wayland and Windows are
compile-reviewed, Windows via the cross-check below.
