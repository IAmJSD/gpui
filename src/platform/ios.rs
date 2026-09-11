//! iOS / iPadOS platform backend.
//!
//! UIKit owns the process: `Platform::run` hands control to
//! `UIApplicationMain` and never returns, exactly like `[NSApp run]` on
//! macOS. Each gpui window is a `UIWindow` holding a Metal-backed `UIView`;
//! frames are paced by a `CADisplayLink`. The Metal renderer, the CoreText
//! text system and the libdispatch executor are the macOS ones, included
//! from `platform/mac` by path so the two backends cannot drift apart.
//!
//! Touch input is synthesised into gpui's mouse model in `window.rs`: a tap
//! is a left click, a drag scrolls (with momentum), a long press is a right
//! click or, over a text field, the system edit menu, and a two-finger
//! pinch is a `PinchEvent`. iPad pointers arrive as real mouse events. App
//! menus are published through `UIMenuBuilder`, so they show in the iPadOS
//! menu bar and the hardware-keyboard shortcut overlay; context menus are
//! action sheets on iPhone and anchored popovers on iPad. See
//! `docs/ios.md`.

mod display;
mod events;
mod platform;
mod window;

// Shared with macOS. These files only use the frameworks that exist on
// every Apple platform (libdispatch, Metal, CoreText, CoreVideo).
#[path = "mac/dispatcher.rs"]
mod dispatcher;
#[path = "mac/metal_atlas.rs"]
mod metal_atlas;
#[path = "mac/metal_renderer.rs"]
pub mod metal_renderer;
#[cfg(feature = "font-kit")]
#[path = "mac/open_type.rs"]
mod open_type;
#[cfg(feature = "font-kit")]
#[path = "mac/text_system.rs"]
mod text_system;

use metal_renderer as renderer;

pub(crate) use dispatcher::*;
pub(crate) use display::*;
pub(crate) use platform::*;
#[cfg(feature = "font-kit")]
pub(crate) use text_system::*;
pub(crate) use window::*;

use objc::runtime::{BOOL, NO, Object, YES};
use objc::{class, msg_send, sel, sel_impl};
use std::{
    ffi::{CStr, c_char},
    ops::Range,
};

use crate::{Pixels, Point, Size, point, px, size};

/// Screen capture is not available on iOS.
pub(crate) type PlatformScreenCaptureFrame = ();

#[link(name = "UIKit", kind = "framework")]
unsafe extern "C" {}
#[link(name = "Foundation", kind = "framework")]
unsafe extern "C" {}
#[link(name = "QuartzCore", kind = "framework")]
unsafe extern "C" {}
#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {}
#[link(name = "CoreText", kind = "framework")]
unsafe extern "C" {}
#[link(name = "Metal", kind = "framework")]
unsafe extern "C" {}
#[link(name = "UniformTypeIdentifiers", kind = "framework")]
unsafe extern "C" {}

/// The handful of `cocoa` items the shared Metal renderer imports. The
/// `cocoa` crate is AppKit-only, so this stands in for it on iOS.
pub(crate) mod cocoa_shim {
    pub(crate) use super::NSSize;
    pub(crate) use objc::runtime::{NO, YES};
    pub(crate) type NSUInteger = u64;

    /// `CAAutoresizingMask`.
    #[repr(transparent)]
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct AutoresizingMask(pub u32);

    impl AutoresizingMask {
        pub const WIDTH_SIZABLE: Self = Self(1 << 1);
        pub const HEIGHT_SIZABLE: Self = Self(1 << 4);
    }

    impl std::ops::BitOr for AutoresizingMask {
        type Output = Self;
        fn bitor(self, rhs: Self) -> Self {
            Self(self.0 | rhs.0)
        }
    }

    unsafe impl objc::Encode for AutoresizingMask {
        fn encode() -> objc::Encoding {
            u32::encode()
        }
    }
}

/// An Objective-C object pointer.
#[allow(non_camel_case_types)]
pub(crate) type id = *mut Object;
#[allow(non_upper_case_globals)]
pub(crate) const nil: id = std::ptr::null_mut();

pub(crate) trait BoolExt {
    fn to_objc(self) -> BOOL;
}

impl BoolExt for bool {
    fn to_objc(self) -> BOOL {
        if self { YES } else { NO }
    }
}

pub(crate) trait NSStringExt {
    unsafe fn to_str(&self) -> &str;
}

impl NSStringExt for id {
    unsafe fn to_str(&self) -> &str {
        unsafe {
            if self.is_null() {
                return "";
            }
            let cstr: *const c_char = msg_send![*self, UTF8String];
            if cstr.is_null() {
                ""
            } else {
                CStr::from_ptr(cstr).to_str().unwrap_or("")
            }
        }
    }
}

/// Builds an autoreleased `NSString`.
pub(crate) unsafe fn ns_string(string: &str) -> id {
    unsafe {
        let bytes = string.as_bytes();
        let s: id = msg_send![class!(NSString), alloc];
        let s: id =
            msg_send![s, initWithBytes: bytes.as_ptr() length: bytes.len() encoding: 4usize];
        msg_send![s, autorelease]
    }
}

#[allow(non_upper_case_globals)]
pub(crate) const NSNotFound: usize = isize::MAX as usize;

#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub(crate) struct NSRange {
    pub location: usize,
    pub length: usize,
}

impl NSRange {
    pub(crate) fn is_valid(&self) -> bool {
        self.location != NSNotFound
    }

    pub(crate) fn to_range(self) -> Option<Range<usize>> {
        if self.is_valid() {
            Some(self.location..self.location + self.length)
        } else {
            None
        }
    }
}

impl From<Range<usize>> for NSRange {
    fn from(range: Range<usize>) -> Self {
        NSRange {
            location: range.start,
            length: range.len(),
        }
    }
}

unsafe impl objc::Encode for NSRange {
    fn encode() -> objc::Encoding {
        let encoding = format!(
            "{{NSRange={}{}}}",
            usize::encode().as_str(),
            usize::encode().as_str()
        );
        unsafe { objc::Encoding::from_str(&encoding) }
    }
}

/// `UIEdgeInsets`.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default)]
pub(crate) struct UIEdgeInsets {
    pub top: f64,
    pub left: f64,
    pub bottom: f64,
    pub right: f64,
}

unsafe impl objc::Encode for UIEdgeInsets {
    fn encode() -> objc::Encoding {
        let encoding = format!(
            "{{UIEdgeInsets={}{}{}{}}}",
            f64::encode().as_str(),
            f64::encode().as_str(),
            f64::encode().as_str(),
            f64::encode().as_str()
        );
        unsafe { objc::Encoding::from_str(&encoding) }
    }
}

/// `CGPoint`, as UIKit passes it.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub(crate) struct NSPoint {
    pub x: f64,
    pub y: f64,
}

impl NSPoint {
    pub(crate) fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }
}

unsafe impl objc::Encode for NSPoint {
    fn encode() -> objc::Encoding {
        let encoding = format!(
            "{{CGPoint={}{}}}",
            f64::encode().as_str(),
            f64::encode().as_str()
        );
        unsafe { objc::Encoding::from_str(&encoding) }
    }
}

/// `CGSize`.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub(crate) struct NSSize {
    pub width: f64,
    pub height: f64,
}

impl NSSize {
    pub(crate) fn new(width: f64, height: f64) -> Self {
        Self { width, height }
    }
}

unsafe impl objc::Encode for NSSize {
    fn encode() -> objc::Encoding {
        let encoding = format!(
            "{{CGSize={}{}}}",
            f64::encode().as_str(),
            f64::encode().as_str()
        );
        unsafe { objc::Encoding::from_str(&encoding) }
    }
}

/// `CGRect`.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub(crate) struct NSRect {
    pub origin: NSPoint,
    pub size: NSSize,
}

impl NSRect {
    pub(crate) fn new(origin: NSPoint, size: NSSize) -> Self {
        Self { origin, size }
    }
}

unsafe impl objc::Encode for NSRect {
    fn encode() -> objc::Encoding {
        let encoding = format!(
            "{{CGRect={}{}}}",
            NSPoint::encode().as_str(),
            NSSize::encode().as_str()
        );
        unsafe { objc::Encoding::from_str(&encoding) }
    }
}

impl From<NSSize> for Size<Pixels> {
    fn from(value: NSSize) -> Self {
        size(px(value.width as f32), px(value.height as f32))
    }
}

impl From<NSRect> for Size<Pixels> {
    fn from(rect: NSRect) -> Self {
        rect.size.into()
    }
}

impl From<NSPoint> for Point<Pixels> {
    fn from(value: NSPoint) -> Self {
        point(px(value.x as f32), px(value.y as f32))
    }
}

/// `UIUserInterfaceStyle` values.
pub(crate) const UI_USER_INTERFACE_STYLE_DARK: isize = 2;

/// Maps a `UIUserInterfaceStyle` to a gpui appearance.
pub(crate) fn appearance_from_style(style: isize) -> crate::WindowAppearance {
    if style == UI_USER_INTERFACE_STYLE_DARK {
        crate::WindowAppearance::Dark
    } else {
        crate::WindowAppearance::Light
    }
}

/// The application's key window's root view controller, used to present
/// system UI (pickers, alerts, sheets).
pub(crate) unsafe fn presenting_view_controller() -> Option<id> {
    unsafe {
        let app: id = msg_send![class!(UIApplication), sharedApplication];
        let scenes: id = msg_send![app, connectedScenes];
        let scenes: id = msg_send![scenes, allObjects];
        let scene_count: usize = msg_send![scenes, count];
        let mut fallback: Option<id> = None;
        for i in 0..scene_count {
            let scene: id = msg_send![scenes, objectAtIndex: i];
            let is_window_scene: BOOL = msg_send![scene, isKindOfClass: class!(UIWindowScene)];
            if is_window_scene == NO {
                continue;
            }
            let windows: id = msg_send![scene, windows];
            let window_count: usize = msg_send![windows, count];
            for j in 0..window_count {
                let window: id = msg_send![windows, objectAtIndex: j];
                let controller: id = msg_send![window, rootViewController];
                if controller.is_null() {
                    continue;
                }
                let is_key: BOOL = msg_send![window, isKeyWindow];
                if is_key == YES {
                    return Some(topmost_presented(controller));
                }
                fallback.get_or_insert(controller);
            }
        }
        fallback.map(|controller| topmost_presented(controller))
    }
}

unsafe fn topmost_presented(mut controller: id) -> id {
    unsafe {
        loop {
            let presented: id = msg_send![controller, presentedViewController];
            if presented.is_null() {
                return controller;
            }
            controller = presented;
        }
    }
}
