//! Direct2D and DirectWrite drawing helpers used by every window of the app.

use crate::image::Image;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use windows::UI::ViewManagement::{UIColorType, UISettings};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::*;
use windows_numerics::{Matrix3x2, Vector2};

pub type Color = D2D1_COLOR_F;

pub const fn rgba(hex: u32, a: f32) -> Color {
    D2D1_COLOR_F {
        r: ((hex >> 16) & 0xff) as f32 / 255.0,
        g: ((hex >> 8) & 0xff) as f32 / 255.0,
        b: (hex & 0xff) as f32 / 255.0,
        a,
    }
}

pub const fn rgb(hex: u32) -> Color {
    rgba(hex, 1.0)
}

// The dark theme colors of Windows 11 (Fluent).
pub const TRANSPARENT: Color = rgba(0x000000, 0.0);
pub const TEXT: Color = rgb(0xffffff);
pub const TEXT_DIM: Color = rgba(0xffffff, 0.66);
pub const TEXT_OFF: Color = rgba(0xffffff, 0.36);
pub const BAR_BG: Color = rgba(0x202020, 0.98);
pub const DIVIDER: Color = rgba(0xffffff, 0.084);
pub const HOVER: Color = rgba(0xffffff, 0.061);
pub const CONTROL: Color = rgba(0xffffff, 0.061);
pub const CONTROL_HOVER: Color = rgba(0xffffff, 0.095);
pub const CONTROL_STROKE: Color = rgba(0xffffff, 0.07);
pub const CARD: Color = rgba(0xffffff, 0.051);
pub const CARD_STROKE: Color = rgba(0x000000, 0.18);
pub const FLYOUT: Color = rgb(0x2c2c2c);
pub const FLYOUT_STROKE: Color = rgba(0xffffff, 0.09);
pub const ON_ACCENT: Color = rgba(0x000000, 0.9);
pub const RED: Color = rgb(0xe5383b);

const FALLBACK_ACCENT: Color = rgb(0x60cdff);

thread_local! {
    static ACCENT: Cell<Color> = const { Cell::new(FALLBACK_ACCENT) };
}

/// The accent color chosen in Windows, in the shade used on dark backgrounds.
pub fn accent() -> Color {
    ACCENT.with(|a| a.get())
}

/// Reads the accent color again; called when a window opens, as it can change at any time.
pub fn refresh_accent() {
    let read = || -> Result<Color> {
        let c = UISettings::new()?.GetColorValue(UIColorType::AccentLight2)?;
        Ok(D2D1_COLOR_F { r: c.R as f32 / 255.0, g: c.G as f32 / 255.0, b: c.B as f32 / 255.0, a: 1.0 })
    };
    if let Ok(color) = read() {
        ACCENT.with(|a| a.set(color));
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct RectF {
    pub l: f32,
    pub t: f32,
    pub r: f32,
    pub b: f32,
}

impl RectF {
    pub const fn new(l: f32, t: f32, r: f32, b: f32) -> Self {
        Self { l, t, r, b }
    }

    pub fn xywh(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self::new(x, y, x + w, y + h)
    }

    pub fn from_rect(r: crate::geom::Rect) -> Self {
        Self::xywh(r.x as f32, r.y as f32, r.w as f32, r.h as f32)
    }

    pub fn w(&self) -> f32 {
        self.r - self.l
    }

    pub fn h(&self) -> f32 {
        self.b - self.t
    }

    pub fn cx(&self) -> f32 {
        (self.l + self.r) / 2.0
    }

    pub fn cy(&self) -> f32 {
        (self.t + self.b) / 2.0
    }

    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.l && x < self.r && y >= self.t && y < self.b
    }

    pub fn inflate(&self, d: f32) -> Self {
        Self::new(self.l - d, self.t - d, self.r + d, self.b + d)
    }

    pub fn normalized(&self) -> Self {
        Self::new(self.l.min(self.r), self.t.min(self.b), self.l.max(self.r), self.t.max(self.b))
    }

    pub fn translated(&self, dx: f32, dy: f32) -> Self {
        Self::new(self.l + dx, self.t + dy, self.r + dx, self.b + dy)
    }

    fn d2d(&self) -> D2D_RECT_F {
        D2D_RECT_F { left: self.l, top: self.t, right: self.r, bottom: self.b }
    }
}

fn v2(p: (f32, f32)) -> Vector2 {
    Vector2 { X: p.0, Y: p.1 }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum Align {
    Left,
    Center,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum Font {
    Ui,
    UiBold,
    Icons,
    /// Annotation text on screenshots: no wrapping, top-left anchored.
    Note,
}

struct Shared {
    d2d: ID2D1Factory,
    dw: IDWriteFactory,
    round_stroke: ID2D1StrokeStyle,
    dash_stroke: ID2D1StrokeStyle,
    formats: RefCell<HashMap<(Font, u32, Align), IDWriteTextFormat>>,
}

impl Shared {
    fn create() -> Result<Self> {
        unsafe {
            let d2d: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
            let dw: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
            let round = D2D1_STROKE_STYLE_PROPERTIES {
                startCap: D2D1_CAP_STYLE_ROUND,
                endCap: D2D1_CAP_STYLE_ROUND,
                dashCap: D2D1_CAP_STYLE_ROUND,
                lineJoin: D2D1_LINE_JOIN_ROUND,
                miterLimit: 10.0,
                dashStyle: D2D1_DASH_STYLE_SOLID,
                dashOffset: 0.0,
            };
            let dash = D2D1_STROKE_STYLE_PROPERTIES {
                startCap: D2D1_CAP_STYLE_FLAT,
                endCap: D2D1_CAP_STYLE_FLAT,
                dashCap: D2D1_CAP_STYLE_FLAT,
                lineJoin: D2D1_LINE_JOIN_MITER,
                miterLimit: 10.0,
                dashStyle: D2D1_DASH_STYLE_DASH,
                dashOffset: 0.0,
            };
            Ok(Self {
                round_stroke: d2d.CreateStrokeStyle(&round, None)?,
                dash_stroke: d2d.CreateStrokeStyle(&dash, None)?,
                d2d,
                dw,
                formats: RefCell::new(HashMap::new()),
            })
        }
    }

    fn format(&self, font: Font, size: f32, align: Align) -> Result<IDWriteTextFormat> {
        let key = (font, (size * 8.0) as u32, align);
        if let Some(f) = self.formats.borrow().get(&key) {
            return Ok(f.clone());
        }
        let (family, weight) = match font {
            Font::Ui => (w!("Segoe UI Variable Text"), DWRITE_FONT_WEIGHT_NORMAL),
            Font::UiBold => (w!("Segoe UI Variable Text"), DWRITE_FONT_WEIGHT_SEMI_BOLD),
            Font::Icons => (w!("Segoe Fluent Icons"), DWRITE_FONT_WEIGHT_NORMAL),
            Font::Note => (w!("Segoe UI"), DWRITE_FONT_WEIGHT_SEMI_BOLD),
        };
        unsafe {
            let f = self.dw.CreateTextFormat(
                family,
                None,
                weight,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                size,
                w!("ru-ru"),
            )?;
            f.SetTextAlignment(match align {
                Align::Left => DWRITE_TEXT_ALIGNMENT_LEADING,
                Align::Center => DWRITE_TEXT_ALIGNMENT_CENTER,
            })?;
            if font == Font::Note {
                f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_NEAR)?;
            } else {
                f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
            }
            f.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
            self.formats.borrow_mut().insert(key, f.clone());
            Ok(f)
        }
    }
}

thread_local! {
    // Deliberately leaked. A destructor here would run during process exit, under the
    // Windows loader lock, where releasing Direct2D deadlocks and the process never ends.
    static SHARED: &'static Shared =
        Box::leak(Box::new(Shared::create().expect("Direct2D is not available")));
}

pub fn d2d_factory() -> ID2D1Factory {
    SHARED.with(|s| s.d2d.clone())
}

/// Width and height of a block of text in the given font.
pub fn measure(text: &str, font: Font, size: f32) -> (f32, f32) {
    SHARED.with(|s| unsafe {
        let run = || -> Result<(f32, f32)> {
            let format = s.format(font, size, Align::Left)?;
            let utf16: Vec<u16> = text.encode_utf16().collect();
            let layout = s.dw.CreateTextLayout(&utf16, &format, 100_000.0, 100_000.0)?;
            let mut m = DWRITE_TEXT_METRICS::default();
            layout.GetMetrics(&mut m)?;
            Ok((m.widthIncludingTrailingWhitespace, m.height))
        };
        run().unwrap_or((0.0, size))
    })
}

/// Drawing commands bound to one render target. Cloning only copies COM references.
#[derive(Clone)]
pub struct Painter {
    pub rt: ID2D1RenderTarget,
    brush: ID2D1SolidColorBrush,
}

impl Painter {
    pub fn new(rt: ID2D1RenderTarget) -> Result<Self> {
        let brush = unsafe {
            // ClearType paints colored fringes on thin icons and in saved screenshots.
            rt.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
            rt.CreateSolidColorBrush(&rgb(0xffffff), None)?
        };
        Ok(Self { rt, brush })
    }

    fn paint(&self, c: Color) -> &ID2D1SolidColorBrush {
        unsafe { self.brush.SetColor(&c) };
        &self.brush
    }

    pub fn clear(&self, c: Color) {
        unsafe { self.rt.Clear(Some(&c)) }
    }

    pub fn set_transform(&self, scale: f32, dx: f32, dy: f32) {
        let m = Matrix3x2 { M11: scale, M12: 0.0, M21: 0.0, M22: scale, M31: dx, M32: dy };
        unsafe { self.rt.SetTransform(&m) }
    }

    pub fn reset_transform(&self) {
        self.set_transform(1.0, 0.0, 0.0);
    }

    pub fn push_clip(&self, r: RectF) {
        unsafe { self.rt.PushAxisAlignedClip(&r.d2d(), D2D1_ANTIALIAS_MODE_ALIASED) }
    }

    pub fn pop_clip(&self) {
        unsafe { self.rt.PopAxisAlignedClip() }
    }

    pub fn fill(&self, r: RectF, c: Color) {
        unsafe { self.rt.FillRectangle(&r.d2d(), self.paint(c)) }
    }

    pub fn stroke(&self, r: RectF, c: Color, w: f32) {
        unsafe { self.rt.DrawRectangle(&r.d2d(), self.paint(c), w, None) }
    }

    /// A soft shadow around a rectangle, for pictures lying on the window background.
    pub fn soft_shadow(&self, r: RectF, scale: f32) {
        const STEPS: i32 = 9;
        for i in 1..=STEPS {
            let spread = i as f32 * 1.6 * scale;
            let alpha = 0.055 * (STEPS - i + 1) as f32 / STEPS as f32;
            self.round_stroke(r.inflate(spread), spread, rgba(0x000000, alpha), 1.6 * scale);
        }
    }

    pub fn stroke_dashed(&self, r: RectF, c: Color, w: f32) {
        SHARED.with(|s| unsafe { self.rt.DrawRectangle(&r.d2d(), self.paint(c), w, &s.dash_stroke) })
    }

    pub fn round(&self, r: RectF, radius: f32, c: Color) {
        let rr = D2D1_ROUNDED_RECT { rect: r.d2d(), radiusX: radius, radiusY: radius };
        unsafe { self.rt.FillRoundedRectangle(&rr, self.paint(c)) }
    }

    pub fn round_stroke(&self, r: RectF, radius: f32, c: Color, w: f32) {
        let rr = D2D1_ROUNDED_RECT { rect: r.d2d(), radiusX: radius, radiusY: radius };
        unsafe { self.rt.DrawRoundedRectangle(&rr, self.paint(c), w, None) }
    }

    pub fn ellipse(&self, r: RectF, c: Color, w: f32) {
        let e = D2D1_ELLIPSE {
            point: v2((r.cx(), r.cy())),
            radiusX: r.w() / 2.0,
            radiusY: r.h() / 2.0,
        };
        unsafe { self.rt.DrawEllipse(&e, self.paint(c), w, None) }
    }

    pub fn fill_circle(&self, cx: f32, cy: f32, radius: f32, c: Color) {
        let e = D2D1_ELLIPSE { point: v2((cx, cy)), radiusX: radius, radiusY: radius };
        unsafe { self.rt.FillEllipse(&e, self.paint(c)) }
    }

    pub fn line(&self, a: (f32, f32), b: (f32, f32), c: Color, w: f32) {
        SHARED.with(|s| unsafe { self.rt.DrawLine(v2(a), v2(b), self.paint(c), w, &s.round_stroke) })
    }

    fn geometry(&self, pts: &[(f32, f32)], closed: bool, smooth: bool) -> Result<ID2D1PathGeometry> {
        unsafe {
            let geo = d2d_factory().CreatePathGeometry()?;
            let sink = geo.Open()?;
            let begin = if closed { D2D1_FIGURE_BEGIN_FILLED } else { D2D1_FIGURE_BEGIN_HOLLOW };
            sink.BeginFigure(v2(pts[0]), begin);
            if smooth && pts.len() > 2 {
                // Curves through segment midpoints turn mouse samples into a soft line.
                for i in 1..pts.len() - 1 {
                    let mid = ((pts[i].0 + pts[i + 1].0) / 2.0, (pts[i].1 + pts[i + 1].1) / 2.0);
                    sink.AddQuadraticBezier(&D2D1_QUADRATIC_BEZIER_SEGMENT {
                        point1: v2(pts[i]),
                        point2: v2(mid),
                    });
                }
                sink.AddLine(v2(pts[pts.len() - 1]));
            } else {
                for p in &pts[1..] {
                    sink.AddLine(v2(*p));
                }
            }
            sink.EndFigure(if closed { D2D1_FIGURE_END_CLOSED } else { D2D1_FIGURE_END_OPEN });
            sink.Close()?;
            Ok(geo)
        }
    }

    /// A freehand line drawn as one shape, so a translucent marker stays even
    /// where the stroke crosses itself.
    pub fn stroke_path(&self, pts: &[(f32, f32)], c: Color, w: f32) {
        if pts.is_empty() {
            return;
        }
        if pts.len() == 1 {
            self.fill_circle(pts[0].0, pts[0].1, w / 2.0, c);
            return;
        }
        if let Ok(geo) = self.geometry(pts, false, true) {
            SHARED.with(|s| unsafe { self.rt.DrawGeometry(&geo, self.paint(c), w, &s.round_stroke) });
        }
    }

    pub fn fill_polygon(&self, pts: &[(f32, f32)], c: Color) {
        if let Ok(geo) = self.geometry(pts, true, false) {
            unsafe { self.rt.FillGeometry(&geo, self.paint(c), None) };
        }
    }

    pub fn text(&self, s: &str, r: RectF, font: Font, size: f32, c: Color, align: Align) {
        let format = SHARED.with(|sh| sh.format(font, size, align));
        if let Ok(format) = format {
            let utf16: Vec<u16> = s.encode_utf16().collect();
            unsafe {
                self.rt.DrawText(
                    &utf16,
                    &format,
                    &r.d2d(),
                    self.paint(c),
                    D2D1_DRAW_TEXT_OPTIONS_NONE,
                    DWRITE_MEASURING_MODE_NATURAL,
                );
            }
        }
    }

    pub fn icon(&self, glyph: char, cx: f32, cy: f32, size: f32, c: Color) {
        let mut buf = [0u8; 4];
        let r = RectF::xywh(cx - size, cy - size, size * 2.0, size * 2.0);
        self.text(glyph.encode_utf8(&mut buf), r, Font::Icons, size, c, Align::Center);
    }

    pub fn bitmap(&self, img: &Image) -> Result<ID2D1Bitmap> {
        let props = D2D1_BITMAP_PROPERTIES {
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
            },
            dpiX: 96.0,
            dpiY: 96.0,
        };
        unsafe {
            self.rt.CreateBitmap(
                D2D_SIZE_U { width: img.w, height: img.h },
                Some(img.bgra.as_ptr() as _),
                img.w * 4,
                &props,
            )
        }
    }

    pub fn draw_bitmap(&self, bmp: &ID2D1Bitmap, dst: RectF, src: Option<RectF>, smooth: bool) {
        let mode = if smooth {
            D2D1_BITMAP_INTERPOLATION_MODE_LINEAR
        } else {
            D2D1_BITMAP_INTERPOLATION_MODE_NEAREST_NEIGHBOR
        };
        let src = src.map(|s| s.d2d());
        unsafe {
            self.rt.DrawBitmap(bmp, Some(&dst.d2d()), 1.0, mode, src.as_ref().map(|s| s as *const _));
        }
    }
}

/// Render target of a window, recreated after a GPU reset.
pub struct Surface {
    hwnd: HWND,
    target: Option<(ID2D1HwndRenderTarget, Painter)>,
    /// Keeps alpha, so that Mica shows through what the frame leaves unpainted.
    see_through: bool,
    /// Changes whenever bitmaps created on this surface have to be recreated.
    pub generation: u64,
}

impl Surface {
    pub fn new(hwnd: HWND) -> Self {
        Self { hwnd, target: None, see_through: false, generation: 0 }
    }

    /// For windows with `win::fluent_frame`.
    pub fn glass(hwnd: HWND) -> Self {
        Self { hwnd, target: None, see_through: true, generation: 0 }
    }

    fn create(&mut self) -> Result<()> {
        let (w, h) = crate::win::client_size(self.hwnd);
        let props = D2D1_RENDER_TARGET_PROPERTIES {
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: if self.see_through { D2D1_ALPHA_MODE_PREMULTIPLIED } else { D2D1_ALPHA_MODE_IGNORE },
            },
            dpiX: 96.0,
            dpiY: 96.0,
            ..Default::default()
        };
        let hwnd_props = D2D1_HWND_RENDER_TARGET_PROPERTIES {
            hwnd: self.hwnd,
            pixelSize: D2D_SIZE_U { width: w.max(1) as u32, height: h.max(1) as u32 },
            presentOptions: D2D1_PRESENT_OPTIONS_IMMEDIATELY,
        };
        unsafe {
            let rt = d2d_factory().CreateHwndRenderTarget(&props, &hwnd_props)?;
            let painter = Painter::new(rt.cast()?)?;
            self.target = Some((rt, painter));
        }
        self.generation += 1;
        Ok(())
    }

    /// Starts a frame. Returns `None` when the GPU is unavailable right now.
    pub fn begin(&mut self) -> Option<Painter> {
        if self.target.is_none() {
            if let Err(e) = self.create() {
                crate::paths::log(&format!("render target: {e}"));
                return None;
            }
        }
        let (rt, painter) = self.target.as_ref()?;
        unsafe { rt.BeginDraw() };
        Some(painter.clone())
    }

    pub fn end(&mut self) {
        if let Some((rt, _)) = &self.target {
            let result = unsafe { rt.EndDraw(None, None) };
            if let Err(e) = result {
                if e.code() == D2DERR_RECREATE_TARGET {
                    self.target = None;
                    crate::win::invalidate(self.hwnd);
                }
            }
        }
    }

    pub fn resize(&mut self) {
        if let Some((rt, _)) = &self.target {
            let (w, h) = crate::win::client_size(self.hwnd);
            let size = D2D_SIZE_U { width: w.max(1) as u32, height: h.max(1) as u32 };
            unsafe {
                let _ = rt.Resize(&size);
            }
        }
    }
}
