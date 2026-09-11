use crate::{Bounds, DisplayId, Pixels, PlatformDisplay, point, px, size};
use anyhow::Result;
use objc::{class, msg_send, sel, sel_impl};
use uuid::Uuid;

use super::{NSRect, id};

/// The device's screen. iOS exposes a single display to applications (an
/// external display is a separate `UIScene`, not a second screen a window
/// can be placed on), so this is a unit type.
#[derive(Debug)]
pub(crate) struct IosDisplay;

impl IosDisplay {
    /// The scale of the main screen: 2 or 3 on every current device.
    pub(crate) fn scale_factor() -> f32 {
        unsafe {
            let screen: id = msg_send![class!(UIScreen), mainScreen];
            if screen.is_null() {
                return 2.0;
            }
            let scale: f64 = msg_send![screen, scale];
            scale as f32
        }
    }
}

impl PlatformDisplay for IosDisplay {
    fn id(&self) -> DisplayId {
        DisplayId(0)
    }

    fn uuid(&self) -> Result<Uuid> {
        // Stable across launches; there is no per-panel identifier to read.
        Ok(Uuid::from_bytes(*b"gpui-ios-screen0"))
    }

    fn bounds(&self) -> Bounds<Pixels> {
        unsafe {
            let screen: id = msg_send![class!(UIScreen), mainScreen];
            if screen.is_null() {
                return Bounds::new(point(px(0.), px(0.)), crate::DEFAULT_WINDOW_SIZE);
            }
            let frame: NSRect = msg_send![screen, bounds];
            Bounds::new(
                point(px(frame.origin.x as f32), px(frame.origin.y as f32)),
                size(px(frame.size.width as f32), px(frame.size.height as f32)),
            )
        }
    }
}
