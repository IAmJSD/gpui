# gpui on the web

gpui compiles to `wasm32-unknown-unknown` and renders into an
`HtmlCanvasElement` through **WebGPU**. The browser's JS event loop is the
platform event loop: tasks run as microtasks and timeouts, and frames are
driven by `requestAnimationFrame`.

## Status

Smoke-tested in Chrome 151 and Firefox 148 (macOS, Apple M4): rendering,
text, mouse input, resize, dark-mode tracking, and a 3-minute soak at a
solid 60fps with no leaks or GPU errors. Pinch (the ctrl+wheel path),
typing, dead keys, IME composition, external clipboard paste and
multi-canvas windows have since had a Chrome 151 pass of their own, driven
through the DevTools protocol against `examples/input_web.rs` and
`examples/multi_window_web.rs`. The ctrl+wheel pinch path was confirmed
again with a physical trackpad pinch, which reaches the application and
leaves the page's own zoom alone. Safari 18.6 (macOS 15.6.1) passes too, once
WebGPU is switched on in its feature flags (see Requirements): clean boot,
an Apple adapter, the same rendering, and typing, IME composition and
ctrl+wheel all behaving as they do in Chrome. Its nonstandard
GestureEvents -- the one pinch path no other browser fires, and one nothing
can synthesize as trusted input -- were exercised with a physical trackpad
gesture.

Positioned popup windows, Option/AltGr characters and Tab capture have had
a Chrome 151 pass too. The Option characters and Tab were driven as real
macOS key events (Chrome in `--app` mode, so the omnibox cannot take the
focus): option-a/q/l insert exactly one "å"/"œ"/"¬" apiece, ctrl-option-a
stays a chord, and DOM focus stays on the hidden input across repeated
Tabs with typing still live. AltGr was exercised against a spoofed Windows
user agent, where ctrl+alt+2/e insert "²"/"€" and an untransformed
ctrl+alt+a does not. The one input path still without empirical backing is
a real OS input method (macOS Japanese/Chinese) typed physically; only
CDP-synthesized composition stands behind it.

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
  reaches gpui; the browser context menu is suppressed. A key gpui leaves
  unhandled is fed to the window's input handler as text, the same way the
  desktop backends do it, so typing lands in text fields. Option/AltGr
  characters (macOS option-a -> "å"; AltGr chords, which browsers report
  as ctrl+alt) follow the macOS convention: the keystroke's `key` is the
  physical key so bindings like alt-a still match, and the transformed
  character rides in `key_char` and is inserted when gpui leaves the key
  unbound. Apple platforms are the exception to the ctrl+alt half: option
  there transforms the character even with control held, but the mac
  backend only fills `key_char` when control is up, so ctrl-option-a stays
  a chord and types nothing. Tab always has its browser default
  suppressed -- it is a gpui navigation key, and the browser's response
  would move the DOM focus off the window's hidden input and take the
  keyboard with it.
- Dark mode: `prefers-color-scheme` is reflected in `window_appearance` and
  appearance-change callbacks.
- Cursor styles (CSS cursors) and `open_url` (new tab; subject to the
  popup blocker outside user gestures).
- **Pinch gestures**, both ways browsers report them: the ctrl+wheel
  events Chrome/Firefox/Edge synthesize for trackpad pinches (a real
  ctrl+wheel also zooms, which matches web convention -- the two are
  indistinguishable), and Safari's nonstandard GestureEvents. Sequences
  have no explicit end on the wheel path, so the gesture ends 150ms after
  its last event. GestureEvents report a cumulative `scale`, which is
  divided back into the per-event deltas gpui wants; a real Safari gesture
  reproduces its own final scale exactly. Such a gesture also reports
  rotation, which gpui has no event for, so a twist with no pinch in it
  still arrives as `Moved` events carrying a delta of 1.0.
- Clipboard, in both directions with caveats: gpui's
  `read_from_clipboard` is synchronous while the browser clipboard is
  async and permission-gated, so reads are served from a mirror. Writes
  update the mirror and (for text, best effort) the real clipboard. For
  external content, a paste keystroke (ctrl/cmd-v) is held back briefly
  so the browser's `paste` event -- the one place external clipboard text
  is synchronously readable -- can refresh the mirror first; if no paste
  event arrives within 100ms the keystroke is dispatched as-is. Pasting
  via a non-default keybinding therefore reads the mirror only.
- **IME composition**: an invisible `<input>` per window receives the
  composition events; compositionupdate marks text via the window's input
  handler and compositionend commits it. Key events are suppressed while
  composing -- including the keydown that *starts* a composition, which
  browsers flag with the keyCode 229 sentinel rather than `isComposing`.
  The element is parked at the caret on every composition event (asking the
  input handler where that is) so the candidate window appears in the right
  place; `update_ime_position` moves it too, for applications that call
  `Window::invalidate_character_coordinates`.
- Windows are canvases. A `WindowKind::Normal` window fills the viewport:
  the first claims `<canvas id="gpui">` if the page provides one (and it
  is unclaimed), the rest get their own full-viewport canvases appended
  to `<body>`, stacked in creation order. `PopUp` and `Floating` windows
  are instead positioned canvases at their requested bounds, above every
  normal window, and honor `resize()`; their mouse coordinates are
  window-relative, since `offsetX`/`offsetY` are measured from the canvas;
  closing any window removes its elements from the DOM. Device-pixel-ratio changes and canvas resizes are
  picked up every frame. Each window also owns the hidden `<input>` that
  its keyboard, composition and paste listeners hang off, so the window
  holding the DOM focus -- the newest one, until a click on another
  canvas moves it -- is the only one a keystroke reaches. Pointer events
  are suppressed at the browser level so that focus stays put; a canvas
  is not focusable, and the browser would otherwise hand the keyboard to
  the document body, which listens for nothing.

Not yet implemented:

- File drag-and-drop (gpui's file-drop events carry filesystem paths,
  which browser `File` objects do not have).
- `BackgroundExecutor::block` cannot work on the web for futures that are
  not already complete (there is no second thread to make progress while
  the caller waits); it panics with a message saying so. Blocking on an
  immediately-ready future still succeeds.
- The `test-support` feature does not build on wasm.

## Performance notes

An idle window costs no GPU work: gpui only draws when a window is dirty
and only re-presents briefly after input, so the per-frame
`requestAnimationFrame` tick otherwise just polls the canvas size. Hidden
tabs cost nothing at all -- browsers stop firing rAF for them. Continuous
submits (one per frame) happen exactly when something animates, which is
what the 60fps soak in the smoke tests measured.

Bundle size is dominated by embedded fonts (`hello_web`: 5.9MB of wasm, of
which ~4MB is code; `input_web` adds a 1.8MB CJK font). For production,
subset your fonts to the scripts you need, and run the release wasm
through `wasm-opt -Oz` from [binaryen](https://github.com/WebAssembly/binaryen)
after `wasm-bindgen`, which typically shaves another 10-20%.

## Requirements

- A browser with WebGPU (Chrome/Edge 113+, Firefox 141+, Safari 26+).
  Safari 18 has WebGPU only behind Develop > Feature Flags > WebGPU; with
  the flag off it does not define `navigator.gpu` at all, and gpui reports
  `failed to request a WebGPU adapter` to the console and opens no window.
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

Two more examples cover the input paths that need a browser to exercise.
`input_web` is a text editor with a keystroke log, the clipboard mirror's
contents and a pinch readout -- the place to try typing, dead keys, an IME,
ctrl/cmd-v and a trackpad pinch. It embeds M PLUS 1p as well as IBM Plex
Sans, since a browser has no system font to fall back to and everything an
input method commits would otherwise be tofu. `multi_window_web` opens two windows to
show that each gets its own canvas. Each has its own page next to
`index.html`:

```sh
for example in input_web multi_window_web; do
    cargo build --target wasm32-unknown-unknown --example "$example" --release
    wasm-bindgen --target web --out-dir "examples/web/pkg-$example" \
        "target/wasm32-unknown-unknown/release/examples/$example.wasm"
done
# open http://localhost:8000/input_web.html
```

Watching what the backend does is much easier with logging on: both
examples initialize `console_log`, and the browser console then shows every
keystroke, composition step, pinch and clipboard read.

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
