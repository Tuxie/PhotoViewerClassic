//! Window-control helpers (sizing, monitor geometry). Kept out of `main.rs`.
#![allow(dead_code)]

/// Largest (w, h) of ratio `aspect` (w/h) fitting in 0.8 × monitor, centered on that
/// monitor (top-left = mon_pos + offset). Pure; positions may be negative on
/// multi-monitor setups. Returns (w, h, x, y) in physical pixels.
pub fn fit_80_dims(aspect: f32, mon_w: u32, mon_h: u32, mon_x: i32, mon_y: i32) -> (u32, u32, i32, i32) {
    let box_w = mon_w as f32 * 0.8;
    let box_h = mon_h as f32 * 0.8;
    let (w, h) = if box_w / box_h > aspect {
        (box_h * aspect, box_h) // box is wider than the image → height-limited
    } else {
        (box_w, box_w / aspect) // → width-limited
    };
    let w = (w.round() as u32).max(1);
    let h = (h.round() as u32).max(1);
    let x = mon_x + (mon_w as i32 - w as i32) / 2;
    let y = mon_y + (mon_h as i32 - h as i32) / 2;
    (w, h, x, y)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn landscape_image_in_landscape_monitor_is_width_limited() {
        // 1000x1000 monitor, 0.8 box = 800x800. aspect 2.0 (wide) → width-limited: 800x400.
        let (w, h, x, y) = fit_80_dims(2.0, 1000, 1000, 0, 0);
        assert_eq!((w, h), (800, 400));
        assert_eq!((x, y), ((1000 - 800) / 2, (1000 - 400) / 2)); // (100, 300)
    }

    #[test]
    fn portrait_image_is_height_limited() {
        // box 800x800, aspect 0.5 (tall) → height-limited: 400x800.
        let (w, h, _, _) = fit_80_dims(0.5, 1000, 1000, 0, 0);
        assert_eq!((w, h), (400, 800));
    }

    #[test]
    fn centering_adds_monitor_offset_and_stays_signed() {
        // Monitor to the left of primary (negative x).
        let (w, _h, x, _y) = fit_80_dims(1.0, 1000, 1000, -1920, 0);
        assert_eq!(w, 800);
        assert_eq!(x, -1920 + (1000 - 800) / 2); // -1820
    }
}
