//! Native pen orientation conversion. Screen X points right, screen Y down.
//! Kept independent of platform handles so all conversions run in unit tests.

pub(crate) fn degrees(value: [f32; 2]) -> Option<[f32; 2]> {
    value
        .iter()
        .all(|v| v.is_finite() && v.abs() <= 90.0)
        .then_some(value)
}

#[cfg(any(target_os = "macos", test))]
pub(crate) fn appkit(subtype: i16, value: [f32; 2]) -> Option<[f32; 2]> {
    // NSTabletPointEventSubtype. Non-tablet mouse events have no valid tilt.
    (subtype == 1)
        .then(|| degrees([value[0] * 90.0, -value[1] * 90.0]))
        .flatten()
}

#[cfg(any(target_os = "windows", test))]
pub(crate) fn windows(mask: u32, x: i32, y: i32) -> Option<[f32; 2]> {
    // Both PEN_MASK_TILT_X (4) and PEN_MASK_TILT_Y (8) must be present.
    (mask & 12 == 12)
        .then(|| degrees([x as f32, y as f32]))
        .flatten()
}

#[cfg(any(target_os = "windows", test))]
pub(crate) fn windows_is_pen_mouse(extra_info: u32) -> bool {
    extra_info & 0xffffff80 == 0xff515700
}

#[cfg(any(target_os = "windows", test))]
pub(crate) fn windows_mouse(extra_info: u32, sample: Option<[f32; 2]>) -> Option<[f32; 2]> {
    // MI_WP_SIGNATURE with the touch bit excluded. A normal mouse must never
    // inherit a cached pen sample, including immediately after a pen stroke.
    windows_is_pen_mouse(extra_info).then_some(sample).flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn degree_axes_preserve_direction_and_reject_invalid_samples() {
        assert_eq!(degrees([-45.0, 30.0]), Some([-45.0, 30.0]));
        assert_eq!(degrees([0.0, 0.0]), Some([0.0, 0.0]));
        assert_eq!(degrees([-90.0, 90.0]), Some([-90.0, 90.0]));
        for value in [f32::NAN, f32::INFINITY, -91.0, 91.0] {
            assert_eq!(degrees([value, 0.0]), None);
            assert_eq!(degrees([0.0, value]), None);
        }
    }
    #[test]
    fn appkit_scales_tablet_axes_and_flips_unflipped_view_y() {
        assert_eq!(appkit(1, [-0.5, 0.25]), Some([-45.0, -22.5]));
        assert_eq!(appkit(0, [0.5, 0.5]), None);
        assert_eq!(appkit(1, [1.1, 0.0]), None);
    }
    #[test]
    fn windows_requires_both_axes_and_filters_synthetic_touch_and_real_mouse() {
        let sample = windows(12, -45, 30);
        assert_eq!(sample, Some([-45.0, 30.0]));
        for mask in [0, 4, 8] {
            assert_eq!(windows(mask, 30, 30), None);
        }
        assert_eq!(windows(12, 91, 0), None);
        assert_eq!(windows_mouse(0xff515700, sample), sample);
        assert_eq!(windows_mouse(0xff515701, sample), sample);
        assert_eq!(windows_mouse(0xff515780, sample), None);
        assert_eq!(windows_mouse(0, sample), None);
        assert_eq!(windows_mouse(0xff515700, None), None);
    }
}
