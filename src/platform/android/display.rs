use crate::{Bounds, DisplayId, Pixels, PlatformDisplay, point, px, size};
use anyhow::Result;
use uuid::Uuid;

/// The device's screen. An activity sees one display, so this is the size
/// the activity's window has (or, before it has one, the configuration's
/// screen size), in points.
#[derive(Debug)]
pub(crate) struct AndroidDisplay {
    bounds: Bounds<Pixels>,
}

impl AndroidDisplay {
    pub(crate) fn new(bounds: Bounds<Pixels>) -> Self {
        Self { bounds }
    }

    /// The screen size the activity's configuration reports, in dp.
    pub(crate) fn from_config(app: &android_activity::AndroidApp) -> Self {
        let config = app.config();
        let width = config.screen_width_dp().unwrap_or(0) as f32;
        let height = config.screen_height_dp().unwrap_or(0) as f32;
        let size = if width > 0.0 && height > 0.0 {
            size(px(width), px(height))
        } else {
            crate::DEFAULT_WINDOW_SIZE
        };
        Self::new(Bounds::new(point(px(0.), px(0.)), size))
    }
}

impl PlatformDisplay for AndroidDisplay {
    fn id(&self) -> DisplayId {
        DisplayId(0)
    }

    fn uuid(&self) -> Result<Uuid> {
        Ok(Uuid::from_bytes(*b"gpui-android-scr"))
    }

    fn bounds(&self) -> Bounds<Pixels> {
        self.bounds
    }
}
