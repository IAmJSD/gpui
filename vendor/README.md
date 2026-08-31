# Vendored dependencies

These are local copies of support crates that Zed publishes to crates.io,
wired in through the `[patch.crates-io]` table at the bottom of the top-level
`Cargo.toml`. They exist for one reason: the published crates do not compile
for `wasm32-unknown-unknown`, because they unconditionally depend on crates
that need OS facilities the web does not have (`smol`'s IO reactor, `dirs`,
`tempfile`, `which`, `walkdir`, `async-fs`, `async-tar`).

| directory | crates.io package | version |
|---|---|---|
| `util/` | `gpui_util` | 0.2.2 |
| `http_client/` | `gpui_http_client` | 0.2.2 |

Each copy is the published source, byte-identical except for:

- moving the desktop-only dependencies to
  `[target.'cfg(not(target_arch = "wasm32"))'.dependencies]`, and
- `#[cfg(not(target_arch = "wasm32"))]` on the modules that use them
  (`util`: `archive`, `command`, `fs`, `shell`, `shell_env`, `test`, plus
  `home_dir()`'s `dirs` call; `http_client`: `github_download`), and
- a wasm fallback arm in `PathExt::try_from_bytes`, which previously only had
  `cfg(unix)` / `cfg(windows)` arms.

Native builds see the same API and behavior as the registry crates.

Note that `[patch.crates-io]` only takes effect in the top-level manifest of a
build. Building this repository directly (checks, tests, examples) uses these
copies; a downstream project that depends on `gpui` and wants to target wasm
must copy the same two `[patch.crates-io]` entries into its own workspace
manifest.

When re-vendoring a newer GPUI (see `UPSTREAM.md`), bump these to the matching
`gpui_util` / `gpui_http_client` versions and re-apply the cfg gates.
