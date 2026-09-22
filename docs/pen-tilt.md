# Native pen orientation

`MouseDownEvent`, `MouseMoveEvent` and `MouseUpEvent` carry optional `tilt`:
X/Y degrees from the perpendicular, positive right/down, matching Pointer
Events. `None` means there is no trustworthy orientation for this sample.
Mouse, synthesized touch, mobile and GPUI browser events use `None`; browser
hosts may continue reading the original DOM PointerEvent separately. The new
field requires downstream exhaustive event constructors to specify `tilt`.

- macOS reads `NSEvent.tilt` only for tablet-point events or mouse events with the tablet-point subtype.
  The view also receives standalone `tabletPoint:` updates. Multiply X
  by 90 and Y by -90 for GPUI's unflipped AppKit view. This follows the
  [Chromium AppKit conversion](https://chromium.googlesource.com/chromium/src/+/refs/tags/126.0.6443.1/content/common/input/web_input_event_builders_mac.mm)
  and [SDL Cocoa pen backend](https://github.com/libsdl-org/SDL/blob/main/src/video/cocoa/SDL_cocoapen.m).
  Apple's [tilt documentation](https://developer.apple.com/documentation/appkit/nsevent/tilt)
  defines the normalized range and valid event types; its Y-direction prose
  differs from those implementations' handling of unflipped AppKit views.
  Hardware direction testing remains required.
- Windows reads [POINTER_PEN_INFO](https://learn.microsoft.com/en-us/windows/win32/api/winuser/ns-winuser-pointer_pen_info)
  only when both [tilt mask bits](https://learn.microsoft.com/en-us/windows/win32/inputmsg/pen-mask-constants)
  are valid. Primary `WM_POINTERUPDATE` samples dispatch directly, including
  changes in orientation at a stationary point; their promoted mouse moves
  are suppressed. Existing promoted down/up events retain click handling.
  [Promoted input signatures](https://learn.microsoft.com/en-us/windows/win32/tablet/system-events-and-mouse-messages)
  distinguish real mice and touch from pen events so cached pen data cannot
  be applied to them.
- Wayland uses tablet-v2's advertised tilt capability and degree-valued
  [tilt events](https://gitlab.freedesktop.org/wayland/wayland-protocols/-/blob/main/stable/tablet/tablet-v2.xml).
  Tilt is cached per tool, delivered atomically at frame boundaries (including
  orientation-only changes), and cleared on proximity transitions.
- X11 uses named `Abs Tilt X`/`Abs Tilt Y` absolute valuators with resolution
  57 (rounded units per radian for degree-valued axes), as supplied by
  [xf86-input-wacom](https://github.com/linuxwacom/xf86-input-wacom/blob/master/src/wcmConfig.c)
  and its [unit definitions](https://github.com/linuxwacom/xf86-input-wacom/blob/master/src/xf86WacomDefs.h).
  Both axes are required; sparse updates are cached by source device and
  orientation-only events are dispatched. Unknown resolutions/relative axes
  are intentionally unavailable. Device range is not normalized to ±90°:
  doing so would distort tablets whose physical limit is ±60°.

Conversions are original code based on these APIs; no external source code
is copied. No physical tablets or native macOS/Windows runtime were available
for hardware verification. In particular, tablet mapping/rotation by drivers
and the AppKit Y direction should be checked on real hardware before claiming
platform certification. Legacy Windows Wintab-only drivers, X11 drivers with
unknown axis units, and iOS/Android tilt are not covered by this change.

`CARGO_TARGET_DIR=/path/to/build make test-tilt` runs handle-independent input
conversion tests. Downstream's normal Linux GPUI build compiles both Wayland
and X11 routing; GPUI's `tilt_tests` also exercise real sparse XInput samples.
