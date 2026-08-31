# gpui on the web

gpui compiles to `wasm32-unknown-unknown` and renders into an
`HtmlCanvasElement` through **WebGPU**. The browser's JS event loop is the
platform event loop: tasks run as microtasks and timeouts, and frames are
driven by `requestAnimationFrame`.

## Status

Smoke-tested in Chrome 151 and Firefox 148 (macOS, Apple M4): rendering,
text, mouse input, resize, dark-mode tracking, and a 3-minute soak at a
solid 60fps with no leaks or GPU errors. Safari and the newest additions
below (pinch, external paste, IME) have not had a browser pass yet.

Working:

- The full gpui programming model: entities, layout (taffy), styling,
  animations, `Application::run` / `open_window`.
- Rendering of quads, borders, shadows, underlines, paths, sprites and text
  via WebGPU (a port of the blade renderer; see
  `src/platform/web/renderer.rs`).
- **Text**, through the same cosmic-text stack as Linux. The browser
  exposes no system fonts, so the font database starts empty: add fonts
  with `cx.text_system().add_fonts(...)` at startup. gpui's default
  `.SystemUIFont` resolves to "IBM Plex Sans" on this backend; the
  `hello_web` example embeds it (`examples/fonts/`, SIL OFL 1.1).
- **Input**: pointer events (with pen pressure and manual multi-click
  counting), the scroll wheel (pixel and line deltas), keyboard events
  (modifier tracking, capslock, browser default suppressed exactly when
  gpui marks an event handled), hover and focus tracking. Right-click
  reaches gpui; the browser context menu is suppressed.
- Dark mode: `prefers-color-scheme` is reflected in `window_appearance` and
  appearance-change callbacks.
- Cursor styles (CSS cursors) and `open_url` (new tab; subject to the
  popup blocker outside user gestures).
- **Pinch gestures**, both ways browsers report them: the ctrl+wheel
  events Chrome/Firefox/Edge synthesize for trackpad pinches (a real
  ctrl+wheel also zooms, which matches web convention -- the two are
  indistinguishable), and Safari's nonstandard GestureEvents. Sequences
  have no explicit end on the wheel path, so the gesture ends 150ms after
  its last event.
- Clipboard, in both directions with caveats: gpui's
  `read_from_clipboard` is synchronous while the browser clipboard is
  async and permission-gated, so reads are served from a mirror. Writes
  update the mirror and (for text, best effort) the real clipboard. For
  external content, a paste keystroke (ctrl/cmd-v) is held back briefly
  so the browser's `paste` event -- the one place external clipboard text
  is synchronously readable -- can refresh the mirror first; if no paste
  event arrives within 100ms the keystroke is dispatched as-is. Pasting
  via a non-default keybinding therefore reads the mirror only.
- **IME composition** (first cut, not yet browser-verified): an invisible
  focused `<input>` receives composition events; compositionupdate marks
  text via the window's input handler, compositionend commits it, and
  `update_ime_position` parks the element at the caret so the IME popup
  appears in the right place. Key events are suppressed while composing.
- Windows are canvases. The first window claims `<canvas id="gpui">` if
  the page provides one (and it is unclaimed); every window otherwise
  gets its own full-viewport canvas appended to `<body>`, stacked in
  creation order. Device-pixel-ratio changes and canvas resizes are
  picked up every frame.

Not yet implemented:

- File drag-and-drop (gpui's file-drop events carry filesystem paths,
  which browser `File` objects do not have).
- `BackgroundExecutor::block` cannot work on the web (there is no second
  thread to make progress while the caller waits) and will panic if reached.
- The `test-support` feature does not build on wasm.

## Requirements

- A browser with WebGPU (Chrome/Edge 113+, Safari 18+, Firefox 141+).
- `rustup target add wasm32-unknown-unknown`
- `cargo install wasm-bindgen-cli --version <version>` where `<version>`
  matches the `wasm-bindgen` entry in `Cargo.lock`.

## macOS build hosts

`cargo build --target wasm32-unknown-unknown` (not `check`, which never
archives) fails on macOS hosts: the `psm` crate ships a prebuilt
`wasm32.o` that Xcode's Mach-O-only `ar`/`ranlib` archive into a library
LLVM cannot read (`LLVM error: section too large`). Point the archiver at
LLVM's own tools:

```sh
brew install llvm
export AR_wasm32_unknown_unknown=/opt/homebrew/opt/llvm/bin/llvm-ar
export RANLIB_wasm32_unknown_unknown=/opt/homebrew/opt/llvm/bin/llvm-ranlib
```

Linux hosts are unaffected.

## Building the example

```sh
cargo build --target wasm32-unknown-unknown --example hello_web --release
wasm-bindgen --target web --out-dir examples/web/pkg \
    target/wasm32-unknown-unknown/release/examples/hello_web.wasm
```

Serve `examples/web/` from any static file server (WebGPU requires a secure
context: `localhost` or https):

```sh
python3 -m http.server -d examples/web 8000
# open http://localhost:8000
```

## Application structure on the web

`Platform::run` does not block. It performs the asynchronous WebGPU setup
(adapter and device requests return promises) and then invokes your
`Application::run` callback; after the callback returns, the application
lives on in the callbacks it registered (rendering, timers, spawned tasks).
Consequences:

- `open_window` must be called from (or after) the run callback, never
  before `run`.
- `main` returns almost immediately; that is normal.
- Use `cx.spawn` / executors for async work. Both executors run on the JS
  event loop; `BackgroundExecutor::spawn` still requires `Send` futures but
  they execute on the single browser thread.

## Downstream crates

`[patch.crates-io]` is only honored in the top-level manifest of a build, so
a project that depends on gpui and targets wasm needs to copy the two patch
entries (for `gpui_util` and `gpui_http_client`) from this repository's
`Cargo.toml` into its own workspace manifest, along with the
`getrandom_backend = "wasm_js"` rustflag from `.cargo/config.toml`.
