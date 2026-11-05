//! In-memory BGRA image plus PNG encoding and decoding.

use crate::geom::Rect;
use std::io::{BufReader, BufWriter, Write};
use std::path::Path;

#[derive(Clone)]
pub struct Image {
    pub w: u32,
    pub h: u32,
    /// Top-down, 4 bytes per pixel, always fully opaque.
    pub bgra: Vec<u8>,
}

impl Image {
    pub fn new(w: u32, h: u32, bgra: Vec<u8>) -> Self {
        debug_assert_eq!(bgra.len(), (w * h * 4) as usize);
        Self { w, h, bgra }
    }

    pub fn bounds(&self) -> Rect {
        Rect::new(0, 0, self.w as i32, self.h as i32)
    }

    #[inline]
    pub fn px(&self, x: i32, y: i32) -> [u8; 3] {
        let i = (y as usize * self.w as usize + x as usize) * 4;
        [self.bgra[i], self.bgra[i + 1], self.bgra[i + 2]]
    }

    /// Copies a part of the image; the rectangle is clamped to the image.
    pub fn crop(&self, r: Rect) -> Image {
        let Some(r) = r.intersect(&self.bounds()) else {
            return Image::new(0, 0, Vec::new());
        };
        let stride = self.w as usize * 4;
        let row = r.w as usize * 4;
        let mut out = Vec::with_capacity(row * r.h as usize);
        for y in r.y..r.bottom() {
            let start = y as usize * stride + r.x as usize * 4;
            out.extend_from_slice(&self.bgra[start..start + row]);
        }
        Image::new(r.w as u32, r.h as u32, out)
    }

    /// A smaller copy that fits the given size; small images stay as they are.
    pub fn thumbnail(&self, max_w: u32, max_h: u32) -> Image {
        let k = (max_w as f64 / self.w.max(1) as f64).min(max_h as f64 / self.h.max(1) as f64);
        if k >= 1.0 {
            return self.clone();
        }
        let w = ((self.w as f64 * k).round() as u32).max(1);
        let h = ((self.h as f64 * k).round() as u32).max(1);
        let mut out = Vec::with_capacity((w * h * 4) as usize);
        for dy in 0..h {
            let y0 = (dy as u64 * self.h as u64 / h as u64) as i32;
            let y1 = (((dy + 1) as u64 * self.h as u64 / h as u64) as i32).max(y0 + 1);
            for dx in 0..w {
                let x0 = (dx as u64 * self.w as u64 / w as u64) as i32;
                let x1 = (((dx + 1) as u64 * self.w as u64 / w as u64) as i32).max(x0 + 1);
                // Averaging keeps thin lines and text visible in the small picture.
                let (mut sum, mut n) = ([0u32; 3], 0u32);
                for y in y0..y1 {
                    for x in x0..x1 {
                        let p = self.px(x, y);
                        for c in 0..3 {
                            sum[c] += p[c] as u32;
                        }
                        n += 1;
                    }
                }
                out.extend_from_slice(&[(sum[0] / n) as u8, (sum[1] / n) as u8, (sum[2] / n) as u8, 255]);
            }
        }
        Image::new(w, h, out)
    }

    /// A copy at twice the size with smooth edges, for text recognition.
    pub fn doubled(&self) -> Image {
        let (w, h) = (self.w as usize, self.h as usize);
        let (dw, dh) = (w * 2, h * 2);
        let mut out = vec![255u8; dw * dh * 4];
        let at = |x: usize, y: usize, c: usize| self.bgra[(y.min(h - 1) * w + x.min(w - 1)) * 4 + c] as u32;
        for y in 0..dh {
            let (sy, fy) = (y / 2, y % 2);
            for x in 0..dw {
                let (sx, fx) = (x / 2, x % 2);
                let i = (y * dw + x) * 4;
                for c in 0..3 {
                    // Even positions keep the source pixel, odd ones average with the neighbor.
                    let left = at(sx, sy, c) + if fy == 1 { at(sx, sy + 1, c) } else { at(sx, sy, c) };
                    let right = if fx == 1 {
                        at(sx + 1, sy, c) + if fy == 1 { at(sx + 1, sy + 1, c) } else { at(sx + 1, sy, c) }
                    } else {
                        left
                    };
                    out[i + c] = ((left + right + 2) / 4) as u8;
                }
            }
        }
        Image::new(dw as u32, dh as u32, out)
    }

    fn write_png<W: Write>(&self, out: W) -> Result<(), String> {
        let mut enc = png::Encoder::new(out, self.w, self.h);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_compression(png::Compression::Fast);
        let mut writer = enc.write_header().map_err(|e| e.to_string())?;
        let mut rgb = Vec::with_capacity((self.w * self.h * 3) as usize);
        for p in self.bgra.chunks_exact(4) {
            rgb.extend_from_slice(&[p[2], p[1], p[0]]);
        }
        writer.write_image_data(&rgb).map_err(|e| e.to_string())?;
        writer.finish().map_err(|e| e.to_string())
    }

    pub fn to_png(&self) -> Result<Vec<u8>, String> {
        let mut out = Vec::new();
        self.write_png(&mut out)?;
        Ok(out)
    }

    pub fn save_png(&self, path: &Path) -> Result<(), String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let file = std::fs::File::create(path).map_err(|e| e.to_string())?;
        self.write_png(BufWriter::new(file))
    }

    pub fn from_png_file(path: &Path) -> Result<Image, String> {
        let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
        let mut dec = png::Decoder::new(BufReader::new(file));
        dec.set_transformations(png::Transformations::normalize_to_color8());
        let mut reader = dec.read_info().map_err(|e| e.to_string())?;
        let size = reader.output_buffer_size().ok_or("PNG is too large")?;
        let mut buf = vec![0u8; size];
        let info = reader.next_frame(&mut buf).map_err(|e| e.to_string())?;
        let (w, h) = (info.width, info.height);
        let mut bgra = Vec::with_capacity((w * h * 4) as usize);
        let data = &buf[..info.buffer_size()];
        match info.color_type {
            png::ColorType::Rgb => {
                for p in data.chunks_exact(3) {
                    bgra.extend_from_slice(&[p[2], p[1], p[0], 255]);
                }
            }
            png::ColorType::Rgba => {
                // Composite over white: the editor works with opaque images.
                for p in data.chunks_exact(4) {
                    let a = p[3] as u32;
                    let mix = |c: u8| ((c as u32 * a + 255 * (255 - a)) / 255) as u8;
                    bgra.extend_from_slice(&[mix(p[2]), mix(p[1]), mix(p[0]), 255]);
                }
            }
            png::ColorType::Grayscale => {
                for &g in data {
                    bgra.extend_from_slice(&[g, g, g, 255]);
                }
            }
            png::ColorType::GrayscaleAlpha => {
                for p in data.chunks_exact(2) {
                    let a = p[1] as u32;
                    let g = ((p[0] as u32 * a + 255 * (255 - a)) / 255) as u8;
                    bgra.extend_from_slice(&[g, g, g, 255]);
                }
            }
            other => return Err(format!("unsupported PNG color type {other:?}")),
        }
        Ok(Image::new(w, h, bgra))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Image {
        // 2x2: blue, green, red, white.
        Image::new(
            2,
            2,
            vec![255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255],
        )
    }

    #[test]
    fn crop_takes_the_requested_pixels() {
        let c = sample().crop(Rect::new(1, 1, 5, 5));
        assert_eq!((c.w, c.h), (1, 1));
        assert_eq!(c.bgra, vec![255, 255, 255, 255]);
    }

    #[test]
    fn thumbnail_keeps_proportions_and_averages_pixels() {
        let img = Image::new(4, 2, [[0u8, 0, 0, 255], [200, 200, 200, 255]].repeat(4).concat());
        let t = img.thumbnail(2, 2);
        assert_eq!((t.w, t.h), (2, 1));
        assert_eq!(t.px(0, 0), [100, 100, 100]);
    }

    #[test]
    fn doubling_keeps_pixels_and_blends_between_them() {
        // One row: black, white.
        let img = Image::new(2, 1, vec![0, 0, 0, 255, 200, 200, 200, 255]);
        let d = img.doubled();
        assert_eq!((d.w, d.h), (4, 2));
        assert_eq!(d.px(0, 0), [0, 0, 0]);
        assert_eq!(d.px(1, 0), [100, 100, 100]);
        assert_eq!(d.px(2, 0), [200, 200, 200]);
        assert_eq!(d.px(3, 0), [200, 200, 200]);
    }

    #[test]
    fn thumbnail_does_not_enlarge() {
        let t = sample().thumbnail(100, 100);
        assert_eq!((t.w, t.h), (2, 2));
    }

    #[test]
    fn png_round_trip_keeps_colors_in_place() {
        let dir = std::env::temp_dir().join(format!("kadr-test-{}", std::process::id()));
        let path = dir.join("t.png");
        let img = sample();
        img.save_png(&path).unwrap();
        let back = Image::from_png_file(&path).unwrap();
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!((back.w, back.h), (2, 2));
        assert_eq!(back.bgra, img.bgra);
    }
}
