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

iOS shares the Metal renderer, the CoreText text system and the libdispatch
dispatcher with macOS by `#[path]`-including them from `platform/mac`, so a
change to those files needs both Apple targets checked. From a macOS host
with Xcode:

```sh
rustup target add aarch64-apple-ios-sim aarch64-apple-ios
cargo check --target aarch64-apple-ios-sim
cargo check --target aarch64-apple-ios
```

Android shares the blade renderer with Linux and the cosmic-text text
system with Linux and the web, so changes to either need the Android target
checked too. `cargo check` needs an NDK (the `android-activity` glue
compiles a C file with the NDK's clang), which `examples/android/run-emulator.sh`
finds under `ANDROID_HOME`; by hand:

```sh
rustup target add aarch64-linux-android
export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER=$NDK/toolchains/llvm/prebuilt/*/bin/aarch64-linux-android30-clang
export CC_aarch64_linux_android=$CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER
export AR_aarch64_linux_android=$NDK/toolchains/llvm/prebuilt/*/bin/llvm-ar
cargo check --target aarch64-linux-android
```

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
  Wayland (`zwp_pointer_gestures_v1`), X11 (XI 2.4 gesture events), Windows
  touchscreens (`WM_GESTURE`/`GID_ZOOM`) and Windows precision touchpads
  (Direct Manipulation). See `README.md` for the API and the support matrix.

  The touchpad path is opt-in, behind `GPUI_ENABLE_DIRECT_MANIPULATION`, and
  the opt-in is the point rather than an afterthought. `DM_POINTERHITTEST`
  arrives before Windows has classified the gesture, so claiming the contact
  to get a pinch claims two-finger pans as well and suppresses the
  `WM_MOUSEWHEEL` they would otherwise produce; `direct_manipulation.rs`
  consequently synthesises the scrolling too. Replacing the scroll path of a
  platform that cannot be run here, on the strength of a cross-compile, is
  not something to do by default -- so it is off until someone has driven it
  on real hardware, and flipping the default is a one-line change in
  `DirectManipulation::new` once they have.
- **Stylus pressure** on the three mouse events. Backends: macOS
  (`NSEvent.pressure`), X11 (XInput2 "Abs Pressure" valuator), Windows
  (`WM_POINTER` pen info, carried onto the synthesised legacy mouse
  messages; pen system gestures are disabled per window so pen-down is
  immediate) and Wayland (`zwp_tablet_v2`). Also in `README.md`.

  Wayland is the odd one out: tablet input there is a separate protocol
  rather than valuators on the pointer, and binding it stops the compositor
  emulating pointer events for the tool, so the whole mouse event stream is
  synthesised from `zwp_tablet_tool_v2` (see `README.md` for the mapping).
  Tool events are frame-batched -- the `pressure` for a tip-down arrives
  *after* the `down` -- so they are accumulated in `TabletFrame` and
  dispatched together on `frame`. Tablet pads are ignored, but their
  `zwp_tablet_pad_*` objects still need `Dispatch` impls: the seat announces
  pads unconditionally and a `new_id` with no registered child handler
  panics the queue.

- **A web platform backend** (`platform/web`, target
  `wasm32-unknown-unknown`), documented in `docs/web.md`. The JS event loop
  is the platform event loop: the dispatcher schedules runnables as
  microtasks and `setTimeout`s, a window is an `HtmlCanvasElement` driven by
  `requestAnimationFrame`, and rendering is a port of the blade renderer to
  wgpu/WebGPU (`platform/web/renderer.rs` -- WebGL2 was not an option, the
  shaders' storage-buffer instancing needs WebGPU). The WGSL is shared with
  blade except for explicit `@group`/`@binding` decorations, which blade
  injects but raw wgpu requires. `Platform::run` cannot block in a browser;
  it performs the async WebGPU setup, then calls the launch callback, and
  the app lives on in its registered callbacks. `Instant` is `web-time`'s
  re-export crate-wide (std's panics on wasm; on native it is the same
  type).

  Text runs on the same cosmic-text stack as Linux --
  `platform/linux/text_system.rs` moved to `platform/cosmic_text_system.rs`,
  shared by both -- with one wasm divergence: font-kit does not compile
  there, so the final style/weight/stretch candidate selection has a local
  CSS-matching approximation behind `cfg(target_arch = "wasm32")`. The
  browser has no system fonts; applications add fonts at startup
  (`hello_web` embeds IBM Plex Sans, which is what the default
  `.SystemUIFont` resolves to). DOM pointer/keyboard/wheel events are
  translated to `PlatformInput` (including pen pressure and manual
  multi-click counting), pinch gestures arrive via the ctrl+wheel events
  browsers synthesize (plus Safari's GestureEvents), dark mode tracks
  `prefers-color-scheme`, and cursor styles map to CSS cursors. The
  clipboard is a mirror (reads are synchronous in gpui; the browser's is
  async and permission-gated) with one enhancement: paste keystrokes are
  briefly held for the browser's `paste` event, the only place external
  clipboard text is synchronously readable, so pasting from other
  applications works. IME composition goes through an invisible focused
  `<input>` that receives the composition events and follows the caret.
  Popup and floating windows are positioned canvases at their requested
  bounds; normal windows fill the viewport. CI
  (`.github/workflows/ci.yml`) runs the full gate -- native check and
  tests, wasm32, the web examples, and the Windows cross-check.

  Enabling the target took some dependency surgery: `gpui_util` and
  `gpui_http_client` are vendored under `vendor/` with their desktop-only
  modules cfg'd off for wasm (see `vendor/README.md`); `smol` is a non-wasm
  dependency (the executor uses `futures-lite`'s prelude, which is what
  `smol::prelude` re-exports anyway); `uuid` gets randomness from the
  browser via getrandom's `wasm_js` backend (`.cargo/config.toml`). The
  `test-support` feature is not available on wasm, and
  `BackgroundExecutor::block` panics there (no second thread to make
  progress while parked).

  Smoke-tested in Chrome 151 and Firefox 148 on macOS/Apple M4:
  rendering, text, mouse input, resize, dark mode, and a 3-minute 60fps
  soak with zero console/GPU errors (the run surfaced and fixed three
  real bugs: an application-lifetime bug in the non-blocking `run`, a
  cross-stage bind-group derivation conflict wgpu-core rejects but Dawn
  accepts, and a cursor gated on focus instead of hover). Safari, and
  the additions that came after that pass -- pinch, external paste, IME
  composition -- have not run in a browser yet. macOS *build hosts* need
  llvm-ar for the wasm target; see docs/web.md.

- **An Android backend** (`platform/android`, targets
  `aarch64-linux-android` and `x86_64-linux-android`), documented in
  `docs/android.md`. The app is a `NativeActivity` reached through the
  `android-activity` glue crate: `Platform::run` is the `android_main`
  thread's loop, polling the activity's looper for lifecycle commands,
  input and executor wake-ups, and pacing frames from the display's
  refresh rate. The renderer is blade on Vulkan (the Linux one), with a
  small change so the sprite atlas outlives the renderer: Android takes
  the surface away whenever the activity leaves the foreground, and the
  renderer goes with it, while the atlas (glyphs, images) is kept for the
  next surface. Text is the cosmic-text stack, loading `/system/fonts`
  itself (fontdb knows no Android directories) with Roboto as the system
  face and a local font-matching step instead of font-kit, which would
  have brought FreeType along. Touch is synthesised into the mouse model
  as on iOS, only here every recognizer (slop, long press, two-finger pan,
  pinch, fling) is hand-rolled in `window.rs`. Keys come with a key code
  and a meta state; the characters come from the device's
  `KeyCharacterMap` through JNI (the glue crate's own lookup fails for
  the virtual keyboard's device id, which is what the software keyboard
  reports). Everything only Java can do goes through the `jni` crate:
  the clipboard, the window insets (`WindowInsets.getInsets`, API 30+),
  `ACTION_VIEW` intents, the launch intent's data, the display's refresh
  rate, `finish()`, and an AES key in the Android keystore that encrypts
  the credentials written to the app's private storage. There is no way
  to receive an activity result or implement a Java listener without a
  compiled Java class, so file pickers, native menus and dialogs are not
  provided (`prompt` returns `None`, `show_context_menu` `false`), and
  the software keyboard drives text fields through key events rather than
  an `InputConnection`, which rules out IME composition.

  Building an APK without Gradle: `examples/android/run-emulator.sh`
  builds the `<example>_android` cdylib targets (rustc cannot mix the
  `bin` and `cdylib` crate types, so the Android examples are separate
  targets wrapping the same sources), links a manifest with `aapt2`, adds
  the library, `zipalign`s and signs with the debug keystore, and installs
  and launches it with `adb`, booting an emulator if no device is
  attached. `gpui::android_main!(main)` defines the `android_main` entry
  the activity calls and expands to nothing elsewhere.

  Exercised in the API 35 emulator on the SwiftShader Vulkan device
  (rendering, insets, scroll and fling, long press, pinch, keys, the
  software keyboard, clipboard, rotation, dark mode, backgrounding,
  Back, relaunch); see `docs/android.md` for the list and what a real
  device still has to confirm. Two things it turned up: the emulator's
  driver lists none of the extensions blade requires by name although
  they are core in its Vulkan 1.3, hence the vendored `blade-graphics`
  (`vendor/README.md`), and `adb shell input swipe` under ~100ms sends a
  down and an up with no move, which the touch synthesis now reads as a
  scroll rather than a tap.

- **Images on the Linux clipboards.** Both Linux backends wrote
  `item.text().unwrap_or_default()` and nothing else, so copying a
  `ClipboardItem::new_image` -- a region of a picture -- put an empty string
  on the system clipboard and every other application pasted nothing.
  (Pasting *into* gpui already read images, and copying inside one process
  worked off the cached item, which is what hid it.)

  X11 now offers every entry of the item at once: each image under its own
  MIME atom, the text as `UTF8_STRING`. `Clipboard::set_image` had the same
  bug in miniature -- it computed the format atom and then hardcoded
  `image/png` -- and is gone along with `set_text`, both replaced by
  `set_item`. Two smaller fixes fell out of serving a selection honestly:
  `TARGETS` advertises the two MIME spellings of `UTF8_STRING` but the
  content path only matched the exact atom, so a requestor that picked one
  of them was refused; and a property write that fails (there is no INCR on
  the write side, so a selection larger than one request cannot be sent)
  now answers with `None` instead of leaving the requestor to time out. A
  megabyte goes through in one request under BIG-REQUESTS, which covers
  what a copied image weighs.

  Wayland offers the image MIME types on the data source, and `send` /
  `send_primary` honour the mime type they are handed rather than always
  writing the text.

  The X11 path has unit tests (`platform::linux::x11::clipboard`) that go
  over the wire from a second connection -- a real second X client -- so
  they need a display; they no-op when `DISPLAY` is unset. Wayland is
  compile-reviewed as ever.

Of these backends only X11 could be exercised on real input during
development, and only for the mouse (pressure-less) path; Xvfb cannot
synthesise gestures or tablets. macOS, Wayland and Windows are
compile-reviewed, Windows via the cross-check below. Direct Manipulation is
the least exercised of the lot -- a cross-compile says nothing about whether
a COM callback sequence is right -- which is why it is the one thing here
that has to be asked for.
