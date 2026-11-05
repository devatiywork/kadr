//! Integer pixel rectangles shared by capture, overlay and recording code.

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Self { x, y, w, h }
    }

    pub fn from_points(a: (i32, i32), b: (i32, i32)) -> Self {
        let (x0, x1) = (a.0.min(b.0), a.0.max(b.0));
        let (y0, y1) = (a.1.min(b.1), a.1.max(b.1));
        Self::new(x0, y0, x1 - x0, y1 - y0)
    }

    pub fn right(&self) -> i32 {
        self.x + self.w
    }

    pub fn bottom(&self) -> i32 {
        self.y + self.h
    }

    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x && y >= self.y && x < self.right() && y < self.bottom()
    }

    pub fn center(&self) -> (i32, i32) {
        (self.x + self.w / 2, self.y + self.h / 2)
    }

    pub fn area(&self) -> i64 {
        self.w as i64 * self.h as i64
    }

    pub fn intersect(&self, o: &Rect) -> Option<Rect> {
        let x0 = self.x.max(o.x);
        let y0 = self.y.max(o.y);
        let x1 = self.right().min(o.right());
        let y1 = self.bottom().min(o.bottom());
        (x1 > x0 && y1 > y0).then(|| Rect::new(x0, y0, x1 - x0, y1 - y0))
    }

    pub fn translated(&self, dx: i32, dy: i32) -> Rect {
        Rect::new(self.x + dx, self.y + dy, self.w, self.h)
    }
}

/// Hardware H.264 encoders refuse very small frames and odd dimensions, so a
/// selected region is grown to a minimum size, kept inside its monitor and
/// rounded down to even width and height.
pub fn fit_for_video(r: Rect, bounds: Rect) -> Rect {
    const MIN_W: i32 = 160;
    const MIN_H: i32 = 90;
    let w = r.w.max(MIN_W).min(bounds.w);
    let h = r.h.max(MIN_H).min(bounds.h);
    let (cx, cy) = r.center();
    let w = w & !1;
    let h = h & !1;
    let x = (cx - w / 2).clamp(bounds.x, bounds.right() - w);
    let y = (cy - h / 2).clamp(bounds.y, bounds.bottom() - h);
    Rect::new(x, y, w, h)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_points_normalizes_any_drag_direction() {
        assert_eq!(Rect::from_points((30, 40), (10, 20)), Rect::new(10, 20, 20, 20));
    }

    #[test]
    fn intersect_returns_overlap_or_none() {
        let a = Rect::new(0, 0, 100, 100);
        assert_eq!(a.intersect(&Rect::new(50, 50, 100, 100)), Some(Rect::new(50, 50, 50, 50)));
        assert_eq!(a.intersect(&Rect::new(100, 0, 10, 10)), None);
    }

    #[test]
    fn video_region_becomes_even() {
        let m = Rect::new(0, 0, 2560, 1440);
        let r = fit_for_video(Rect::new(101, 201, 641, 361), m);
        assert_eq!((r.w % 2, r.h % 2), (0, 0));
        assert_eq!((r.w, r.h), (640, 360));
    }

    #[test]
    fn tiny_video_region_grows_and_stays_on_its_monitor() {
        let m = Rect::new(2560, 0, 2560, 1440);
        let r = fit_for_video(Rect::new(2561, 1, 10, 10), m);
        assert_eq!((r.w, r.h), (160, 90));
        assert_eq!((r.x, r.y), (2560, 0));
    }

    #[test]
    fn full_monitor_with_odd_size_stays_inside() {
        let m = Rect::new(0, 0, 1367, 769);
        let r = fit_for_video(m, m);
        assert_eq!((r.w, r.h), (1366, 768));
        assert!(r.x >= 0 && r.right() <= m.right() && r.bottom() <= m.bottom());
    }
}
