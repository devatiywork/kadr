//! Screenshot editor: annotations on top of the captured picture.
//! The original pixels are never changed, so every step can be undone.

use crate::geom::Rect;
use crate::gfx::{self, *};
use crate::image::Image;
use crate::settings;
use crate::smart;
use crate::win::{self, WindowSpec, WndState};
use std::path::PathBuf;
use std::time::Instant;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Gdi::{ScreenToClient, ValidateRect};
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::*;
use windows::Win32::UI::Controls::WM_MOUSELEAVE;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::*;

const CLASS: PCWSTR = w!("KadrEditor");
/// Darkens Mica under the picture, so that the toolbar reads as a separate layer.
const CANVAS_LAYER: Color = rgba(0x000000, 0.2);
const PALETTE: [u32; 24] = [
    0x000000, 0x4D4D4D, 0x8C8C8C, 0xC8C8C8, 0xFFFFFF, 0xF5E6C8, 0x8B5A2B, 0x5C3A1E, //
    0xE81224, 0xF7630C, 0xFFB900, 0xFFF100, 0x16C60C, 0x0B6A0B, 0x00B7C3, 0x0078D4, //
    0x4F4BC9, 0x886CE4, 0x9B4F96, 0xE3008C, 0xF7B2D9, 0xFFD27F, 0xB4E7A8, 0x9FD3F7,
];
const PALETTE_COLUMNS: usize = 8;
const WIDTH_MIN: f32 = 1.0;
const WIDTH_MAX: f32 = 16.0;
const MARKER_FACTOR: f32 = 4.0;
const MARKER_ALPHA: f32 = 0.42;
const MOSAIC_CELL: f32 = 12.0;
/// Mouse travel below this is a click, not a drag (in window pixels).
const CLICK_SLOP: i32 = 4;
const HOVER_TIMER: usize = 7;
const CONFIRM_TIMER: usize = 8;
/// The confirmation on the copy and save buttons, in milliseconds from its start: the icon
/// shrinks away, a check mark pops up and stays, then the icon comes back.
const CONFIRM_OUT_MS: f32 = 90.0;
const CONFIRM_POP_MS: f32 = 260.0;
const CONFIRM_BACK_MS: f32 = 1000.0;
const CONFIRM_END_MS: f32 = 1180.0;
const MIN_WIDTH_DIP: f32 = 1040.0;

type P = (f32, f32);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tool {
    Pointer,
    /// A frame that also finds the object under the mouse.
    Frame,
    Square,
    Ellipse,
    Line,
    Arrow,
    Marker,
    Pen,
    Eraser,
    Text,
    Blur,
    Crop,
}

const SHAPES: [(Tool, &str); 4] = [
    (Tool::Square, "Квадрат (Q)"),
    (Tool::Ellipse, "Овал (E)"),
    (Tool::Line, "Линия (L)"),
    (Tool::Arrow, "Стрелка (A)"),
];

fn is_shape(tool: Tool) -> bool {
    SHAPES.iter().any(|s| s.0 == tool)
}

/// With Shift held: equal sides for a square and an oval, steps of 45 degrees for lines.
fn constrained_end(tool: Tool, a: P, b: P) -> P {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    match tool {
        Tool::Square | Tool::Frame | Tool::Ellipse => {
            let side = dx.abs().max(dy.abs());
            (a.0 + side.copysign(dx), a.1 + side.copysign(dy))
        }
        Tool::Line | Tool::Arrow => {
            let len = (dx * dx + dy * dy).sqrt();
            let step = std::f32::consts::FRAC_PI_4;
            let angle = (dy.atan2(dx) / step).round() * step;
            (a.0 + len * angle.cos(), a.1 + len * angle.sin())
        }
        _ => b,
    }
}

#[derive(Clone)]
enum Item {
    Frame { r: RectF, color: u32, w: f32 },
    Ellipse { r: RectF, color: u32, w: f32 },
    Line { a: P, b: P, color: u32, w: f32 },
    Arrow { a: P, b: P, color: u32, w: f32 },
    Pen { pts: Vec<P>, color: u32, w: f32 },
    Marker { pts: Vec<P>, color: u32, w: f32 },
    Text { at: P, text: String, color: u32, size: f32, extent: P },
    Blur { r: RectF, mosaic: Image },
}

fn dist_to_segment(p: P, a: P, b: P) -> f32 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len2 = dx * dx + dy * dy;
    let t = if len2 == 0.0 { 0.0 } else { (((p.0 - a.0) * dx + (p.1 - a.1) * dy) / len2).clamp(0.0, 1.0) };
    ((p.0 - a.0 - t * dx).powi(2) + (p.1 - a.1 - t * dy).powi(2)).sqrt()
}

fn near_polyline(pts: &[P], p: P, tol: f32) -> bool {
    match pts {
        [] => false,
        [one] => dist_to_segment(p, *one, *one) <= tol,
        _ => pts.windows(2).any(|s| dist_to_segment(p, s[0], s[1]) <= tol),
    }
}

impl Item {
    fn bounds(&self) -> RectF {
        let of_points = |pts: &[P]| {
            let mut r = RectF::new(f32::MAX, f32::MAX, f32::MIN, f32::MIN);
            for p in pts {
                r = RectF::new(r.l.min(p.0), r.t.min(p.1), r.r.max(p.0), r.b.max(p.1));
            }
            r
        };
        match self {
            Item::Frame { r, .. } | Item::Ellipse { r, .. } | Item::Blur { r, .. } => *r,
            Item::Line { a, b, .. } | Item::Arrow { a, b, .. } => of_points(&[*a, *b]),
            Item::Pen { pts, .. } | Item::Marker { pts, .. } => of_points(pts),
            Item::Text { at, extent, .. } => RectF::xywh(at.0, at.1, extent.0, extent.1),
        }
    }

    /// Whether the point touches the visible part of the item.
    fn hit(&self, p: P, tol: f32) -> bool {
        match self {
            Item::Frame { r, w, .. } | Item::Ellipse { r, w, .. } => {
                let tol = tol + w / 2.0;
                let outer = r.inflate(tol).contains(p.0, p.1);
                let inner = r.inflate(-tol).contains(p.0, p.1);
                outer && !inner
            }
            Item::Line { a, b, w, .. } | Item::Arrow { a, b, w, .. } => dist_to_segment(p, *a, *b) <= tol + w,
            Item::Pen { pts, w, .. } | Item::Marker { pts, w, .. } => near_polyline(pts, p, tol + w / 2.0),
            Item::Text { .. } | Item::Blur { .. } => self.bounds().inflate(tol).contains(p.0, p.1),
        }
    }

    fn translate(&mut self, dx: f32, dy: f32, src: &Image) {
        match self {
            Item::Frame { r, .. } | Item::Ellipse { r, .. } => *r = r.translated(dx, dy),
            Item::Line { a, b, .. } | Item::Arrow { a, b, .. } => {
                *a = (a.0 + dx, a.1 + dy);
                *b = (b.0 + dx, b.1 + dy);
            }
            Item::Pen { pts, .. } | Item::Marker { pts, .. } => {
                for p in pts {
                    *p = (p.0 + dx, p.1 + dy);
                }
            }
            Item::Text { at, .. } => *at = (at.0 + dx, at.1 + dy),
            Item::Blur { r, mosaic } => {
                *r = r.translated(dx, dy);
                *mosaic = make_mosaic(src, *r);
            }
        }
    }
}

/// Averages the source into coarse cells: unlike a blur, this cannot be undone
/// to read what was hidden.
fn make_mosaic(src: &Image, r: RectF) -> Image {
    let cols = ((r.w() / MOSAIC_CELL).ceil() as u32).max(1);
    let rows = ((r.h() / MOSAIC_CELL).ceil() as u32).max(1);
    let mut out = Vec::with_capacity((cols * rows * 4) as usize);
    let bounds = src.bounds();
    for row in 0..rows {
        for col in 0..cols {
            let cell = Rect::new(
                (r.l + col as f32 * MOSAIC_CELL) as i32,
                (r.t + row as f32 * MOSAIC_CELL) as i32,
                MOSAIC_CELL as i32,
                MOSAIC_CELL as i32,
            );
            let (mut sum, mut n) = ([0u32; 3], 0u32);
            if let Some(c) = cell.intersect(&bounds) {
                for y in c.y..c.bottom() {
                    for x in c.x..c.right() {
                        let px = src.px(x, y);
                        for i in 0..3 {
                            sum[i] += px[i] as u32;
                        }
                        n += 1;
                    }
                }
            }
            let n = n.max(1);
            out.extend_from_slice(&[(sum[0] / n) as u8, (sum[1] / n) as u8, (sum[2] / n) as u8, 255]);
        }
    }
    Image::new(cols, rows, out)
}

fn draw_arrow(p: &Painter, a: P, b: P, c: Color, w: f32) {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len = (dx * dx + dy * dy).sqrt();
    if len < 1.0 {
        return;
    }
    let (ux, uy) = (dx / len, dy / len);
    let head = (w * 4.0).max(12.0).min(len);
    let base = (b.0 - ux * head, b.1 - uy * head);
    let half = head * 0.5;
    p.line(a, base, c, w);
    p.fill_polygon(
        &[b, (base.0 - uy * half, base.1 + ux * half), (base.0 + uy * half, base.1 - ux * half)],
        c,
    );
}

fn draw_item(p: &Painter, item: &Item) {
    match item {
        Item::Frame { r, color, w } => p.round_stroke(*r, *w * 1.2, rgb(*color), *w),
        Item::Ellipse { r, color, w } => p.ellipse(*r, rgb(*color), *w),
        Item::Line { a, b, color, w } => p.line(*a, *b, rgb(*color), *w),
        Item::Arrow { a, b, color, w } => draw_arrow(p, *a, *b, rgb(*color), *w),
        Item::Pen { pts, color, w } => p.stroke_path(pts, rgb(*color), *w),
        Item::Marker { pts, color, w } => p.stroke_path(pts, rgba(*color, MARKER_ALPHA), *w),
        Item::Text { at, text, color, size, .. } => {
            let r = RectF::xywh(at.0, at.1, 100_000.0, 100_000.0);
            p.text(text, r, Font::Note, *size, rgb(*color), Align::Left);
        }
        Item::Blur { r, mosaic } => {
            if let Ok(bmp) = p.bitmap(mosaic) {
                p.draw_bitmap(&bmp, *r, None, false);
            }
        }
    }
}

#[derive(Clone)]
struct Doc {
    items: Vec<Item>,
    crop: Rect,
}

/// Undo and redo stacks of whole document states.
struct History<T> {
    undo: Vec<T>,
    redo: Vec<T>,
}

impl<T: Clone> History<T> {
    fn new() -> Self {
        Self { undo: Vec::new(), redo: Vec::new() }
    }

    /// Call with the state from before a change.
    fn record(&mut self, before: T) {
        self.undo.push(before);
        self.redo.clear();
    }

    fn undo(&mut self, current: &T) -> Option<T> {
        let previous = self.undo.pop()?;
        self.redo.push(current.clone());
        Some(previous)
    }

    fn redo(&mut self, current: &T) -> Option<T> {
        let next = self.redo.pop()?;
        self.undo.push(current.clone());
        Some(next)
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Hit {
    New,
    Tool(Tool),
    Shapes,
    Style,
    EraseAll,
    ScanText,
    Undo,
    Redo,
    Fit,
    Copy,
    Save,
}

struct Btn {
    hit: Hit,
    r: RectF,
    tip: &'static str,
}

enum Drag {
    None,
    Shape { start: P, cur: P, down: (i32, i32), moved: bool },
    Stroke(Vec<P>),
    /// `before` is the document until the first item is actually erased.
    Erase { before: Option<Doc>, erased: bool },
    Move { last: P, before: Option<Doc> },
    Pan { last: (i32, i32) },
}

struct TextEdit {
    at: P,
    text: String,
    color: u32,
    size: f32,
}

struct Editor {
    hwnd: HWND,
    surface: Surface,
    src: Image,
    bitmap: Option<ID2D1Bitmap>,
    generation: u64,
    doc: Doc,
    history: History<Doc>,
    tool: Tool,
    color: u32,
    width: f32,
    /// The palette under the color button is open.
    palette_open: bool,
    /// The list of shapes is open, and the shape its button currently stands for.
    shapes_open: bool,
    shape: Tool,
    dragging_width: bool,
    /// The question about saving is on the screen.
    asking_to_save: bool,
    /// The user answered it; the next WM_CLOSE really closes.
    close_confirmed: bool,
    drag: Drag,
    edit: Option<TextEdit>,
    selected: Option<usize>,
    hover_object: Option<Rect>,
    last_detect: Instant,
    mouse: (i32, i32),
    hover_btn: Option<usize>,
    pressed_btn: Option<usize>,
    btns: Vec<Btn>,
    separators: Vec<f32>,
    scale: f32,
    zoom: f32,
    origin: P,
    user_zoomed: bool,
    dirty: bool,
    /// The button showing its confirmation and when it began.
    confirming: Option<(Hit, Instant)>,
}

/// Opens a new editor window for a captured image.
pub fn open(img: Image) {
    let monitors = crate::capture::monitors();
    let (mx, my) = win::cursor_pos();
    let m = crate::capture::monitor_at(&monitors, mx, my);
    let (work, scale) = m.map(|m| (m.work, m.scale.max(1.0))).unwrap_or((Rect::new(0, 0, 1280, 800), 1.0));
    let want_w = (img.w as f32 + 72.0 * scale).max(MIN_WIDTH_DIP * scale);
    let want_h = (img.h as f32 + 48.0 * scale + 72.0 * scale).max(560.0 * scale);
    let w = (want_w as i32).min(work.w * 9 / 10);
    let h = (want_h as i32).min(work.h * 9 / 10);
    let rect = Rect::new(work.x + (work.w - w) / 2, work.y + (work.h - h) / 2, w, h);

    refresh_accent();
    let s = settings::get();
    let crop = img.bounds();
    let state = Editor {
        hwnd: HWND::default(),
        surface: Surface::new(HWND::default()),
        src: img,
        bitmap: None,
        generation: 0,
        doc: Doc { items: Vec::new(), crop },
        history: History::new(),
        tool: Tool::Frame,
        color: s.ink_color,
        width: s.ink_width.clamp(WIDTH_MIN, WIDTH_MAX),
        palette_open: false,
        shapes_open: false,
        shape: Tool::Square,
        dragging_width: false,
        asking_to_save: false,
        close_confirmed: false,
        drag: Drag::None,
        edit: None,
        selected: None,
        hover_object: None,
        last_detect: Instant::now(),
        mouse: (-1, -1),
        hover_btn: None,
        pressed_btn: None,
        btns: Vec::new(),
        separators: Vec::new(),
        scale,
        zoom: 1.0,
        origin: (0.0, 0.0),
        user_zoomed: false,
        dirty: false,
        confirming: None,
    };
    let spec = WindowSpec {
        class: CLASS,
        title: w!("Kadr — редактор"),
        style: WS_OVERLAPPEDWINDOW,
        ex_style: WINDOW_EX_STYLE(0),
        rect,
    };
    match win::create_window(spec, state) {
        Ok(hwnd) => {
            crate::app::window_opened();
            win::fluent_frame(hwnd);
            unsafe {
                let _ = ShowWindow(hwnd, SW_SHOW);
            }
            win::force_foreground(hwnd);
        }
        Err(e) => win::show_error(&format!("Не удалось открыть редактор: {e}")),
    }
}

/// One moment of the confirmation on the copy and save buttons.
#[derive(Clone, Copy, Debug)]
struct ConfirmFrame {
    icon_scale: f32,
    icon_alpha: f32,
    check_scale: f32,
    check_alpha: f32,
}

const NO_CONFIRM: ConfirmFrame = ConfirmFrame { icon_scale: 1.0, icon_alpha: 1.0, check_scale: 1.0, check_alpha: 0.0 };

/// `None` once the confirmation is over.
fn confirm_frame(ms: f32) -> Option<ConfirmFrame> {
    let part = |from: f32, to: f32| (ms - from) / (to - from);
    if ms < CONFIRM_OUT_MS {
        let u = part(0.0, CONFIRM_OUT_MS);
        Some(ConfirmFrame { icon_scale: 1.0 - 0.4 * u, icon_alpha: 1.0 - u, ..NO_CONFIRM })
    } else if ms < CONFIRM_POP_MS {
        let u = part(CONFIRM_OUT_MS, CONFIRM_POP_MS);
        // Passes its final size a little and settles back.
        const OVERSHOOT: f32 = 1.70158;
        let v = u - 1.0;
        let eased = 1.0 + (OVERSHOOT + 1.0) * v * v * v + OVERSHOOT * v * v;
        Some(ConfirmFrame { icon_alpha: 0.0, check_scale: 0.4 + 0.6 * eased, check_alpha: (u * 2.0).min(1.0), ..NO_CONFIRM })
    } else if ms < CONFIRM_BACK_MS {
        Some(ConfirmFrame { icon_alpha: 0.0, check_alpha: 1.0, ..NO_CONFIRM })
    } else if ms < CONFIRM_END_MS {
        let u = part(CONFIRM_BACK_MS, CONFIRM_END_MS);
        Some(ConfirmFrame { icon_scale: 0.6 + 0.4 * u, icon_alpha: u, check_alpha: 1.0 - u, ..NO_CONFIRM })
    } else {
        None
    }
}

fn confirm_ms(started: Instant) -> f32 {
    started.elapsed().as_secs_f32() * 1000.0
}

impl Editor {
    fn bar_height(&self) -> f32 {
        48.0 * self.scale
    }

    fn layout(&mut self) {
        let s = self.scale;
        let (client_w, _) = win::client_size(self.hwnd);
        let size = 36.0 * s;
        let top = (self.bar_height() - size) / 2.0;
        let step = 2.0 * s;
        self.btns.clear();
        self.separators.clear();

        let new_w = 32.0 * s + measure("Новый", Font::Ui, 13.0 * s).0 + 12.0 * s;
        self.btns.push(Btn {
            hit: Hit::New,
            r: RectF::xywh(8.0 * s, top, new_w, size),
            tip: "Новый снимок (Ctrl+N)",
        });

        // The actions on the right end where the window buttons of Windows begin.
        let mut right = client_w as f32 - win::CAPTION_BUTTONS_DIP * s - 8.0 * s;
        right -= size;
        self.btns.push(Btn { hit: Hit::Save, r: RectF::xywh(right, top, size, size), tip: "Сохранить как (Ctrl+S)" });
        right -= size + step;
        self.btns.push(Btn { hit: Hit::Copy, r: RectF::xywh(right, top, size, size), tip: "Копировать (Ctrl+C)" });
        right -= size + step;
        self.btns.push(Btn { hit: Hit::ScanText, r: RectF::xywh(right, top, size, size), tip: "Копировать текст со снимка" });
        right -= 56.0 * s + step;
        self.btns.push(Btn {
            hit: Hit::Fit,
            r: RectF::xywh(right, top, 56.0 * s, size),
            tip: "По размеру окна (Ctrl+0)",
        });
        right -= 8.0 * s;
        self.separators.push(right);
        right -= 8.0 * s + size;
        self.btns.push(Btn { hit: Hit::Redo, r: RectF::xywh(right, top, size, size), tip: "Вернуть (Ctrl+Y)" });
        right -= size + step;
        self.btns.push(Btn { hit: Hit::Undo, r: RectF::xywh(right, top, size, size), tip: "Отменить (Ctrl+Z)" });
        right -= size + step;
        self.btns.push(Btn { hit: Hit::EraseAll, r: RectF::xywh(right, top, size, size), tip: "Стереть все пометки" });

        let tools = [
            (Tool::Frame, "Рамка (R): клик обводит объект, протяжка рисует"),
            (Tool::Marker, "Маркер (H)"),
            (Tool::Pen, "Перо (P)"),
            (Tool::Eraser, "Ластик (X): удаляет пометку под курсором"),
            (Tool::Text, "Текст (T)"),
            (Tool::Blur, "Скрыть мозаикой (B)"),
            (Tool::Crop, "Обрезать (C)"),
            (Tool::Pointer, "Двигать и удалять (V)"),
        ];
        let shapes_w = 50.0 * s;
        let style_w = 50.0 * s;
        let group_w = tools.len() as f32 * (size + step) + shapes_w + step + 16.0 * s + style_w;
        // Centered in the room between the two sides.
        let room_l = 8.0 * s + new_w + 8.0 * s;
        let room_r = right - 8.0 * s;
        let mut x = (room_l + ((room_r - room_l - group_w) / 2.0).max(0.0)).round();
        for (i, (tool, tip)) in tools.into_iter().enumerate() {
            self.btns.push(Btn { hit: Hit::Tool(tool), r: RectF::xywh(x, top, size, size), tip });
            x += size + step;
            // Shapes come right after the frame, the two ways of marking something out.
            if i == 0 {
                let tip = "Фигуры: квадрат, овал, линия, стрелка. С Shift ровнее";
                self.btns.push(Btn { hit: Hit::Shapes, r: RectF::xywh(x, top, shapes_w, size), tip });
                x += shapes_w + step;
            }
        }
        self.separators.push(x + 6.0 * s);
        x += 14.0 * s;
        self.btns.push(Btn { hit: Hit::Style, r: RectF::xywh(x, top, style_w, size), tip: "Цвет и толщина" });
    }

    fn fit(&mut self) {
        let (w, h) = win::client_size(self.hwnd);
        let bar = self.bar_height();
        let pad = 24.0 * self.scale;
        let crop = self.doc.crop;
        let avail_w = (w as f32 - pad * 2.0).max(1.0);
        let avail_h = (h as f32 - bar - pad * 2.0).max(1.0);
        self.zoom = (avail_w / crop.w as f32).min(avail_h / crop.h as f32).min(1.0);
        self.origin = (
            (w as f32 - crop.w as f32 * self.zoom) / 2.0,
            bar + (h as f32 - bar - crop.h as f32 * self.zoom) / 2.0,
        );
        self.user_zoomed = false;
    }

    fn to_image(&self, x: i32, y: i32) -> P {
        (
            (x as f32 - self.origin.0) / self.zoom + self.doc.crop.x as f32,
            (y as f32 - self.origin.1) / self.zoom + self.doc.crop.y as f32,
        )
    }

    fn to_client(&self, r: RectF) -> RectF {
        let c = self.doc.crop;
        RectF::new(
            (r.l - c.x as f32) * self.zoom + self.origin.0,
            (r.t - c.y as f32) * self.zoom + self.origin.1,
            (r.r - c.x as f32) * self.zoom + self.origin.0,
            (r.b - c.y as f32) * self.zoom + self.origin.1,
        )
    }

    fn clamp_to_crop(&self, p: P) -> P {
        let c = self.doc.crop;
        (p.0.clamp(c.x as f32, c.right() as f32), p.1.clamp(c.y as f32, c.bottom() as f32))
    }

    fn line_width(&self) -> f32 {
        self.width
    }

    fn current_color(&self) -> u32 {
        self.color
    }

    fn set_color(&mut self, color: u32) {
        self.color = color;
        settings::update(|s| s.ink_color = color);
        if let Some(edit) = &mut self.edit {
            edit.color = color;
        }
    }

    fn set_width_from_x(&mut self, x: i32) {
        let Some(panel) = self.palette_rect() else {
            return;
        };
        let track = self.width_track(panel);
        let k = ((x as f32 - track.l) / track.w()).clamp(0.0, 1.0);
        self.width = (WIDTH_MIN + k * (WIDTH_MAX - WIDTH_MIN)).round();
        if let Some(edit) = &mut self.edit {
            edit.size = text_size(self.width);
        }
    }

    /// The buttons of the open list of shapes.
    fn shape_rects(&self) -> Vec<(Tool, RectF, &'static str)> {
        let s = self.scale;
        let Some(button) = self.btns.iter().find(|b| b.hit == Hit::Shapes) else {
            return Vec::new();
        };
        let size = 36.0 * s;
        let top = self.bar_height() + 6.0 * s;
        SHAPES
            .iter()
            .enumerate()
            .map(|(i, (tool, tip))| {
                (*tool, RectF::xywh(button.r.l + i as f32 * (size + 2.0 * s), top, size, size), *tip)
            })
            .collect()
    }

    fn palette_rect(&self) -> Option<RectF> {
        let s = self.scale;
        let style = self.btns.iter().find(|b| b.hit == Hit::Style)?;
        let (w, _) = win::client_size(self.hwnd);
        let width = 8.0 * 2.0 * s + PALETTE_COLUMNS as f32 * 34.0 * s;
        let left = (style.r.cx() - width / 2.0).clamp(8.0 * s, (w as f32 - width - 8.0 * s).max(8.0 * s));
        Some(RectF::xywh(left, self.bar_height() + 2.0 * s, width, 196.0 * s))
    }

    fn swatch_rects(&self, panel: RectF) -> Vec<(u32, RectF)> {
        let s = self.scale;
        PALETTE
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let col = (i % PALETTE_COLUMNS) as f32;
                let row = (i / PALETTE_COLUMNS) as f32;
                (*c, RectF::xywh(panel.l + 8.0 * s + col * 34.0 * s, panel.t + 8.0 * s + row * 34.0 * s, 34.0 * s, 34.0 * s))
            })
            .collect()
    }

    fn width_track(&self, panel: RectF) -> RectF {
        let s = self.scale;
        RectF::new(panel.l + 16.0 * s, panel.b - 34.0 * s, panel.r - 16.0 * s, panel.b - 30.0 * s)
    }

    /// The card of the question about saving, and its three buttons.
    fn save_question_rects(&self) -> (RectF, [RectF; 3]) {
        let s = self.scale;
        let (w, h) = win::client_size(self.hwnd);
        let (cw, ch) = (452.0 * s, 158.0 * s);
        let card = RectF::xywh((w as f32 - cw) / 2.0, (h as f32 - ch) / 2.0, cw, ch);
        let bw = (cw - 24.0 * s * 2.0 - 8.0 * s * 2.0) / 3.0;
        let top = card.b - 24.0 * s - 32.0 * s;
        let at = |i: f32| RectF::xywh(card.l + 24.0 * s + i * (bw + 8.0 * s), top, bw, 32.0 * s);
        (card, [at(0.0), at(1.0), at(2.0)])
    }

    fn answer_save_question(&mut self, choice: usize) {
        self.asking_to_save = false;
        win::invalidate(self.hwnd);
        let close = match choice {
            0 => self.save_as(),
            1 => true,
            _ => false,
        };
        if close {
            self.close_confirmed = true;
            unsafe {
                let _ = PostMessageW(Some(self.hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
            }
        }
    }

    fn after_edit(&mut self) {
        self.dirty = true;
        // Pasting right after an edit gives the edited picture.
        self.copy();
        win::invalidate(self.hwnd);
    }

    fn commit_item(&mut self, item: Item) {
        self.history.record(self.doc.clone());
        self.doc.items.push(item);
        self.after_edit();
    }

    fn commit_text(&mut self) {
        let Some(edit) = self.edit.take() else {
            return;
        };
        if edit.text.trim().is_empty() {
            win::invalidate(self.hwnd);
            return;
        }
        let extent = measure(&edit.text, Font::Note, edit.size);
        self.commit_item(Item::Text { at: edit.at, text: edit.text, color: edit.color, size: edit.size, extent });
    }

    fn shape_item(&self, a: P, b: P) -> Option<Item> {
        let b = if win::key_down(VK_SHIFT) { constrained_end(self.tool, a, b) } else { b };
        let r = RectF::new(a.0, a.1, b.0, b.1).normalized();
        let (color, w) = (self.current_color(), self.line_width());
        match self.tool {
            Tool::Frame | Tool::Square => Some(Item::Frame { r, color, w }),
            Tool::Ellipse => Some(Item::Ellipse { r, color, w }),
            Tool::Line => Some(Item::Line { a, b, color, w }),
            Tool::Arrow => Some(Item::Arrow { a, b, color, w }),
            Tool::Blur => Some(Item::Blur { r, mosaic: make_mosaic(&self.src, r) }),
            _ => None,
        }
    }

    fn stroke_item(&self, pts: Vec<P>) -> Item {
        let color = self.current_color();
        if self.tool == Tool::Marker {
            Item::Marker { pts, color, w: self.line_width() * MARKER_FACTOR }
        } else {
            Item::Pen { pts, color, w: self.line_width() }
        }
    }

    fn set_tool(&mut self, tool: Tool) {
        self.commit_text();
        self.tool = tool;
        if is_shape(tool) {
            self.shape = tool;
        }
        self.selected = None;
        self.hover_object = None;
        self.update_hover_object();
        win::invalidate(self.hwnd);
    }

    fn undo(&mut self) {
        self.commit_text();
        if let Some(doc) = self.history.undo(&self.doc) {
            self.apply_history(doc);
        }
    }

    fn redo(&mut self) {
        if let Some(doc) = self.history.redo(&self.doc) {
            self.apply_history(doc);
        }
    }

    fn apply_history(&mut self, doc: Doc) {
        self.doc = doc;
        self.selected = None;
        self.fit();
        self.after_edit();
    }

    /// Removes the topmost annotation at the point, if there is one.
    fn erase_at(&mut self, p: P) {
        let tol = 6.0 / self.zoom;
        let Some(i) = self.doc.items.iter().rposition(|item| item.hit(p, tol)) else {
            return;
        };
        if let Drag::Erase { before, erased } = &mut self.drag {
            if let Some(doc) = before.take() {
                self.history.record(doc);
            }
            *erased = true;
        }
        self.doc.items.remove(i);
        self.selected = None;
    }

    fn erase_all(&mut self) {
        self.commit_text();
        if !self.doc.items.is_empty() {
            self.history.record(self.doc.clone());
            self.doc.items.clear();
            self.selected = None;
            self.after_edit();
        }
    }

    fn delete_selected(&mut self) {
        if let Some(i) = self.selected.take() {
            self.history.record(self.doc.clone());
            self.doc.items.remove(i);
            self.after_edit();
        }
    }

    fn copy(&self) -> bool {
        match self.composite() {
            Ok(img) => match crate::clipboard::set_image(&img) {
                Ok(_) => true,
                Err(e) => {
                    crate::paths::log(&format!("clipboard: {e}"));
                    false
                }
            },
            Err(e) => {
                crate::paths::log(&format!("composite: {e}"));
                false
            }
        }
    }

    /// The copy the user asked for: the picture is safe on the clipboard, so closing needs no
    /// question until the next edit. The copies made after each edit do not count for that.
    fn copy_on_request(&mut self) {
        self.commit_text();
        if self.copy() {
            self.dirty = false;
            self.start_confirm(Hit::Copy);
        }
    }

    fn start_confirm(&mut self, hit: Hit) {
        self.confirming = Some((hit, Instant::now()));
        unsafe { SetTimer(Some(self.hwnd), CONFIRM_TIMER, 16, None) };
        win::invalidate(self.hwnd);
    }

    /// Flattens the picture and annotations into pixels.
    fn composite(&self) -> Result<Image> {
        let crop = self.doc.crop;
        if self.doc.items.is_empty() {
            return Ok(self.src.crop(crop));
        }
        let (w, h) = (crop.w as u32, crop.h as u32);
        unsafe {
            let wic: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
            let target = wic.CreateBitmap(w, h, &GUID_WICPixelFormat32bppPBGRA, WICBitmapCacheOnLoad)?;
            let props = D2D1_RENDER_TARGET_PROPERTIES {
                pixelFormat: D2D1_PIXEL_FORMAT {
                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                },
                dpiX: 96.0,
                dpiY: 96.0,
                ..Default::default()
            };
            let rt = gfx::d2d_factory().CreateWicBitmapRenderTarget(&target, &props)?;
            let p = Painter::new(rt)?;
            p.rt.BeginDraw();
            p.set_transform(1.0, -crop.x as f32, -crop.y as f32);
            let bmp = p.bitmap(&self.src)?;
            p.draw_bitmap(&bmp, RectF::from_rect(self.src.bounds()), None, false);
            for item in &self.doc.items {
                draw_item(&p, item);
            }
            p.rt.EndDraw(None, None)?;
            let mut pixels = vec![0u8; (w * h * 4) as usize];
            let area = WICRect { X: 0, Y: 0, Width: w as i32, Height: h as i32 };
            target.CopyPixels(&area, w * 4, &mut pixels)?;
            for px in pixels.chunks_exact_mut(4) {
                px[3] = 255;
            }
            Ok(Image::new(w, h, pixels))
        }
    }

    fn save_as(&mut self) -> bool {
        self.commit_text();
        let Some(path) = save_dialog(self.hwnd) else {
            return false;
        };
        match self.composite().map_err(|e| e.to_string()).and_then(|img| img.save_png(&path)) {
            Ok(()) => {
                self.dirty = false;
                self.start_confirm(Hit::Save);
                true
            }
            Err(e) => {
                win::show_error(&format!("Не удалось сохранить файл: {e}"));
                false
            }
        }
    }

    fn set_zoom(&mut self, zoom: f32, around: (i32, i32)) {
        let zoom = zoom.clamp(0.05, 8.0);
        let k = zoom / self.zoom;
        self.origin = (
            around.0 as f32 - (around.0 as f32 - self.origin.0) * k,
            around.1 as f32 - (around.1 as f32 - self.origin.1) * k,
        );
        self.zoom = zoom;
        self.user_zoomed = true;
        win::invalidate(self.hwnd);
    }

    fn update_hover_object(&mut self) {
        let over_canvas = self.mouse.1 as f32 >= self.bar_height() && self.mouse.0 >= 0;
        if self.tool != Tool::Frame || !matches!(self.drag, Drag::None) || !over_canvas {
            self.hover_object = None;
            return;
        }
        let (x, y) = self.to_image(self.mouse.0, self.mouse.1);
        self.hover_object = smart::detect(&self.src, self.doc.crop, x.floor() as i32, y.floor() as i32);
        self.last_detect = Instant::now();
    }

    fn press_button(&mut self, hit: Hit) {
        match hit {
            Hit::New => crate::app::request_capture(),
            Hit::Tool(t) => self.set_tool(t),
            // The first press picks the last used shape, the next one opens the list.
            Hit::Shapes => {
                if is_shape(self.tool) {
                    self.shapes_open = !self.shapes_open;
                } else {
                    self.set_tool(self.shape);
                    self.shapes_open = true;
                }
            }
            Hit::Style => self.palette_open = !self.palette_open,
            Hit::EraseAll => self.erase_all(),
            Hit::ScanText => {
                self.commit_text();
                match self.composite() {
                    Ok(img) => crate::ocr::copy_text(img),
                    Err(e) => crate::paths::log(&format!("composite: {e}")),
                }
            }
            Hit::Undo => self.undo(),
            Hit::Redo => self.redo(),
            Hit::Fit => self.fit(),
            Hit::Copy => self.copy_on_request(),
            Hit::Save => {
                self.save_as();
            }
        }
        win::invalidate(self.hwnd);
    }

    /// Handles a press while the palette is open. Any press outside only closes it.
    fn palette_press(&mut self, x: i32, y: i32) {
        let (fx, fy) = (x as f32, y as f32);
        let panel = self.palette_rect().filter(|p| p.contains(fx, fy));
        if let Some(panel) = panel {
            let swatch = self.swatch_rects(panel).into_iter().find(|(_, r)| r.contains(fx, fy));
            if let Some((color, _)) = swatch {
                self.set_color(color);
            } else if self.width_track(panel).inflate(10.0 * self.scale).contains(fx, fy) {
                self.dragging_width = true;
                self.set_width_from_x(x);
                unsafe { SetCapture(self.hwnd) };
            }
        } else {
            self.palette_open = false;
        }
        win::invalidate(self.hwnd);
    }

    fn on_left_down(&mut self, x: i32, y: i32) {
        if self.asking_to_save {
            return;
        }
        if self.palette_open {
            self.palette_press(x, y);
            return;
        }
        if self.shapes_open {
            let chosen = self.shape_rects().into_iter().find(|s| s.1.contains(x as f32, y as f32));
            self.shapes_open = false;
            if let Some((tool, _, _)) = chosen {
                self.set_tool(tool);
            }
            win::invalidate(self.hwnd);
            return;
        }
        if (y as f32) < self.bar_height() {
            self.pressed_btn = self.btn_at(x, y);
            return;
        }
        let was_editing = self.edit.is_some();
        self.commit_text();
        let raw = self.to_image(x, y);
        let p = self.clamp_to_crop(raw);
        unsafe { SetCapture(self.hwnd) };
        match self.tool {
            Tool::Pointer => {
                let tol = 4.0 / self.zoom;
                self.selected = self.doc.items.iter().rposition(|i| i.hit(raw, tol));
                if self.selected.is_some() {
                    self.drag = Drag::Move { last: raw, before: Some(self.doc.clone()) };
                }
            }
            Tool::Text => {
                if !was_editing {
                    self.edit = Some(TextEdit {
                        at: p,
                        text: String::new(),
                        color: self.current_color(),
                        size: text_size(self.width),
                    });
                }
            }
            Tool::Pen | Tool::Marker => self.drag = Drag::Stroke(vec![p]),
            Tool::Eraser => {
                self.drag = Drag::Erase { before: Some(self.doc.clone()), erased: false };
                self.erase_at(raw);
            }
            _ => self.drag = Drag::Shape { start: p, cur: p, down: (x, y), moved: false },
        }
        self.hover_object = if self.tool == Tool::Frame { self.hover_object } else { None };
        win::invalidate(self.hwnd);
    }

    fn on_mouse_move(&mut self, x: i32, y: i32) {
        self.mouse = (x, y);
        // Asks for WM_MOUSELEAVE, so hints do not stay behind when the mouse leaves.
        let mut track = TRACKMOUSEEVENT {
            cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
            dwFlags: TME_LEAVE,
            hwndTrack: self.hwnd,
            dwHoverTime: 0,
        };
        unsafe {
            let _ = TrackMouseEvent(&mut track);
        }
        if self.dragging_width {
            self.set_width_from_x(x);
            win::invalidate(self.hwnd);
            return;
        }
        if self.asking_to_save || self.palette_open || self.shapes_open {
            win::invalidate(self.hwnd);
            return;
        }
        let raw = self.to_image(x, y);
        let clamped = self.clamp_to_crop(raw);
        if matches!(self.drag, Drag::Erase { .. }) {
            self.erase_at(raw);
        }
        match &mut self.drag {
            Drag::None | Drag::Erase { .. } => {}
            Drag::Shape { cur, down, moved, .. } => {
                *cur = clamped;
                if (x - down.0).abs() > CLICK_SLOP || (y - down.1).abs() > CLICK_SLOP {
                    *moved = true;
                }
            }
            Drag::Stroke(pts) => {
                if pts.last().is_none_or(|l| (l.0 - clamped.0).abs() + (l.1 - clamped.1).abs() >= 1.5) {
                    pts.push(clamped);
                }
            }
            Drag::Move { last, before } => {
                let (dx, dy) = (raw.0 - last.0, raw.1 - last.1);
                *last = raw;
                if let Some(i) = self.selected {
                    if let Some(doc) = before.take() {
                        self.history.record(doc);
                    }
                    self.doc.items[i].translate(dx, dy, &self.src);
                }
            }
            Drag::Pan { last } => {
                self.origin.0 += (x - last.0) as f32;
                self.origin.1 += (y - last.1) as f32;
                *last = (x, y);
                self.user_zoomed = true;
            }
        }

        let hover = if (y as f32) < self.bar_height() { self.btn_at(x, y) } else { None };
        self.hover_btn = hover;

        if matches!(self.drag, Drag::None) && self.tool == Tool::Frame {
            // Pixel search is limited to about 30 times a second; the timer
            // catches the final position when the mouse stops.
            if self.last_detect.elapsed().as_millis() >= 33 {
                self.update_hover_object();
            } else {
                unsafe { SetTimer(Some(self.hwnd), HOVER_TIMER, 35, None) };
            }
        }
        win::invalidate(self.hwnd);
    }

    fn on_left_up(&mut self, x: i32, y: i32) {
        if self.asking_to_save {
            let (_, buttons) = self.save_question_rects();
            if let Some(i) = buttons.iter().position(|r| r.contains(x as f32, y as f32)) {
                self.answer_save_question(i);
            }
            return;
        }
        if self.dragging_width {
            self.dragging_width = false;
            let width = self.width;
            settings::update(|s| s.ink_width = width);
            unsafe {
                let _ = ReleaseCapture();
            }
            win::invalidate(self.hwnd);
            return;
        }
        if let Some(i) = self.pressed_btn.take() {
            if self.btn_at(x, y) == Some(i) {
                let hit = self.btns[i].hit;
                self.press_button(hit);
            }
            return;
        }
        unsafe {
            let _ = ReleaseCapture();
        }
        let drag = std::mem::replace(&mut self.drag, Drag::None);
        match drag {
            Drag::Shape { start, cur, moved, .. } => self.finish_shape(start, cur, moved),
            Drag::Stroke(pts) => {
                let item = self.stroke_item(pts);
                self.commit_item(item);
            }
            Drag::Move { before, .. } => {
                // `before` is taken on the first real movement.
                if before.is_none() {
                    self.after_edit();
                }
            }
            Drag::Erase { erased, .. } => {
                if erased {
                    self.after_edit();
                }
            }
            Drag::None | Drag::Pan { .. } => {}
        }
        win::invalidate(self.hwnd);
    }

    fn finish_shape(&mut self, start: P, cur: P, moved: bool) {
        if !moved {
            // A click with the frame tool outlines the object under the mouse.
            if self.tool == Tool::Frame {
                self.update_hover_object();
                if let Some(obj) = self.hover_object {
                    let pad = self.line_width() / 2.0 + 2.0;
                    let r = RectF::from_rect(obj).inflate(pad);
                    let (color, w) = (self.current_color(), self.line_width());
                    self.commit_item(Item::Frame { r, color, w });
                }
            }
            return;
        }
        if self.tool == Tool::Crop {
            let r = Rect::from_points(
                (start.0.round() as i32, start.1.round() as i32),
                (cur.0.round() as i32, cur.1.round() as i32),
            );
            if r.w >= 2 && r.h >= 2 {
                self.history.record(self.doc.clone());
                self.doc.crop = r;
                self.fit();
                self.after_edit();
            }
            return;
        }
        if let Some(item) = self.shape_item(start, cur) {
            self.commit_item(item);
        }
    }

    fn btn_at(&self, x: i32, y: i32) -> Option<usize> {
        self.btns.iter().position(|b| b.r.contains(x as f32, y as f32))
    }

    fn on_key(&mut self, vk: VIRTUAL_KEY) {
        if self.asking_to_save {
            match vk {
                VK_RETURN => self.answer_save_question(0),
                VK_ESCAPE => self.answer_save_question(2),
                _ => {}
            }
            return;
        }
        if (self.palette_open || self.shapes_open) && vk == VK_ESCAPE {
            self.palette_open = false;
            self.shapes_open = false;
            win::invalidate(self.hwnd);
            return;
        }
        let ctrl = win::key_down(VK_CONTROL);
        let shift = win::key_down(VK_SHIFT);
        if self.edit.is_some() && !ctrl {
            return; // Typing is handled in WM_CHAR.
        }
        if ctrl {
            match vk.0 as u8 as char {
                'Z' if shift => self.redo(),
                'Z' => self.undo(),
                'Y' => self.redo(),
                'C' => self.copy_on_request(),
                'S' => {
                    self.save_as();
                }
                '0' => self.fit(),
                'N' => crate::app::request_capture(),
                _ => {}
            }
            win::invalidate(self.hwnd);
            return;
        }
        match vk {
            VK_DELETE => self.delete_selected(),
            VK_ESCAPE => {
                self.drag = Drag::None;
                self.selected = None;
                unsafe {
                    let _ = ReleaseCapture();
                }
            }
            _ => {
                let tool = match vk.0 as u8 as char {
                    'V' => Some(Tool::Pointer),
                    'R' => Some(Tool::Frame),
                    'Q' => Some(Tool::Square),
                    'E' => Some(Tool::Ellipse),
                    'L' => Some(Tool::Line),
                    'A' => Some(Tool::Arrow),
                    'X' => Some(Tool::Eraser),
                    'H' => Some(Tool::Marker),
                    'P' => Some(Tool::Pen),
                    'T' => Some(Tool::Text),
                    'B' => Some(Tool::Blur),
                    'C' => Some(Tool::Crop),
                    _ => None,
                };
                if let Some(t) = tool {
                    self.set_tool(t);
                }
            }
        }
        win::invalidate(self.hwnd);
    }

    fn on_char(&mut self, c: u32) {
        let Some(edit) = &mut self.edit else {
            return;
        };
        match c {
            8 => {
                edit.text.pop();
            }
            27 => self.edit = None,
            13 => {
                if win::key_down(VK_SHIFT) {
                    edit.text.push('\n');
                } else {
                    self.commit_text();
                }
            }
            c if c >= 32 => {
                if let Some(ch) = char::from_u32(c) {
                    edit.text.push(ch);
                }
            }
            _ => {}
        }
        win::invalidate(self.hwnd);
    }

    fn paint(&mut self) {
        let Some(p) = self.surface.begin() else {
            return;
        };
        if self.bitmap.is_none() || self.generation != self.surface.generation {
            self.bitmap = p.bitmap(&self.src).ok();
            self.generation = self.surface.generation;
        }
        p.clear(TRANSPARENT);
        self.paint_canvas(&p);
        self.paint_bar(&p);
        if self.palette_open {
            self.paint_palette(&p);
        } else if self.shapes_open {
            self.paint_shape_list(&p);
        } else if !self.asking_to_save {
            self.paint_tooltip(&p);
        }
        if self.asking_to_save {
            self.paint_save_question(&p);
        }
        self.surface.end();
    }

    fn paint_canvas(&self, p: &Painter) {
        let crop = self.doc.crop;
        let (cw, ch) = win::client_size(self.hwnd);
        let area = RectF::new(0.0, self.bar_height(), cw as f32, ch as f32);
        p.fill(area, CANVAS_LAYER);
        // The toolbar is see-through, so nothing may be drawn under it when panning.
        p.push_clip(area);
        let picture = self.to_client(RectF::from_rect(crop));
        p.soft_shadow(picture, self.scale);
        p.set_transform(
            self.zoom,
            self.origin.0 - crop.x as f32 * self.zoom,
            self.origin.1 - crop.y as f32 * self.zoom,
        );
        p.push_clip(RectF::from_rect(crop));
        if let Some(bmp) = &self.bitmap {
            let smooth = self.zoom < 2.0 && self.zoom != 1.0;
            p.draw_bitmap(bmp, RectF::from_rect(self.src.bounds()), None, smooth);
        }
        for item in &self.doc.items {
            draw_item(p, item);
        }
        match &self.drag {
            Drag::Shape { start, cur, moved: true, .. } if self.tool != Tool::Crop => {
                if let Some(item) = self.shape_item(*start, *cur) {
                    draw_item(p, &item);
                }
            }
            Drag::Stroke(pts) => draw_item(p, &self.stroke_item(pts.clone())),
            _ => {}
        }
        if let Some(edit) = &self.edit {
            let r = RectF::xywh(edit.at.0, edit.at.1, 100_000.0, 100_000.0);
            p.text(&edit.text, r, Font::Note, edit.size, rgb(edit.color), Align::Left);
            let lines: Vec<&str> = edit.text.split('\n').collect();
            let line_h = measure("Ag", Font::Note, edit.size).1;
            let last = lines.last().copied().unwrap_or("");
            let cx = edit.at.0 + measure(last, Font::Note, edit.size).0 + 1.5;
            let cy = edit.at.1 + line_h * (lines.len() - 1) as f32;
            p.line((cx, cy + 2.0), (cx, cy + line_h - 2.0), rgb(edit.color), 2.0 / self.zoom);
        }
        p.pop_clip();
        p.reset_transform();

        if let Drag::Shape { start, cur, moved: true, .. } = &self.drag {
            if self.tool == Tool::Crop {
                self.paint_crop_preview(p, *start, *cur);
            }
        }
        if let Some(obj) = self.hover_object {
            let r = self.to_client(RectF::from_rect(obj)).inflate(3.0);
            p.fill(r, rgba(self.color, 0.10));
            p.stroke_dashed(r, rgb(self.color), 1.5);
        }
        // The eraser shows what a click would remove.
        if self.tool == Tool::Eraser && (self.mouse.1 as f32) >= self.bar_height() && self.mouse.0 >= 0 {
            let at = self.to_image(self.mouse.0, self.mouse.1);
            let tol = 6.0 / self.zoom;
            if let Some(item) = self.doc.items.iter().rev().find(|i| i.hit(at, tol)) {
                p.stroke_dashed(self.to_client(item.bounds()).inflate(6.0), RED, 1.5);
            }
        }
        if let Some(i) = self.selected {
            if let Some(item) = self.doc.items.get(i) {
                p.stroke_dashed(self.to_client(item.bounds()).inflate(6.0), accent(), 1.5);
            }
        }
        p.pop_clip();
    }

    fn paint_crop_preview(&self, p: &Painter, start: P, cur: P) {
        let r = self.to_client(RectF::new(start.0, start.1, cur.0, cur.1).normalized());
        let area = self.to_client(RectF::from_rect(self.doc.crop));
        let dim = rgba(0x000000, 0.55);
        p.fill(RectF::new(area.l, area.t, area.r, r.t), dim);
        p.fill(RectF::new(area.l, r.b, area.r, area.b), dim);
        p.fill(RectF::new(area.l, r.t, r.l, r.b), dim);
        p.fill(RectF::new(r.r, r.t, area.r, r.b), dim);
        p.stroke(r, rgb(0xffffff), 1.5);
    }

    fn paint_bar(&self, p: &Painter) {
        let s = self.scale;
        let radius = 4.0 * s;
        let bar = self.bar_height();
        for x in &self.separators {
            p.line((*x, 14.0 * s), (*x, bar - 14.0 * s), DIVIDER, 1.0);
        }
        for (i, b) in self.btns.iter().enumerate() {
            let hovered = self.hover_btn == Some(i) && !self.asking_to_save;
            match b.hit {
                Hit::New => {
                    if hovered {
                        p.round(b.r, radius, HOVER);
                    }
                    p.icon('\u{E710}', b.r.l + 16.0 * s, b.r.cy(), 14.0 * s, TEXT);
                    let text = RectF::new(b.r.l + 32.0 * s, b.r.t, b.r.r, b.r.b);
                    p.text("Новый", text, Font::Ui, 13.0 * s, TEXT, Align::Left);
                }
                Hit::Tool(tool) => {
                    let active = self.tool == tool;
                    if active {
                        p.round(b.r, radius, accent());
                    } else if hovered {
                        p.round(b.r, radius, HOVER);
                    }
                    paint_tool_icon(p, tool, b.r, s, if active { ON_ACCENT } else { TEXT });
                }
                Hit::Shapes => {
                    let active = is_shape(self.tool);
                    if active {
                        p.round(b.r, radius, accent());
                    } else if hovered || self.shapes_open {
                        p.round(b.r, radius, HOVER);
                    }
                    let c = if active { ON_ACCENT } else { TEXT };
                    let icon = RectF::xywh(b.r.l, b.r.t, b.r.h(), b.r.h());
                    paint_tool_icon(p, self.shape, icon, s * 0.9, c);
                    p.icon('\u{E70D}', b.r.r - 10.0 * s, b.r.cy(), 8.0 * s, c);
                }
                Hit::EraseAll => {
                    let enabled = !self.doc.items.is_empty();
                    if hovered && enabled {
                        p.round(b.r, radius, HOVER);
                    }
                    p.icon('\u{E74D}', b.r.cx(), b.r.cy(), 16.0 * s, if enabled { TEXT } else { TEXT_OFF });
                }
                Hit::ScanText => {
                    if hovered {
                        p.round(b.r, radius, HOVER);
                    }
                    crate::overlay::paint_scan_text_icon(p, b.r.cx(), b.r.cy(), s, TEXT);
                }
                Hit::Style => {
                    if hovered || self.palette_open {
                        p.round(b.r, radius, HOVER);
                    }
                    // The circle shows both settings: its color and, roughly, the line width.
                    let dot = 3.0 + (self.width - WIDTH_MIN) / (WIDTH_MAX - WIDTH_MIN) * 6.5;
                    let cx = b.r.l + 18.0 * s;
                    p.fill_circle(cx, b.r.cy(), dot * s, rgb(self.color));
                    if self.color == 0x000000 {
                        let ring = RectF::xywh(cx - dot * s, b.r.cy() - dot * s, dot * 2.0 * s, dot * 2.0 * s);
                        p.ellipse(ring, TEXT_DIM, 1.0);
                    }
                    p.icon('\u{E70D}', b.r.r - 13.0 * s, b.r.cy(), 9.0 * s, TEXT_DIM);
                }
                Hit::Undo | Hit::Redo => {
                    let enabled = match b.hit {
                        Hit::Undo => !self.history.undo.is_empty(),
                        _ => !self.history.redo.is_empty(),
                    };
                    if hovered && enabled {
                        p.round(b.r, radius, HOVER);
                    }
                    let glyph = if b.hit == Hit::Undo { '\u{E7A7}' } else { '\u{E7A6}' };
                    p.icon(glyph, b.r.cx(), b.r.cy(), 16.0 * s, if enabled { TEXT } else { TEXT_OFF });
                }
                Hit::Fit => {
                    if hovered {
                        p.round(b.r, radius, HOVER);
                    }
                    let label = format!("{}%", (self.zoom * 100.0).round() as i32);
                    p.text(&label, b.r, Font::Ui, 12.5 * s, TEXT_DIM, Align::Center);
                }
                Hit::Copy | Hit::Save => {
                    if hovered {
                        p.round(b.r, radius, HOVER);
                    }
                    let glyph = if b.hit == Hit::Copy { '\u{E8C8}' } else { '\u{E74E}' };
                    let frame = self
                        .confirming
                        .as_ref()
                        .filter(|c| c.0 == b.hit)
                        .and_then(|c| confirm_frame(confirm_ms(c.1)))
                        .unwrap_or(NO_CONFIRM);
                    let size = 16.0 * s;
                    if frame.check_alpha > 0.0 {
                        p.round(b.r, radius, Color { a: 0.2 * frame.check_alpha, ..accent() });
                        let c = Color { a: frame.check_alpha, ..accent() };
                        p.icon('\u{E73E}', b.r.cx(), b.r.cy(), size * frame.check_scale, c);
                    }
                    if frame.icon_alpha > 0.0 {
                        let c = Color { a: frame.icon_alpha, ..TEXT };
                        p.icon(glyph, b.r.cx(), b.r.cy(), size * frame.icon_scale, c);
                    }
                }
            }
        }
    }

    fn paint_tooltip(&self, p: &Painter) {
        let Some(i) = self.hover_btn else {
            return;
        };
        let s = self.scale;
        let (w, _) = win::client_size(self.hwnd);
        let b = &self.btns[i];
        let size = 12.0 * s;
        let tw = measure(b.tip, Font::Ui, size).0 + 18.0 * s;
        let left = (b.r.cx() - tw / 2.0).clamp(6.0 * s, (w as f32 - tw - 6.0 * s).max(6.0 * s));
        let tip = RectF::xywh(left, self.bar_height() + 4.0 * s, tw, 28.0 * s);
        p.round(tip, 4.0 * s, FLYOUT);
        p.round_stroke(tip, 4.0 * s, FLYOUT_STROKE, 1.0);
        p.text(b.tip, tip, Font::Ui, size, TEXT, Align::Center);
    }

    fn paint_shape_list(&self, p: &Painter) {
        let s = self.scale;
        let rects = self.shape_rects();
        let (Some(first), Some(last)) = (rects.first(), rects.last()) else {
            return;
        };
        let panel = RectF::new(first.1.l - 4.0 * s, first.1.t - 4.0 * s, last.1.r + 4.0 * s, last.1.b + 4.0 * s);
        p.soft_shadow(panel, s);
        p.round(panel, 8.0 * s, FLYOUT);
        p.round_stroke(panel, 8.0 * s, FLYOUT_STROKE, 1.0);
        let (mx, my) = (self.mouse.0 as f32, self.mouse.1 as f32);
        let mut hovered_tip = None;
        for (tool, r, tip) in &rects {
            let active = self.tool == *tool;
            if active {
                p.round(*r, 4.0 * s, accent());
            } else if r.contains(mx, my) {
                p.round(*r, 4.0 * s, HOVER);
            }
            if r.contains(mx, my) {
                hovered_tip = Some(*tip);
            }
            paint_tool_icon(p, *tool, *r, s, if active { ON_ACCENT } else { TEXT });
        }
        if let Some(text) = hovered_tip {
            let size = 12.0 * s;
            let tw = measure(text, Font::Ui, size).0 + 18.0 * s;
            let tip = RectF::xywh(panel.l, panel.b + 4.0 * s, tw, 28.0 * s);
            p.round(tip, 4.0 * s, FLYOUT);
            p.round_stroke(tip, 4.0 * s, FLYOUT_STROKE, 1.0);
            p.text(text, tip, Font::Ui, size, TEXT, Align::Center);
        }
    }

    fn paint_palette(&self, p: &Painter) {
        let Some(panel) = self.palette_rect() else {
            return;
        };
        let s = self.scale;
        p.soft_shadow(panel, s);
        p.round(panel, 8.0 * s, FLYOUT);
        p.round_stroke(panel, 8.0 * s, FLYOUT_STROKE, 1.0);
        let (mx, my) = (self.mouse.0 as f32, self.mouse.1 as f32);
        for (color, r) in self.swatch_rects(panel) {
            let selected = color == self.color;
            if selected {
                p.ellipse(r.inflate(-3.0 * s), TEXT, 1.5 * s);
            } else if r.contains(mx, my) {
                p.ellipse(r.inflate(-3.0 * s), TEXT_DIM, 1.0 * s);
            }
            p.fill_circle(r.cx(), r.cy(), 10.5 * s, rgb(color));
            if color == 0x000000 {
                p.ellipse(r.inflate(-6.5 * s), rgba(0xffffff, 0.3), 1.0);
            }
        }
        let track = self.width_track(panel);
        let label = RectF::new(track.l, track.t - 28.0 * s, track.r, track.t - 6.0 * s);
        p.text("Толщина", label, Font::Ui, 12.5 * s, TEXT_DIM, Align::Left);
        let value = format!("{} px", self.width as i32);
        let vw = measure(&value, Font::Ui, 12.5 * s).0;
        let value_r = RectF::new(track.r - vw, label.t, track.r, label.b);
        p.text(&value, value_r, Font::Ui, 12.5 * s, TEXT, Align::Left);
        let k = (self.width - WIDTH_MIN) / (WIDTH_MAX - WIDTH_MIN);
        let knob = track.l + k * track.w();
        p.round(track, 2.0 * s, rgba(0xffffff, 0.3));
        p.round(RectF::new(track.l, track.t, knob, track.b), 2.0 * s, accent());
        p.fill_circle(knob, track.cy(), 10.0 * s, rgb(0x454545));
        p.fill_circle(knob, track.cy(), 6.0 * s, accent());
    }

    fn paint_save_question(&self, p: &Painter) {
        let s = self.scale;
        let (w, h) = win::client_size(self.hwnd);
        p.fill(RectF::xywh(0.0, 0.0, w as f32, h as f32), rgba(0x000000, 0.45));
        let (card, buttons) = self.save_question_rects();
        p.soft_shadow(card, s);
        p.round(card, 8.0 * s, FLYOUT);
        p.round_stroke(card, 8.0 * s, FLYOUT_STROKE, 1.0);
        let left = card.l + 24.0 * s;
        let title = RectF::new(left, card.t + 18.0 * s, card.r, card.t + 48.0 * s);
        p.text("Сохранить изменения?", title, Font::UiBold, 18.0 * s, TEXT, Align::Left);
        let text = RectF::new(left, card.t + 50.0 * s, card.r, card.t + 76.0 * s);
        p.text("Правки сейчас только в буфере обмена.", text, Font::Ui, 13.0 * s, TEXT_DIM, Align::Left);
        let (mx, my) = (self.mouse.0 as f32, self.mouse.1 as f32);
        for (i, label) in ["Сохранить", "Не сохранять", "Отмена"].into_iter().enumerate() {
            let r = buttons[i];
            let hovered = r.contains(mx, my);
            if i == 0 {
                p.round(r, 4.0 * s, if hovered { Color { a: 0.9, ..accent() } } else { accent() });
                p.text(label, r, Font::Ui, 13.0 * s, ON_ACCENT, Align::Center);
            } else {
                p.round(r, 4.0 * s, if hovered { CONTROL_HOVER } else { CONTROL });
                p.round_stroke(r, 4.0 * s, CONTROL_STROKE, 1.0);
                p.text(label, r, Font::Ui, 13.0 * s, TEXT, Align::Center);
            }
        }
    }
}

fn text_size(width: f32) -> f32 {
    14.0 + width * 2.4
}

fn paint_tool_icon(p: &Painter, tool: Tool, r: RectF, s: f32, c: Color) {
    let (cx, cy) = (r.cx(), r.cy());
    let u = 7.5 * s;
    let w = 1.25 * s;
    match tool {
        Tool::Frame => {
            // Corner brackets around a dot: "frame the object".
            let t = 5.0 * s;
            for (sx, sy) in [(-1.0, -1.0), (1.0, -1.0), (-1.0, 1.0), (1.0, 1.0)] {
                let corner = (cx + sx * u, cy + sy * u);
                p.line(corner, (corner.0 - sx * t, corner.1), c, w);
                p.line(corner, (corner.0, corner.1 - sy * t), c, w);
            }
            p.fill_circle(cx, cy, 2.2 * s, c);
        }
        Tool::Square => p.round_stroke(RectF::new(cx - u * 0.9, cy - u * 0.9, cx + u * 0.9, cy + u * 0.9), 1.5 * s, c, w),
        Tool::Line => p.line((cx - u, cy + u), (cx + u, cy - u), c, w),
        Tool::Eraser => p.icon('\u{E75C}', cx, cy, 16.0 * s, c),
        Tool::Ellipse => p.ellipse(RectF::new(cx - u, cy - u * 0.75, cx + u, cy + u * 0.75), c, w),
        Tool::Arrow => draw_arrow(p, (cx - u, cy + u), (cx + u, cy - u), c, w),
        Tool::Marker => p.icon('\u{E7E6}', cx, cy, 16.0 * s, c),
        Tool::Pen => p.icon('\u{E70F}', cx, cy, 16.0 * s, c),
        Tool::Text => p.icon('\u{E8D2}', cx, cy, 16.0 * s, c),
        Tool::Crop => p.icon('\u{E7A8}', cx, cy, 16.0 * s, c),
        Tool::Blur => {
            let cell = 4.6 * s;
            let alphas = [1.0, 0.45, 0.8, 0.5, 0.95, 0.35, 0.8, 0.4, 1.0];
            for (i, a) in alphas.into_iter().enumerate() {
                let (col, row) = ((i % 3) as f32 - 1.5, (i / 3) as f32 - 1.5);
                let cell_r = RectF::xywh(cx + col * cell, cy + row * cell, cell - 0.6, cell - 0.6);
                p.fill(cell_r, Color { a, ..c });
            }
        }
        Tool::Pointer => {
            let pts = [
                (cx - 5.0 * s, cy - 8.0 * s),
                (cx - 5.0 * s, cy + 7.0 * s),
                (cx - 1.0 * s, cy + 3.5 * s),
                (cx + 1.8 * s, cy + 9.0 * s),
                (cx + 4.0 * s, cy + 8.0 * s),
                (cx + 1.2 * s, cy + 2.5 * s),
                (cx + 6.0 * s, cy + 2.5 * s),
            ];
            p.fill_polygon(&pts, c);
        }
    }
}

fn save_dialog(owner: HWND) -> Option<PathBuf> {
    unsafe {
        let dialog: IFileSaveDialog = CoCreateInstance(&FileSaveDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        let name = w!("Изображение PNG");
        let spec = w!("*.png");
        let filter = [COMDLG_FILTERSPEC { pszName: name, pszSpec: spec }];
        dialog.SetFileTypes(&filter).ok()?;
        dialog.SetDefaultExtension(w!("png")).ok()?;
        let file = win::wide(&format!("Снимок экрана {}.png", crate::paths::timestamp()));
        dialog.SetFileName(PCWSTR(file.as_ptr())).ok()?;
        let dir = crate::paths::screenshots_dir();
        let _ = std::fs::create_dir_all(&dir);
        let dir_w = win::wide(&dir.to_string_lossy());
        let folder: Result<IShellItem> = SHCreateItemFromParsingName(PCWSTR(dir_w.as_ptr()), None);
        if let Ok(folder) = folder {
            let _ = dialog.SetFolder(&folder);
        }
        dialog.Show(Some(owner)).ok()?;
        let item = dialog.GetResult().ok()?;
        let path = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let result = path.to_string().ok().map(PathBuf::from);
        CoTaskMemFree(Some(path.0 as _));
        result
    }
}

impl WndState for Editor {
    fn on_message(&mut self, hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
        let caption = self.bar_height() as i32;
        let on_control = |x: i32, y: i32| self.btn_at(x, y).is_some();
        if let Some(r) = win::frame_message(hwnd, msg, wp, lp, caption, true, |x, y| !on_control(x, y)) {
            return Some(r);
        }
        match msg {
            WM_CREATE => {
                self.hwnd = hwnd;
                self.surface = Surface::glass(hwnd);
                self.scale = win::dpi_scale(hwnd);
                self.layout();
                self.fit();
                Some(LRESULT(0))
            }
            WM_ERASEBKGND => Some(LRESULT(1)),
            WM_PAINT => {
                self.paint();
                unsafe {
                    let _ = ValidateRect(Some(hwnd), None);
                }
                Some(LRESULT(0))
            }
            WM_SIZE => {
                self.surface.resize();
                self.layout();
                if !self.user_zoomed {
                    self.fit();
                }
                win::invalidate(hwnd);
                Some(LRESULT(0))
            }
            WM_DPICHANGED => {
                self.scale = ((wp.0 >> 16) & 0xffff) as f32 / 96.0;
                let r = unsafe { &*(lp.0 as *const RECT) };
                unsafe {
                    let _ = SetWindowPos(
                        hwnd,
                        None,
                        r.left,
                        r.top,
                        r.right - r.left,
                        r.bottom - r.top,
                        SWP_NOZORDER | SWP_NOACTIVATE,
                    );
                }
                self.layout();
                Some(LRESULT(0))
            }
            WM_GETMINMAXINFO => {
                let info = unsafe { &mut *(lp.0 as *mut MINMAXINFO) };
                info.ptMinTrackSize = POINT { x: (MIN_WIDTH_DIP * self.scale) as i32, y: (420.0 * self.scale) as i32 };
                Some(LRESULT(0))
            }
            WM_LBUTTONDOWN => {
                let (x, y) = win::mouse_xy(lp);
                self.on_left_down(x, y);
                Some(LRESULT(0))
            }
            WM_MOUSEMOVE => {
                let (x, y) = win::mouse_xy(lp);
                self.on_mouse_move(x, y);
                Some(LRESULT(0))
            }
            WM_LBUTTONUP => {
                let (x, y) = win::mouse_xy(lp);
                self.on_left_up(x, y);
                Some(LRESULT(0))
            }
            WM_MOUSELEAVE => {
                self.mouse = (-1, -1);
                self.hover_object = None;
                self.hover_btn = None;
                win::invalidate(hwnd);
                Some(LRESULT(0))
            }
            WM_MBUTTONDOWN => {
                self.drag = Drag::Pan { last: win::mouse_xy(lp) };
                unsafe { SetCapture(hwnd) };
                Some(LRESULT(0))
            }
            WM_MBUTTONUP => {
                self.drag = Drag::None;
                unsafe {
                    let _ = ReleaseCapture();
                }
                Some(LRESULT(0))
            }
            WM_MOUSEWHEEL => {
                let delta = ((wp.0 >> 16) & 0xffff) as u16 as i16 as f32;
                let (sx, sy) = win::mouse_xy(lp);
                let mut pt = POINT { x: sx, y: sy };
                unsafe {
                    let _ = ScreenToClient(hwnd, &mut pt);
                }
                if win::key_down(VK_CONTROL) {
                    self.set_zoom(self.zoom * (1.0 + delta / 120.0 * 0.15), (pt.x, pt.y));
                } else {
                    let step = delta / 120.0 * 60.0 * self.scale;
                    if win::key_down(VK_SHIFT) {
                        self.origin.0 += step;
                    } else {
                        self.origin.1 += step;
                    }
                    self.user_zoomed = true;
                    win::invalidate(hwnd);
                }
                Some(LRESULT(0))
            }
            WM_TIMER if wp.0 == HOVER_TIMER => {
                unsafe {
                    let _ = KillTimer(Some(hwnd), HOVER_TIMER);
                }
                self.update_hover_object();
                win::invalidate(hwnd);
                Some(LRESULT(0))
            }
            WM_TIMER if wp.0 == CONFIRM_TIMER => {
                let running = self.confirming.as_ref().is_some_and(|c| confirm_frame(confirm_ms(c.1)).is_some());
                if !running {
                    self.confirming = None;
                    unsafe {
                        let _ = KillTimer(Some(hwnd), CONFIRM_TIMER);
                    }
                }
                win::invalidate(hwnd);
                Some(LRESULT(0))
            }
            WM_KEYDOWN => {
                self.on_key(VIRTUAL_KEY(wp.0 as u16));
                Some(LRESULT(0))
            }
            WM_CHAR => {
                self.on_char(wp.0 as u32);
                Some(LRESULT(0))
            }
            WM_SETCURSOR if (lp.0 & 0xffff) as u32 == HTCLIENT => {
                let on_bar = (self.mouse.1 as f32) < self.bar_height();
                let id = match self.tool {
                    _ if on_bar => IDC_ARROW,
                    Tool::Pointer => IDC_ARROW,
                    Tool::Text => IDC_IBEAM,
                    _ => IDC_CROSS,
                };
                unsafe {
                    if let Ok(c) = LoadCursorW(None, id) {
                        SetCursor(Some(c));
                    }
                }
                Some(LRESULT(1))
            }
            WM_CLOSE => {
                self.commit_text();
                if self.dirty && !self.close_confirmed {
                    self.asking_to_save = true;
                    self.palette_open = false;
                    win::invalidate(hwnd);
                    return Some(LRESULT(0));
                }
                None
            }
            WM_DESTROY => {
                crate::app::window_closed();
                None
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confirmation_swaps_the_icon_for_a_check_and_brings_it_back() {
        let first = confirm_frame(0.0).unwrap();
        assert_eq!((first.icon_alpha, first.check_alpha), (1.0, 0.0));
        let held = confirm_frame(CONFIRM_POP_MS + 1.0).unwrap();
        assert_eq!((held.icon_alpha, held.check_alpha, held.check_scale), (0.0, 1.0, 1.0));
        let last = confirm_frame(CONFIRM_END_MS - 1.0).unwrap();
        assert!(last.icon_alpha > 0.9 && last.check_alpha < 0.1);
        assert!(confirm_frame(CONFIRM_END_MS).is_none());
    }

    #[test]
    fn check_mark_pops_past_its_size_before_settling() {
        let largest = (CONFIRM_OUT_MS as u32..CONFIRM_POP_MS as u32)
            .map(|ms| confirm_frame(ms as f32).unwrap().check_scale)
            .fold(0.0, f32::max);
        assert!(largest > 1.02 && largest < 1.2, "{largest}");
    }

    #[test]
    fn undo_and_redo_walk_through_states() {
        let mut h = History::new();
        let mut state = 1;
        h.record(state);
        state = 2;
        h.record(state);
        state = 3;
        state = h.undo(&state).unwrap();
        assert_eq!(state, 2);
        state = h.undo(&state).unwrap();
        assert_eq!(state, 1);
        assert_eq!(h.undo(&state), None);
        state = h.redo(&state).unwrap();
        assert_eq!(state, 2);
    }

    #[test]
    fn a_new_change_clears_redo() {
        let mut h = History::new();
        h.record(1);
        let state = h.undo(&2).unwrap();
        assert_eq!(state, 1);
        h.record(state);
        assert_eq!(h.redo(&5), None);
    }

    #[test]
    fn frame_is_hit_on_its_line_but_not_in_the_middle() {
        let item = Item::Frame { r: RectF::new(100.0, 100.0, 300.0, 200.0), color: 0, w: 4.0 };
        assert!(item.hit((100.0, 150.0), 4.0));
        assert!(item.hit((200.0, 201.0), 4.0));
        assert!(!item.hit((200.0, 150.0), 4.0));
        assert!(!item.hit((50.0, 150.0), 4.0));
    }

    #[test]
    fn line_is_hit_near_it_and_moves_with_both_ends() {
        let src = Image::new(1, 1, vec![0, 0, 0, 255]);
        let mut item = Item::Line { a: (0.0, 0.0), b: (100.0, 0.0), color: 0, w: 2.0 };
        assert!(item.hit((50.0, 3.0), 2.0));
        assert!(!item.hit((50.0, 30.0), 2.0));
        item.translate(10.0, 5.0, &src);
        assert_eq!(item.bounds(), RectF::new(10.0, 5.0, 110.0, 5.0));
    }

    #[test]
    fn shift_makes_squares_equal_in_any_direction() {
        assert_eq!(constrained_end(Tool::Square, (10.0, 10.0), (110.0, 40.0)), (110.0, 110.0));
        assert_eq!(constrained_end(Tool::Square, (10.0, 10.0), (-90.0, 0.0)), (-90.0, -90.0));
        assert_eq!(constrained_end(Tool::Ellipse, (0.0, 0.0), (5.0, -20.0)), (20.0, -20.0));
    }

    #[test]
    fn shift_snaps_lines_to_steps_of_45_degrees() {
        let near = |a: P, b: P| (a.0 - b.0).abs() < 0.01 && (a.1 - b.1).abs() < 0.01;
        let (x, y) = constrained_end(Tool::Line, (0.0, 0.0), (100.0, 4.0));
        assert!(near((x, y), ((100.0f32 * 100.0 + 16.0).sqrt(), 0.0)), "{x},{y}");
        let (x, y) = constrained_end(Tool::Arrow, (0.0, 0.0), (3.0, 100.0));
        assert!(x.abs() < 0.01 && y > 99.0, "{x},{y}");
        let (x, y) = constrained_end(Tool::Line, (0.0, 0.0), (100.0, 90.0));
        assert!((x - y).abs() < 0.01, "{x},{y}");
    }

    #[test]
    fn arrow_is_hit_near_the_line() {
        let item = Item::Arrow { a: (0.0, 0.0), b: (100.0, 0.0), color: 0, w: 2.0 };
        assert!(item.hit((50.0, 3.0), 2.0));
        assert!(!item.hit((50.0, 30.0), 2.0));
    }

    #[test]
    fn mosaic_averages_each_cell() {
        // Left half black, right half white, 24x12: exactly two cells.
        let mut px = Vec::new();
        for _ in 0..12 {
            for x in 0..24 {
                let v = if x < 12 { 0 } else { 255 };
                px.extend_from_slice(&[v, v, v, 255]);
            }
        }
        let src = Image::new(24, 12, px);
        let m = make_mosaic(&src, RectF::new(0.0, 0.0, 24.0, 12.0));
        assert_eq!((m.w, m.h), (2, 1));
        assert_eq!(m.px(0, 0), [0, 0, 0]);
        assert_eq!(m.px(1, 0), [255, 255, 255]);
    }

    #[test]
    fn moving_an_item_shifts_its_bounds() {
        let src = Image::new(1, 1, vec![0, 0, 0, 255]);
        let mut item = Item::Pen { pts: vec![(10.0, 10.0), (20.0, 30.0)], color: 0, w: 2.0 };
        item.translate(5.0, -5.0, &src);
        assert_eq!(item.bounds(), RectF::new(15.0, 5.0, 25.0, 25.0));
    }
}
