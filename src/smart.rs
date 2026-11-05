//! "Click an object, get a box around it": finds the on-screen element under a
//! point using only the pixels of the screenshot.
//!
//! Screenshots are mostly flat fills, so two simple cases cover nearly everything:
//! * the point lies on a flat surface (button, card, input): flood-fill its color;
//! * the point lies on detail (text, icon, picture): take the color around it as
//!   the background and collect all nearby pixels that differ from it.

use crate::geom::Rect;
use crate::image::Image;

const FLAT_TOLERANCE: i32 = 12;
/// A surface may drift this far from the color under the pointer (soft gradients)...
const SURFACE_DRIFT: i32 = 40;
/// ...as long as neighbouring pixels stay this close: an edge is a sudden jump.
const SURFACE_STEP: i32 = 5;
const CONTENT_TOLERANCE: i32 = 24;
/// Grid cell of the "detail" search. Letters of a line are joined across up to two
/// empty cells horizontally; vertically the cells must touch.
const CELL: i32 = 4;
/// How far from the pointer a detail still counts as pointed at.
const NEAR_RADIUS: i32 = 5;
/// A flat area with more pixels than this is a surface, never a part of a letter or icon.
const SMALL_FLAT_AREA: i64 = 4096;

#[inline]
fn close(a: [u8; 3], b: [u8; 3], tol: i32) -> bool {
    (0..3).all(|i| (a[i] as i32 - b[i] as i32).abs() <= tol)
}

struct Flood {
    bbox: Rect,
    count: i64,
    overflow: bool,
}

fn flood(img: &Image, b: Rect, x: i32, y: i32, limit: i64) -> Flood {
    let seed = img.px(x, y);
    let w = b.w as usize;
    let mut seen = vec![0u64; (w * b.h as usize).div_ceil(64)];
    let idx = |px: i32, py: i32| (py - b.y) as usize * w + (px - b.x) as usize;
    let mut stack = vec![(x, y)];
    let first = idx(x, y);
    seen[first / 64] |= 1 << (first % 64);
    let (mut x0, mut y0, mut x1, mut y1) = (x, y, x, y);
    let mut count = 0i64;
    while let Some((px, py)) = stack.pop() {
        count += 1;
        if count > limit {
            return Flood { bbox: b, count, overflow: true };
        }
        x0 = x0.min(px);
        x1 = x1.max(px);
        y0 = y0.min(py);
        y1 = y1.max(py);
        let here = img.px(px, py);
        for (nx, ny) in [(px + 1, py), (px - 1, py), (px, py + 1), (px, py - 1)] {
            if !b.contains(nx, ny) {
                continue;
            }
            let i = idx(nx, ny);
            if seen[i / 64] & (1 << (i % 64)) != 0 {
                continue;
            }
            let next = img.px(nx, ny);
            if close(next, here, SURFACE_STEP) && close(next, seed, SURFACE_DRIFT) {
                seen[i / 64] |= 1 << (i % 64);
                stack.push((nx, ny));
            }
        }
    }
    Flood { bbox: Rect::new(x0, y0, x1 - x0 + 1, y1 - y0 + 1), count, overflow: false }
}

fn touching_sides(r: Rect, b: Rect) -> u32 {
    (r.x <= b.x) as u32
        + (r.y <= b.y) as u32
        + (r.right() >= b.right()) as u32
        + (r.bottom() >= b.bottom()) as u32
}

/// A flat surface that looks like a separate element rather than a page background.
fn looks_like_surface(f: &Flood, b: Rect) -> bool {
    !f.overflow
        && f.bbox.w >= 16
        && f.bbox.h >= 12
        && touching_sides(f.bbox, b) < 2
        && f.count * 10 >= f.bbox.area() * 3
        && f.bbox.area() * 2 <= b.area()
}

/// The closest pixel around the point that clearly differs from the flat color
/// under it: the pointer between two letters still means the text.
fn nearby_detail(img: &Image, b: Rect, x: i32, y: i32) -> Option<(i32, i32)> {
    let seed = img.px(x, y);
    for radius in 1..=NEAR_RADIUS {
        for dy in -radius..=radius {
            for dx in -radius..=radius {
                if dx.abs() != radius && dy.abs() != radius {
                    continue;
                }
                let (px, py) = (x + dx, y + dy);
                if b.contains(px, py) && !close(img.px(px, py), seed, CONTENT_TOLERANCE) {
                    return Some((px, py));
                }
            }
        }
    }
    None
}

/// Most common color in a one-pixel ring just outside `r`, plus one ring pixel of it.
fn ring_background(img: &Image, b: Rect, r: Rect) -> Option<([u8; 3], (i32, i32))> {
    let ring = Rect::new(r.x - 3, r.y - 3, r.w + 6, r.h + 6);
    let mut buckets: Vec<([u8; 3], u32, (i32, i32))> = Vec::new();
    let mut add = |x: i32, y: i32| {
        if !b.contains(x, y) {
            return;
        }
        let c = img.px(x, y);
        if let Some(e) = buckets.iter_mut().find(|e| close(e.0, c, FLAT_TOLERANCE)) {
            e.1 += 1;
        } else if buckets.len() < 64 {
            buckets.push((c, 1, (x, y)));
        }
    };
    for x in ring.x..ring.right() {
        add(x, ring.y);
        add(x, ring.bottom() - 1);
    }
    for y in ring.y..ring.bottom() {
        add(ring.x, y);
        add(ring.right() - 1, y);
    }
    buckets.into_iter().max_by_key(|e| e.1).map(|e| (e.0, e.2))
}

/// Collects everything around the point that differs from the background.
fn group_detail(img: &Image, b: Rect, x: i32, y: i32, bg: [u8; 3]) -> Option<Rect> {
    let cols = (b.w + CELL - 1) / CELL;
    let rows = (b.h + CELL - 1) / CELL;
    let mut state = vec![0u8; (cols * rows) as usize]; // 0 unknown, 1 detail, 2 empty, 3 visited
    let content = |cx: i32, cy: i32, state: &mut [u8]| -> bool {
        let i = (cy * cols + cx) as usize;
        if state[i] == 0 {
            let cell = Rect::new(b.x + cx * CELL, b.y + cy * CELL, CELL, CELL);
            let cell = cell.intersect(&b).unwrap_or_default();
            let mut any = false;
            'scan: for py in cell.y..cell.bottom() {
                for px in cell.x..cell.right() {
                    if !close(img.px(px, py), bg, CONTENT_TOLERANCE) {
                        any = true;
                        break 'scan;
                    }
                }
            }
            state[i] = if any { 1 } else { 2 };
        }
        state[i] == 1
    };

    let start = ((x - b.x) / CELL, (y - b.y) / CELL);
    if !content(start.0, start.1, &mut state) {
        return None;
    }
    state[(start.1 * cols + start.0) as usize] = 3;
    let mut stack = vec![start];
    let (mut c0, mut r0, mut c1, mut r1) = (start.0, start.1, start.0, start.1);
    let limit = (cols * rows) as i64 / 2;
    let mut visited = 0i64;
    while let Some((cx, cy)) = stack.pop() {
        visited += 1;
        if visited > limit {
            return None;
        }
        c0 = c0.min(cx);
        c1 = c1.max(cx);
        r0 = r0.min(cy);
        r1 = r1.max(cy);
        for dy in -1..=1 {
            for dx in -3..=3 {
                let (nx, ny) = (cx + dx, cy + dy);
                if nx < 0 || ny < 0 || nx >= cols || ny >= rows {
                    continue;
                }
                if content(nx, ny, &mut state) {
                    state[(ny * cols + nx) as usize] = 3;
                    stack.push((nx, ny));
                }
            }
        }
    }

    // Tighten the cell box to the exact pixels.
    let outer = Rect::new(
        b.x + c0 * CELL,
        b.y + r0 * CELL,
        (c1 - c0 + 1) * CELL,
        (r1 - r0 + 1) * CELL,
    )
    .intersect(&b)?;
    let (mut x0, mut y0, mut x1, mut y1) = (i32::MAX, i32::MAX, i32::MIN, i32::MIN);
    for py in outer.y..outer.bottom() {
        for px in outer.x..outer.right() {
            if !close(img.px(px, py), bg, CONTENT_TOLERANCE) {
                x0 = x0.min(px);
                x1 = x1.max(px);
                y0 = y0.min(py);
                y1 = y1.max(py);
            }
        }
    }
    (x1 >= x0).then(|| Rect::new(x0, y0, x1 - x0 + 1, y1 - y0 + 1))
}

/// Finds the element made of the detail at the point: a text line, an icon, or the
/// button that tightly holds them.
fn detect_detail(img: &Image, b: Rect, x: i32, y: i32) -> Option<Rect> {
    let limit = b.area() * 3 / 5;
    let around = flood(img, b, x, y, limit);
    let (bg, ring_px) = ring_background(img, b, around.bbox)?;
    let parent = flood(img, b, ring_px.0, ring_px.1, limit);
    let inside_surface = looks_like_surface(&parent, b) && parent.bbox.contains(x, y);

    // Stay inside the surface, so neighbouring fields do not merge into one object.
    let area = if inside_surface { parent.bbox } else { b };
    let mut bg = bg;
    let mut group = group_detail(img, area, x, y, bg)?;
    // On a textured object the first guess of the background is the object's own color,
    // which cuts parts of it off. The color around the result is the better guess.
    for _ in 0..3 {
        let Some((around_group, _)) = ring_background(img, b, group) else {
            break;
        };
        if close(around_group, bg, FLAT_TOLERANCE) {
            break;
        }
        match group_detail(img, area, x, y, around_group) {
            Some(wider) if wider.intersect(&group) == Some(group) => {
                group = wider;
                bg = around_group;
            }
            _ => break,
        }
    }
    if group.w < 4 || group.h < 4 {
        return None;
    }
    // A label inside a button: prefer the button if it fits the label snugly.
    if inside_surface && parent.bbox.h <= group.h * 3 && parent.bbox.w <= group.w.max(24) * 3 {
        return Some(parent.bbox);
    }
    Some(group)
}

/// Returns the bounds of the element under (`x`, `y`), or `None` when the point is
/// on the page background. `bounds` limits the search (the current crop).
pub fn detect(img: &Image, bounds: Rect, x: i32, y: i32) -> Option<Rect> {
    let b = bounds.intersect(&img.bounds())?;
    if !b.contains(x, y) {
        return None;
    }
    let seed = flood(img, b, x, y, b.area() * 3 / 5);
    if seed.overflow || seed.count > SMALL_FLAT_AREA {
        // On a wide flat area: something right next to the pointer wins over the area.
        if let Some((nx, ny)) = nearby_detail(img, b, x, y) {
            if let Some(found) = detect_detail(img, b, nx, ny) {
                return Some(found);
            }
        }
        return looks_like_surface(&seed, b).then_some(seed.bbox);
    }
    if looks_like_surface(&seed, b) {
        return Some(seed.bbox);
    }
    detect_detail(img, b, x, y)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: [u8; 3] = [245, 245, 245];
    const BUTTON: [u8; 3] = [200, 120, 0];
    const INK: [u8; 3] = [20, 20, 20];

    fn canvas(w: u32, h: u32) -> Image {
        let mut bgra = Vec::with_capacity((w * h * 4) as usize);
        for _ in 0..w * h {
            bgra.extend_from_slice(&[PAGE[0], PAGE[1], PAGE[2], 255]);
        }
        Image::new(w, h, bgra)
    }

    fn fill(img: &mut Image, r: Rect, c: [u8; 3]) {
        for y in r.y..r.bottom() {
            for x in r.x..r.right() {
                let i = (y as usize * img.w as usize + x as usize) * 4;
                img.bgra[i..i + 3].copy_from_slice(&c);
            }
        }
    }

    /// Letter-like blocks separated by small gaps.
    fn text_line(img: &mut Image, x: i32, y: i32, letters: i32, c: [u8; 3]) -> Rect {
        for i in 0..letters {
            fill(img, Rect::new(x + i * 11, y, 8, 14), c);
        }
        Rect::new(x, y, letters * 11 - 3, 14)
    }

    #[test]
    fn page_background_selects_nothing() {
        let mut img = canvas(800, 600);
        fill(&mut img, Rect::new(100, 100, 120, 40), BUTTON);
        assert_eq!(detect(&img, img.bounds(), 500, 400), None);
    }

    #[test]
    fn click_on_a_flat_button_selects_the_button() {
        let mut img = canvas(800, 600);
        let button = Rect::new(100, 100, 120, 40);
        fill(&mut img, button, BUTTON);
        assert_eq!(detect(&img, img.bounds(), 110, 110), Some(button));
    }

    #[test]
    fn click_on_a_button_label_selects_the_button() {
        let mut img = canvas(800, 600);
        let button = Rect::new(100, 100, 120, 40);
        fill(&mut img, button, BUTTON);
        text_line(&mut img, 120, 113, 6, INK);
        assert_eq!(detect(&img, img.bounds(), 122, 118), Some(button));
    }

    #[test]
    fn click_on_text_selects_the_whole_line() {
        let mut img = canvas(800, 600);
        let line = text_line(&mut img, 200, 300, 12, INK);
        assert_eq!(detect(&img, img.bounds(), 224, 305), Some(line));
    }

    #[test]
    fn pointer_between_letters_still_selects_the_line() {
        let mut img = canvas(800, 600);
        let line = text_line(&mut img, 200, 300, 12, INK);
        // Column 209 is the gap between the first two letters.
        assert_eq!(detect(&img, img.bounds(), 209, 305), Some(line));
    }

    #[test]
    fn pointer_between_letters_of_a_big_button_selects_the_button() {
        let mut img = canvas(800, 600);
        let button = Rect::new(100, 100, 200, 40);
        fill(&mut img, button, BUTTON);
        text_line(&mut img, 130, 113, 12, INK);
        assert_eq!(detect(&img, img.bounds(), 139, 118), Some(button));
    }

    #[test]
    fn icon_in_a_field_is_not_merged_with_neighbouring_fields() {
        let mut img = canvas(800, 600);
        let (a, b) = (Rect::new(100, 100, 60, 60), Rect::new(170, 100, 60, 60));
        for r in [a, b] {
            fill(&mut img, r, BUTTON);
        }
        let glyph = Rect::new(120, 125, 20, 10);
        fill(&mut img, glyph, INK);
        assert_eq!(detect(&img, img.bounds(), 130, 130), Some(glyph));
    }

    #[test]
    fn panel_larger_than_half_of_the_picture_is_a_background() {
        let mut img = canvas(800, 600);
        fill(&mut img, Rect::new(20, 20, 700, 500), BUTTON);
        assert_eq!(detect(&img, img.bounds(), 300, 300), None);
    }

    #[test]
    fn card_smaller_than_half_of_the_picture_is_an_object() {
        let mut img = canvas(800, 600);
        let card = Rect::new(100, 100, 300, 200);
        fill(&mut img, card, BUTTON);
        assert_eq!(detect(&img, img.bounds(), 250, 200), Some(card));
    }

    #[test]
    fn button_with_a_soft_gradient_is_selected_whole() {
        let mut img = canvas(800, 600);
        let button = Rect::new(100, 100, 160, 60);
        for row in 0..button.h {
            // Top to bottom the fill changes by 30 levels, one small step at a time.
            let v = 90 + (row / 2) as u8;
            fill(&mut img, Rect::new(button.x, button.y + row, button.w, 1), [v, v, v]);
        }
        assert_eq!(detect(&img, img.bounds(), 180, 150), Some(button));
    }

    #[test]
    fn separate_objects_are_not_merged() {
        let mut img = canvas(800, 600);
        let left = text_line(&mut img, 100, 100, 5, INK);
        text_line(&mut img, 400, 100, 5, INK);
        assert_eq!(detect(&img, img.bounds(), 102, 105), Some(left));
    }

    /// A real screenshot: Kadr's own toolbar above an app's lock screen.
    fn real_ui() -> Image {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/ui.png");
        Image::from_png_file(std::path::Path::new(path)).unwrap()
    }

    fn assert_near(actual: Option<Rect>, expected: (i32, i32, i32, i32), what: &str) {
        let a = actual.unwrap_or_else(|| panic!("{what}: nothing found"));
        let (l, t, r, b) = expected;
        let ok = (a.x - l).abs() <= 6 && (a.y - t).abs() <= 6 && (a.right() - r).abs() <= 6 && (a.bottom() - b).abs() <= 6;
        assert!(ok, "{what}: got {a:?}, expected about {expected:?}");
    }

    #[test]
    fn finds_objects_on_a_real_screenshot() {
        let img = real_ui();
        let b = img.bounds();
        assert_near(detect(&img, b, 280, 650), (222, 591, 337, 712), "app icon");
        assert_near(detect(&img, b, 280, 750), (152, 739, 407, 762), "heading text");
        assert_near(detect(&img, b, 248, 815), (222, 789, 275, 842), "empty PIN field");
        assert_near(detect(&img, b, 280, 884), (206, 876, 352, 893), "link text");
        assert_near(detect(&img, b, 917, 248), (908, 239, 927, 258), "moon icon");
        assert_near(detect(&img, b, 483, 70), (470, 57, 496, 84), "color swatch");
    }

    #[test]
    fn empty_page_of_a_real_screenshot_selects_nothing() {
        let img = real_ui();
        assert_eq!(detect(&img, img.bounds(), 700, 500), None);
    }

    #[test]
    fn search_stays_inside_the_crop() {
        let mut img = canvas(800, 600);
        fill(&mut img, Rect::new(100, 100, 300, 200), BUTTON);
        let crop = Rect::new(150, 150, 400, 400);
        assert_eq!(detect(&img, crop, 50, 50), None, "point outside the crop");
        let r = detect(&img, crop, 200, 200);
        assert!(r.is_none() || crop.intersect(&r.unwrap()) == r);
    }
}
