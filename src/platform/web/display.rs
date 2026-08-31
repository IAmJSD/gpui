use crate::{Bounds, DisplayId, Pixels, PlatformDisplay, Point, Size, px};
use anyhow::Result;
use uuid::Uuid;

/// The browser viewport, presented as a display.
///
/// There is exactly one, its bounds are the inner size of the browser window,
/// and it has a fixed UUID since the browser offers no stable hardware
/// identifier to persist.
#[derive(Debug)]
pub(crate) struct WebDisplay;

impl WebDisplay {
    const UUID: Uuid = Uuid::from_u128(0x67707569_7765_6221_b164_6973706c6179);
}

impl PlatformDisplay for WebDisplay {
    fn id(&self) -> DisplayId {
        DisplayId(0)
    }

    fn uuid(&self) -> Result<Uuid> {
        Ok(Self::UUID)
    }

    fn bounds(&self) -> Bounds<Pixels> {
        let window = web_sys::window().expect("no global `window`");
        let width = window.inner_width().ok().and_then(|v| v.as_f64());
        let height = window.inner_height().ok().and_then(|v| v.as_f64());
        Bounds {
            origin: Point::default(),
            size: Size {
                width: px(width.unwrap_or(1024.0) as f32),
                height: px(height.unwrap_or(768.0) as f32),
            },
        }
    }
}
