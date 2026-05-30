pub const ZOOM_STEP: f32 = 1.25;
const MIN_SCALE: f32 = 0.05;
const MAX_SCALE: f32 = 32.0;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum ViewMode {
    Fit,
    OneToOne,
    Manual,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Geometry {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub smooth: bool,
}

pub struct ViewState {
    nat_w: f32,
    nat_h: f32,
    vp_w: f32,
    vp_h: f32,
    mode: ViewMode,
    manual_scale: f32,
    last_manual: Option<f32>,
    pan_x: f32,
    pan_y: f32,
}

impl ViewState {
    pub fn new() -> Self {
        ViewState {
            nat_w: 0.0,
            nat_h: 0.0,
            vp_w: 0.0,
            vp_h: 0.0,
            mode: ViewMode::Fit,
            manual_scale: 1.0,
            last_manual: None,
            pan_x: 0.0,
            pan_y: 0.0,
        }
    }

    /// Updates the viewport size. Pan is reset to (0,0); callers that want to
    /// preserve pan across resizes must save and restore it themselves.
    pub fn set_viewport(&mut self, w: f32, h: f32) {
        self.vp_w = w;
        self.vp_h = h;
        self.pan_x = 0.0;
        self.pan_y = 0.0;
    }

    /// Call when the displayed image *changes* (navigation): resets to Fit,
    /// clears pan and the remembered manual zoom.
    pub fn load(&mut self, nat_w: f32, nat_h: f32) {
        self.nat_w = nat_w;
        self.nat_h = nat_h;
        self.mode = ViewMode::Fit;
        self.pan_x = 0.0;
        self.pan_y = 0.0;
        self.last_manual = None;
    }

    /// Call when the natural dimensions change but the *same* image is shown
    /// (e.g. a 90° rotation): keeps the current mode and manual zoom, recenters pan.
    pub fn set_natural(&mut self, nat_w: f32, nat_h: f32) {
        self.nat_w = nat_w;
        self.nat_h = nat_h;
        self.pan_x = 0.0;
        self.pan_y = 0.0;
    }

    fn fit_scale(&self) -> f32 {
        if self.nat_w <= 0.0 || self.nat_h <= 0.0 || self.vp_w <= 0.0 || self.vp_h <= 0.0 {
            return 1.0;
        }
        (self.vp_w / self.nat_w).min(self.vp_h / self.nat_h)
    }

    pub fn scale(&self) -> f32 {
        match self.mode {
            ViewMode::Fit => self.fit_scale(),
            ViewMode::OneToOne => 1.0,
            ViewMode::Manual => self.manual_scale,
        }
    }

    pub fn mode(&self) -> ViewMode {
        self.mode
    }

    fn axis(vp: f32, len: f32, pan: f32) -> f32 {
        if len <= vp {
            (vp - len) / 2.0 // smaller than viewport → centered
        } else {
            (((vp - len) / 2.0) + pan).clamp(vp - len, 0.0) // larger → clamp, no edge gap
        }
    }

    pub fn geometry(&self) -> Geometry {
        let s = self.scale();
        let (w, h) = (self.nat_w * s, self.nat_h * s);
        Geometry {
            x: Self::axis(self.vp_w, w, self.pan_x),
            y: Self::axis(self.vp_h, h, self.pan_y),
            w,
            h,
            smooth: s <= 1.0,
        }
    }

    pub fn zoom(&mut self, factor: f32, ax: f32, ay: f32) {
        let old = self.scale();
        let new = (old * factor).clamp(MIN_SCALE, MAX_SCALE);
        let g = self.geometry();
        let (ix, iy) = ((ax - g.x) / old, (ay - g.y) / old); // image point under anchor
        self.mode = ViewMode::Manual;
        self.manual_scale = new;
        self.last_manual = Some(new);
        self.pan_x = (ax - ix * new) - (self.vp_w - self.nat_w * new) / 2.0; // top-left so anchor stays fixed
        self.pan_y = (ay - iy * new) - (self.vp_h - self.nat_h * new) / 2.0;
    }

    pub fn zoom_center(&mut self, factor: f32) {
        self.zoom(factor, self.vp_w / 2.0, self.vp_h / 2.0);
    }

    pub fn pan(&mut self, dx: f32, dy: f32) {
        self.pan_x += dx;
        self.pan_y += dy;
    }

    pub fn cycle_mode(&mut self) {
        self.mode = match self.mode {
            ViewMode::Fit => ViewMode::OneToOne,
            ViewMode::OneToOne => match self.last_manual {
                Some(s) => {
                    self.manual_scale = s;
                    ViewMode::Manual
                }
                None => ViewMode::Fit,
            },
            ViewMode::Manual => ViewMode::Fit,
        };
        self.pan_x = 0.0;
        self.pan_y = 0.0;
    }

    pub fn reset_fit(&mut self) {
        self.mode = ViewMode::Fit;
        self.pan_x = 0.0;
        self.pan_y = 0.0;
    }

    pub fn zoom_percent(&self) -> u32 {
        // scale() is always within [MIN_SCALE, MAX_SCALE] = [0.05, 32.0], so the
        // cast is non-negative and in range; max(0.0) guards future refactors.
        (self.scale() * 100.0).round().max(0.0) as u32
    }
}

impl Default for ViewState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Helper: f32 "close enough" comparison
    fn approx_eq(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-4
    }

    #[test]
    fn fit_scale_picks_limiting_dimension_portrait_in_landscape() {
        // Viewport 800x600 (landscape), image 200x400 (portrait)
        // w ratio: 800/200 = 4.0, h ratio: 600/400 = 1.5 → min = 1.5
        let mut vs = ViewState::new();
        vs.set_viewport(800.0, 600.0);
        vs.load(200.0, 400.0);
        assert!(approx_eq(vs.scale(), 1.5));
    }

    #[test]
    fn fit_scale_picks_limiting_dimension_landscape_in_portrait() {
        // Viewport 600x800 (portrait), image 400x200 (landscape)
        // w ratio: 600/400 = 1.5, h ratio: 800/200 = 4.0 → min = 1.5
        let mut vs = ViewState::new();
        vs.set_viewport(600.0, 800.0);
        vs.load(400.0, 200.0);
        assert!(approx_eq(vs.scale(), 1.5));
    }

    #[test]
    fn one_to_one_scale_is_1() {
        let mut vs = ViewState::new();
        vs.set_viewport(800.0, 600.0);
        vs.load(200.0, 300.0);
        vs.cycle_mode(); // Fit → OneToOne
        assert_eq!(vs.scale(), 1.0);
    }

    #[test]
    fn zoom_clamps_at_min_scale() {
        let mut vs = ViewState::new();
        vs.set_viewport(800.0, 600.0);
        vs.load(200.0, 200.0);
        // Zoom out many times to hit floor
        for _ in 0..50 {
            vs.zoom_center(0.5);
        }
        assert!(vs.scale() >= MIN_SCALE);
        assert!(approx_eq(vs.scale(), MIN_SCALE));
    }

    #[test]
    fn zoom_clamps_at_max_scale() {
        let mut vs = ViewState::new();
        vs.set_viewport(800.0, 600.0);
        vs.load(200.0, 200.0);
        // Zoom in many times to hit ceiling
        for _ in 0..50 {
            vs.zoom_center(2.0);
        }
        assert!(vs.scale() <= MAX_SCALE);
        assert!(approx_eq(vs.scale(), MAX_SCALE));
    }

    #[test]
    fn zoom_anchor_invariant() {
        // Choose dimensions carefully so image is larger than viewport at the test scale,
        // so axis() clamp doesn't interfere with the anchor invariant.
        // Viewport: 800x600, image: 100x100
        // Start at Fit scale: min(800/100, 600/100) = 6.0 → image 600x600, larger than vp height
        // Actually: 600x600 > 600 height, but 600x600 > 800 width? No: w=600 < vp_w=800, so x is centered.
        // Let's use viewport 400x400, image 100x100:
        //   fit_scale = min(400/100, 400/100) = 4.0 → image 400x400, exactly fits
        //   zoom by 2.0 → new_scale = 8.0, image 800x800 > viewport in both dims
        // Anchor at (200, 200) = center.
        // After zoom, image is 800x800; pan should be set so (200,200) still maps to same image point.
        // With image > vp in both dims, axis() uses the clamped formula; but with pan exactly set
        // by zoom(), the anchor invariant should hold (no edge clamping if anchor is far from edge).

        let mut vs = ViewState::new();
        vs.set_viewport(400.0, 400.0);
        vs.load(100.0, 100.0);
        // Scale is now 4.0 (fit), image is 400x400
        let old_scale = vs.scale();
        let ax = 200.0_f32;
        let ay = 200.0_f32;

        let g_before = vs.geometry();
        let ix_before = (ax - g_before.x) / old_scale;
        let iy_before = (ay - g_before.y) / old_scale;

        vs.zoom(2.0, ax, ay);

        let new_scale = vs.scale();
        let g_after = vs.geometry();
        let ix_after = (ax - g_after.x) / new_scale;
        let iy_after = (ay - g_after.y) / new_scale;

        assert!(
            approx_eq(ix_before, ix_after),
            "x anchor invariant failed: image_x before={ix_before} after={ix_after}"
        );
        assert!(
            approx_eq(iy_before, iy_after),
            "y anchor invariant failed: image_y before={iy_before} after={iy_after}"
        );
    }

    #[test]
    fn geometry_x_clamped_when_larger_than_viewport() {
        // Image larger than viewport → x must be in [vp_w - w, 0]
        let mut vs = ViewState::new();
        vs.set_viewport(400.0, 400.0);
        vs.load(100.0, 100.0);
        // Zoom so image is 800x800 (larger than 400x400 viewport)
        vs.zoom(8.0, 200.0, 200.0);
        let g = vs.geometry();
        assert!(
            g.x >= vs.vp_w - g.w && g.x <= 0.0,
            "x={} should be in [{}, 0]",
            g.x,
            vs.vp_w - g.w
        );
        assert!(
            g.y >= vs.vp_h - g.h && g.y <= 0.0,
            "y={} should be in [{}, 0]",
            g.y,
            vs.vp_h - g.h
        );
    }

    #[test]
    fn geometry_centered_when_smaller_than_viewport_pan_ignored() {
        // Image smaller than viewport → x = (vp_w - w) / 2, pan is irrelevant
        let mut vs = ViewState::new();
        vs.set_viewport(800.0, 600.0);
        vs.load(100.0, 100.0);
        // Fit scale = min(8.0, 6.0) = 6.0 → image 600x600 < vp_w=800, = vp_h=600 (not smaller for h)
        // Let's use a small image so it fits entirely
        vs.load(50.0, 40.0);
        // fit_scale = min(800/50, 600/40) = min(16, 15) = 15 → image 750x600
        // w=750 < vp_w=800 → centered: x = (800-750)/2 = 25
        // h=600 = vp_h=600 → not smaller (len <= vp uses <=), centered: y = (600-600)/2 = 0
        let g = vs.geometry();
        assert!(
            approx_eq(g.x, (vs.vp_w - g.w) / 2.0),
            "expected centered x={}, got {}",
            (vs.vp_w - g.w) / 2.0,
            g.x
        );

        // Adding pan should not change position for width-smaller case
        vs.pan(50.0, 0.0);
        let g2 = vs.geometry();
        assert!(
            approx_eq(g2.x, g.x),
            "pan should not affect centered axis: x={} g.x={}",
            g2.x,
            g.x
        );
    }

    #[test]
    fn cycle_mode_fit_to_onetoone_to_manual_to_fit_when_last_manual_some() {
        let mut vs = ViewState::new();
        vs.set_viewport(400.0, 300.0);
        vs.load(100.0, 100.0);

        // Establish a last_manual by zooming
        vs.zoom_center(2.0);
        let saved_scale = vs.scale();
        assert_eq!(vs.mode, ViewMode::Manual);

        // Reset to Fit
        vs.reset_fit();
        assert_eq!(vs.mode, ViewMode::Fit);
        assert!(vs.last_manual.is_some());

        // Fit → OneToOne
        vs.cycle_mode();
        assert_eq!(vs.mode, ViewMode::OneToOne);

        // OneToOne → Manual (restores last_manual)
        vs.cycle_mode();
        assert_eq!(vs.mode, ViewMode::Manual);
        assert!(approx_eq(vs.scale(), saved_scale));

        // Manual → Fit
        vs.cycle_mode();
        assert_eq!(vs.mode, ViewMode::Fit);
    }

    #[test]
    fn cycle_mode_skips_manual_when_last_manual_none() {
        let mut vs = ViewState::new();
        vs.set_viewport(400.0, 300.0);
        vs.load(100.0, 100.0);
        // No zoom yet: last_manual is None

        // Fit → OneToOne
        vs.cycle_mode();
        assert_eq!(vs.mode, ViewMode::OneToOne);

        // OneToOne → Fit (skips Manual because last_manual is None)
        vs.cycle_mode();
        assert_eq!(vs.mode, ViewMode::Fit);
    }

    #[test]
    fn zoom_percent_rounds_correctly() {
        let mut vs = ViewState::new();
        vs.set_viewport(100.0, 100.0);
        vs.load(100.0, 100.0);
        // Fit scale = 1.0 → 100%
        assert_eq!(vs.zoom_percent(), 100);

        // Manual scale 1.25 → 125%
        vs.zoom_center(1.25);
        assert_eq!(vs.zoom_percent(), 125);
    }

    #[test]
    fn zoom_step_constant_compounds() {
        // Five ZOOM_STEP zoom-ins compound multiplicatively from the fit scale.
        let mut vs = ViewState::new();
        vs.set_viewport(400.0, 400.0);
        vs.load(100.0, 100.0); // fit scale = 4.0
        let start = vs.scale();
        for _ in 0..5 {
            vs.zoom_center(ZOOM_STEP);
        }
        assert!(approx_eq(vs.scale(), start * ZOOM_STEP.powi(5)));
    }

    #[test]
    fn smooth_flag_set_when_scale_le_1() {
        let mut vs = ViewState::new();
        vs.set_viewport(400.0, 400.0);
        vs.load(800.0, 800.0); // image bigger than viewport → fit_scale < 1
        let g = vs.geometry();
        assert!(vs.scale() < 1.0);
        assert!(g.smooth);

        // Zoom in past 1.0
        vs.zoom_center(3.0); // scale becomes 0.5 * 3 = 1.5 > 1
        let g2 = vs.geometry();
        assert!(vs.scale() > 1.0);
        assert!(!g2.smooth);
    }

    #[test]
    fn load_resets_mode_pan_and_last_manual() {
        let mut vs = ViewState::new();
        vs.set_viewport(400.0, 400.0);
        vs.load(100.0, 100.0);
        vs.zoom_center(2.0);
        vs.pan(20.0, 30.0);

        vs.load(200.0, 200.0);
        assert_eq!(vs.mode, ViewMode::Fit);
        assert_eq!(vs.pan_x, 0.0);
        assert_eq!(vs.pan_y, 0.0);
        assert!(vs.last_manual.is_none());
    }

    #[test]
    fn set_natural_keeps_mode_and_recenters() {
        let mut vs = ViewState::new();
        vs.set_viewport(400.0, 400.0);
        vs.load(100.0, 100.0);
        vs.zoom_center(2.0); // Manual mode
        vs.pan(10.0, 10.0);

        let mode_before = vs.mode;
        let scale_before = vs.scale();

        vs.set_natural(200.0, 200.0);
        assert_eq!(vs.mode, mode_before);
        assert!(approx_eq(vs.scale(), scale_before));
        assert_eq!(vs.pan_x, 0.0);
        assert_eq!(vs.pan_y, 0.0);
    }
}
