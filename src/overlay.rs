//! The full-screen layer that opens on a double Print press: a frozen picture of
//! the desktop, a toolbar, and the selection of what to capture.

use crate::capture::{self, Monitor};
use crate::color::format_color;
use crate::delay::CHOICES;
use crate::geom::{self, Rect};
use crate::gfx::*;
use crate::image::Image;
use crate::settings;
use crate::win::{self, WM_KADR_OVERLAY_DONE, WindowSpec, WndState};
use std::cell::{Cell, RefCell};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct2D::ID2D1Bitmap;
use windows::Win32::Graphics::Gdi::ValidateRect;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::*;

const CLASS: PCWSTR = w!("KadrOverlay");
const DIM: Color = rgba(0x000000, 0.5);
/// A drag shorter than this is treated as an accidental click.
const MIN_DRAG: i32 = 6;
/// Presses this soon after opening are the tail of an impatient double press, not a
/// request for the whole screen.
const FULL_SCREEN_GUARD: Duration = Duration::from_millis(600);
const START_WIDTH_DIP: f32 = 84.0;

/// How a video chooses what to record. A picture stays on `Area`, because its window and
/// screen buttons capture at once.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Shape {
    Area,
    Window,
    Full,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Photo,
    Video,
    /// Copies the text in the chosen area.
    Text,
    /// Copies the color under the mouse.
    Color,
}

pub enum Outcome {
    Cancel,
    OpenSettings,
    Text { img: Image },
    Color { value: String, rgb: [u8; 3] },
    Delay { seconds: u32, mode: Mode },
    Photo { img: Image },
    Video { monitor: Monitor, region: Rect, thumb: Image, mic: bool, system: bool },
}

thread_local! {
    static OUTCOME: RefCell<Option<Outcome>> = const { RefCell::new(None) };
    static CURRENT: RefCell<Option<HWND>> = const { RefCell::new(None) };
    static OPENED_AT: Cell<Option<Instant>> = const { Cell::new(None) };
}

pub fn take_outcome() -> Option<Outcome> {
    OUTCOME.with(|o| o.borrow_mut().take())
}

pub fn is_open() -> bool {
    CURRENT.with(|c| c.borrow().is_some())
}

fn past_guard(opened: Instant, now: Instant) -> bool {
    now.saturating_duration_since(opened) >= FULL_SCREEN_GUARD
}

/// The hotkey pressed once more while the overlay is open: take the whole screen.
pub fn capture_full_screen() {
    if !OPENED_AT.with(|o| o.get()).is_some_and(|opened| past_guard(opened, Instant::now())) {
        return;
    }
    if let Some(hwnd) = CURRENT.with(|c| *c.borrow()) {
        unsafe {
            let _ = PostMessageW(Some(hwnd), WM_KEYDOWN, WPARAM(VK_RETURN.0 as usize), LPARAM(0));
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Btn {
    Photo,
    Video,
    Text,
    Color,
    Area,
    Window,
    Full,
    Delay,
    Mic,
    System,
    /// Starts a video, which never begins by itself when something is selected.
    Start,
    Settings,
    Close,
}

struct Item {
    btn: Btn,
    rect: RectF,
}

struct Overlay {
    hwnd: HWND,
    notify: HWND,
    surface: Surface,
    frame: Image,
    bitmap: Option<ID2D1Bitmap>,
    bitmap_generation: u64,
    screen: Rect,
    monitors: Vec<Monitor>,
    windows: Vec<Rect>,
    active_window: Option<Rect>,
    mode: Mode,
    shape: Shape,
    /// What a video will record once it is started, in screen coordinates.
    selection: Option<Rect>,
    mic: bool,
    system: bool,
    mouse: (i32, i32),
    drag_from: Option<(i32, i32)>,
    hover: Option<Btn>,
    pressed_toolbar: bool,
    delay_open: bool,
    bar: RectF,
    separators: Vec<f32>,
    items: Vec<Item>,
    scale: f32,
    home: Monitor,
}

/// What the window button takes: the active window, otherwise the topmost one on this monitor.
fn pick_window(active: Option<Rect>, windows: &[Rect], monitor: Rect) -> Option<Rect> {
    active.or_else(|| {
        windows.iter().copied().find(|w| {
            let (x, y) = w.center();
            monitor.contains(x, y)
        })
    })
}

/// A press that barely moved: a click rather than a drag.
fn is_click(from: (i32, i32), to: (i32, i32)) -> bool {
    (to.0 - from.0).abs() < MIN_DRAG && (to.1 - from.1).abs() < MIN_DRAG
}

pub fn open(mode: Mode, notify: HWND) {
    if is_open() {
        return;
    }
    let screen = capture::virtual_screen();
    let frame = match capture::grab(screen) {
        Ok(f) => f,
        Err(e) => {
            win::show_error(&format!("Не удалось снять экран: {e}"));
            return;
        }
    };
    let monitors = capture::monitors();
    let windows = capture::visible_windows();
    let active_window = capture::active_window();
    let (cx, cy) = win::cursor_pos();
    let Some(home) = capture::monitor_at(&monitors, cx, cy) else {
        return;
    };
    refresh_accent();
    let s = settings::get();
    let state = Overlay {
        hwnd: HWND::default(),
        notify,
        surface: Surface::new(HWND::default()),
        frame,
        bitmap: None,
        bitmap_generation: 0,
        screen,
        monitors,
        windows,
        active_window,
        mode,
        shape: Shape::Area,
        selection: None,
        mic: s.mic,
        system: s.system_audio,
        mouse: (cx - screen.x, cy - screen.y),
        drag_from: None,
        hover: None,
        pressed_toolbar: false,
        delay_open: false,
        bar: RectF::default(),
        separators: Vec::new(),
        items: Vec::new(),
        scale: home.scale.max(1.0),
        home,
    };
    let spec = WindowSpec {
        class: CLASS,
        title: w!("Kadr"),
        style: WS_POPUP,
        ex_style: WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
        rect: screen,
    };
    match win::create_window(spec, state) {
        Ok(hwnd) => {
            CURRENT.with(|c| *c.borrow_mut() = Some(hwnd));
            OPENED_AT.with(|o| o.set(Some(Instant::now())));
            unsafe {
                let _ = ShowWindow(hwnd, SW_SHOW);
            }
            win::force_foreground(hwnd);
        }
        Err(e) => win::show_error(&format!("Не удалось открыть выбор области: {e}")),
    }
}

impl Overlay {
    fn layout(&mut self) {
        let s = self.scale;
        let mut groups = vec![vec![Btn::Photo, Btn::Video, Btn::Text, Btn::Color]];
        if self.mode != Mode::Color {
            groups.push(vec![Btn::Area, Btn::Window, Btn::Full]);
        }
        if matches!(self.mode, Mode::Photo | Mode::Text) {
            groups.push(vec![Btn::Delay]);
        }
        if self.mode == Mode::Video {
            groups.push(vec![Btn::Mic, Btn::System]);
            groups.push(vec![Btn::Start]);
        }
        groups.push(vec![Btn::Settings, Btn::Close]);

        let size = 32.0 * s;
        let gap = 6.0 * s;
        let group_gap = 17.0 * s;
        let pad = 8.0 * s;
        let mut x = 0.0;
        self.items.clear();
        self.separators.clear();
        let last = groups.len() - 1;
        for (g, group) in groups.into_iter().enumerate() {
            for btn in group {
                let width = if btn == Btn::Start { START_WIDTH_DIP * s } else { size };
                self.items.push(Item { btn, rect: RectF::xywh(x, 0.0, width, size) });
                x += width + gap;
            }
            x -= gap;
            if g != last {
                self.separators.push(x + group_gap / 2.0);
                x += group_gap;
            }
        }
        let width = x + pad * 2.0;
        let home = self.home.rect.translated(-self.screen.x, -self.screen.y);
        let left = home.x as f32 + (home.w as f32 - width) / 2.0;
        let top = home.y as f32 + 12.0 * s;
        self.bar = RectF::xywh(left, top, width, size + pad * 2.0);
        for item in &mut self.items {
            item.rect = item.rect.translated(left + pad, top + pad);
        }
        for sep in &mut self.separators {
            *sep += left + pad;
        }
    }

    fn button_at(&self, x: i32, y: i32) -> Option<Btn> {
        let (x, y) = (x as f32, y as f32);
        self.items.iter().find(|i| i.rect.contains(x, y)).map(|i| i.btn)
    }

    fn on_toolbar(&self, x: i32, y: i32) -> bool {
        self.bar.contains(x as f32, y as f32)
    }

    fn monitor_under_mouse(&self) -> Monitor {
        let (x, y) = (self.mouse.0 + self.screen.x, self.mouse.1 + self.screen.y);
        capture::monitor_at(&self.monitors, x, y).unwrap_or(self.home)
    }

    /// The area being dragged, in screen coordinates.
    fn drag_rect(&self) -> Option<Rect> {
        let from = self.drag_from?;
        // With a window or a screen chosen, a press that barely moved only picks another one.
        if self.shape != Shape::Area && is_click(from, self.mouse) {
            return None;
        }
        let start = (from.0 + self.screen.x, from.1 + self.screen.y);
        let end = (self.mouse.0 + self.screen.x, self.mouse.1 + self.screen.y);
        let r = Rect::from_points(start, end);
        if self.mode == Mode::Video {
            // A video covers one monitor: the one where the drag began.
            let m = capture::monitor_at(&self.monitors, start.0, start.1)?;
            return r.intersect(&m.rect);
        }
        Some(r)
    }

    /// What stays bright on the screen: the area being dragged or what a video will record.
    fn target(&self) -> Option<Rect> {
        self.drag_rect().or(self.selection)
    }

    /// The window a click would pick for a video.
    fn hovered_window(&self) -> Option<Rect> {
        if self.shape != Shape::Window || self.drag_from.is_some() || self.on_toolbar(self.mouse.0, self.mouse.1) {
            return None;
        }
        let (x, y) = (self.mouse.0 + self.screen.x, self.mouse.1 + self.screen.y);
        self.windows.iter().copied().find(|w| w.contains(x, y))
    }

    fn pick_under_mouse(&mut self) {
        match self.shape {
            Shape::Area => {}
            Shape::Window => {
                if let Some(window) = self.hovered_window() {
                    self.selection = Some(window);
                }
            }
            Shape::Full => self.selection = Some(self.monitor_under_mouse().rect),
        }
    }

    fn finish(&mut self, outcome: Outcome) {
        settings::update(|s| {
            s.mic = self.mic;
            s.system_audio = self.system;
        });
        OUTCOME.with(|o| *o.borrow_mut() = Some(outcome));
        CURRENT.with(|c| *c.borrow_mut() = None);
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_HIDE);
            let _ = PostMessageW(Some(self.hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
            let _ = PostMessageW(Some(self.notify), WM_KADR_OVERLAY_DONE, WPARAM(0), LPARAM(0));
        }
    }

    fn capture(&mut self, r: Rect) {
        let in_frame = r.translated(-self.screen.x, -self.screen.y);
        match self.mode {
            Mode::Photo | Mode::Text => {
                let img = self.frame.crop(in_frame);
                if img.w == 0 {
                    return;
                }
                let is_text = self.mode == Mode::Text;
                self.finish(if is_text { Outcome::Text { img } } else { Outcome::Photo { img } });
            }
            Mode::Color => self.pick_color(),
            Mode::Video => {
                let (cx, cy) = r.center();
                let Some(monitor) = capture::monitor_at(&self.monitors, cx, cy) else {
                    return;
                };
                let Some(on_monitor) = r.intersect(&monitor.rect) else {
                    return;
                };
                let region = geom::fit_for_video(on_monitor, monitor.rect);
                let thumb = self.frame.crop(region.translated(-self.screen.x, -self.screen.y));
                let (mic, system) = (self.mic, self.system);
                self.finish(Outcome::Video { monitor, region, thumb, mic, system });
            }
        }
    }

    /// The color under the mouse, in the format chosen in the settings.
    fn color_under_mouse(&self) -> Option<([u8; 3], String)> {
        let (x, y) = self.mouse;
        if !self.frame.bounds().contains(x, y) {
            return None;
        }
        let [b, g, r] = self.frame.px(x, y);
        Some(([r, g, b], format_color(r, g, b, settings::get().color_format)))
    }

    fn pick_color(&mut self) {
        if let Some((rgb, value)) = self.color_under_mouse() {
            self.finish(Outcome::Color { value, rgb });
        }
    }

    fn delay_entries(&self) -> Vec<(u32, RectF)> {
        let s = self.scale;
        let Some(item) = self.items.iter().find(|i| i.btn == Btn::Delay) else {
            return Vec::new();
        };
        let (w, h) = (112.0 * s, 32.0 * s);
        let top = self.bar.b + 6.0 * s;
        let left = item.rect.cx() - w / 2.0;
        CHOICES
            .iter()
            .enumerate()
            .map(|(i, seconds)| (*seconds, RectF::xywh(left, top + i as f32 * h, w, h)))
            .collect()
    }

    fn press_button(&mut self, b: Btn) {
        match b {
            Btn::Photo | Btn::Video | Btn::Text | Btn::Color => {
                let mode = match b {
                    Btn::Photo => Mode::Photo,
                    Btn::Video => Mode::Video,
                    Btn::Text => Mode::Text,
                    _ => Mode::Color,
                };
                if mode != self.mode {
                    self.mode = mode;
                    self.shape = Shape::Area;
                    self.selection = None;
                }
                self.delay_open = false;
                self.layout();
            }
            Btn::Delay => self.delay_open = !self.delay_open,
            Btn::Area => {
                if self.shape != Shape::Area {
                    self.shape = Shape::Area;
                    self.selection = None;
                }
            }
            // A picture is taken at once. A video only selects here and waits for its start.
            Btn::Window | Btn::Full if self.mode != Mode::Video => {
                let monitor = self.monitor_under_mouse().rect;
                let taken = if b == Btn::Full {
                    Some(monitor)
                } else {
                    pick_window(self.active_window, &self.windows, monitor)
                };
                if let Some(r) = taken {
                    self.capture(r);
                    return;
                }
            }
            Btn::Window => {
                self.shape = Shape::Window;
                self.selection = pick_window(self.active_window, &self.windows, self.monitor_under_mouse().rect);
            }
            Btn::Full => {
                self.shape = Shape::Full;
                self.selection = Some(self.monitor_under_mouse().rect);
            }
            Btn::Start => {
                if let Some(r) = self.selection {
                    self.capture(r);
                    return;
                }
            }
            Btn::Mic => self.mic = !self.mic,
            Btn::System => self.system = !self.system,
            Btn::Settings => {
                self.finish(Outcome::OpenSettings);
                return;
            }
            Btn::Close => {
                self.finish(Outcome::Cancel);
                return;
            }
        }
        win::invalidate(self.hwnd);
    }

    fn is_active(&self, b: Btn) -> bool {
        match b {
            Btn::Photo => self.mode == Mode::Photo,
            Btn::Video => self.mode == Mode::Video,
            Btn::Text => self.mode == Mode::Text,
            Btn::Color => self.mode == Mode::Color,
            Btn::Delay => self.delay_open,
            Btn::Area => self.shape == Shape::Area,
            Btn::Window => self.shape == Shape::Window,
            Btn::Full => self.shape == Shape::Full,
            Btn::Mic => self.mic,
            Btn::System => self.system,
            Btn::Start | Btn::Settings | Btn::Close => false,
        }
    }

    fn tip(&self, b: Btn) -> String {
        let on_off = |on: bool| if on { "включён" } else { "выключен" };
        match b {
            Btn::Photo => "Снимок".into(),
            Btn::Video => "Видео".into(),
            Btn::Text => "Копировать текст".into(),
            Btn::Color => "Пипетка: копировать цвет".into(),
            Btn::Delay => "Снимок с задержкой".into(),
            Btn::Area => "Область".into(),
            Btn::Window if self.mode == Mode::Video => "Окно (другое выбирается кликом)".into(),
            Btn::Window => "Активное окно".into(),
            Btn::Full if self.mode == Mode::Video => "Весь экран (другой выбирается кликом)".into(),
            Btn::Full => format!("Весь экран (Enter или {})", settings::get().hotkey.label()),
            Btn::Start if self.selection.is_some() => "Начать запись (Enter)".into(),
            Btn::Start => "Сначала выделите область, окно или экран".into(),
            Btn::Mic => format!("Микрофон {}", on_off(self.mic)),
            Btn::System => format!("Звук ПК {}", on_off(self.system)),
            Btn::Settings => "Настройки".into(),
            Btn::Close => "Закрыть (Esc)".into(),
        }
    }

    fn paint(&mut self) {
        let target = self.target();
        let Some(p) = self.surface.begin() else {
            return;
        };
        if self.bitmap.is_none() || self.bitmap_generation != self.surface.generation {
            self.bitmap = p.bitmap(&self.frame).ok();
            self.bitmap_generation = self.surface.generation;
        }
        let full = RectF::xywh(0.0, 0.0, self.screen.w as f32, self.screen.h as f32);
        if let Some(bmp) = &self.bitmap {
            p.draw_bitmap(bmp, full, None, false);
        }
        // The picker has to show true colors, so the screen is not dimmed for it.
        if self.mode != Mode::Color {
            self.paint_dim_and_target(&p, full, target);
            if let Some(window) = self.hovered_window().filter(|w| Some(*w) != self.selection) {
                let r = RectF::from_rect(window.translated(-self.screen.x, -self.screen.y));
                p.stroke(r.inflate(1.0), TEXT_DIM, 1.5 * self.scale);
            }
        }
        self.paint_toolbar(&p);
        if self.mode == Mode::Color && !self.on_toolbar(self.mouse.0, self.mouse.1) {
            self.paint_loupe(&p);
        }
        if self.delay_open {
            self.paint_delay_menu(&p);
        }
        self.surface.end();
    }

    fn paint_dim_and_target(&self, p: &Painter, full: RectF, target: Option<Rect>) {
        let s = self.scale;
        let Some(t) = target else {
            p.fill(full, DIM);
            return;
        };
        let r = RectF::from_rect(t.translated(-self.screen.x, -self.screen.y));
        p.fill(RectF::new(full.l, full.t, full.r, r.t), DIM);
        p.fill(RectF::new(full.l, r.b, full.r, full.b), DIM);
        p.fill(RectF::new(full.l, r.t, r.l, r.b), DIM);
        p.fill(RectF::new(r.r, r.t, full.r, r.b), DIM);
        let border = if self.mode == Mode::Video { RED } else { rgb(0xffffff) };
        p.stroke(r.inflate(1.0), border, 1.5 * s);

        let label = format!("{} x {}", t.w, t.h);
        let size = 12.0 * s;
        let w = measure(&label, Font::Ui, size).0 + 16.0 * s;
        let h = 24.0 * s;
        let mut y = r.b + 6.0 * s;
        if y + h > full.b {
            y = r.b - h - 6.0 * s;
        }
        let badge = RectF::xywh(r.l.max(full.l), y, w, h);
        p.round(badge, 4.0 * s, BAR_BG);
        p.text(&label, badge, Font::Ui, size, TEXT, Align::Center);
    }

    /// A magnified view of the pixels around the mouse, with the color value under it.
    fn paint_loupe(&self, p: &Painter) {
        const CELLS: i32 = 11;
        let (Some(bmp), Some(([r, g, b], value))) = (&self.bitmap, self.color_under_mouse()) else {
            return;
        };
        let s = self.scale;
        let cell = 12.0 * s;
        let view = cell * CELLS as f32;
        let (w, h) = (view + 16.0 * s, view + 16.0 * s + 34.0 * s);
        let (mx, my) = self.mouse;
        let m = self.monitor_under_mouse().rect.translated(-self.screen.x, -self.screen.y);
        // Next to the pointer, on whichever side still fits on this monitor.
        let gap = 24.0 * s;
        let mut left = mx as f32 + gap;
        if left + w > m.right() as f32 {
            left = mx as f32 - gap - w;
        }
        let mut top = my as f32 + gap;
        if top + h > m.bottom() as f32 {
            top = my as f32 - gap - h;
        }
        let panel = RectF::xywh(left, top, w, h);
        p.round(panel, 8.0 * s, FLYOUT);
        p.round_stroke(panel, 8.0 * s, FLYOUT_STROKE, 1.0);

        let half = CELLS / 2;
        let bounds = self.frame.bounds();
        let sx = (mx - half).clamp(0, (bounds.w - CELLS).max(0));
        let sy = (my - half).clamp(0, (bounds.h - CELLS).max(0));
        let source = RectF::xywh(sx as f32, sy as f32, CELLS as f32, CELLS as f32);
        let dst = RectF::xywh(left + 8.0 * s, top + 8.0 * s, view, view);
        p.draw_bitmap(bmp, dst, Some(source), false);
        let center = RectF::xywh(
            dst.l + (mx - sx) as f32 * cell,
            dst.t + (my - sy) as f32 * cell,
            cell,
            cell,
        );
        p.stroke(center.inflate(1.0), rgb(0x000000), 1.0);
        p.stroke(center, rgb(0xffffff), 1.0);

        let row = RectF::new(panel.l + 8.0 * s, dst.b + 8.0 * s, panel.r - 8.0 * s, panel.b - 8.0 * s);
        let chip = RectF::xywh(row.l, row.cy() - 8.0 * s, 16.0 * s, 16.0 * s);
        let color = (r as u32) << 16 | (g as u32) << 8 | b as u32;
        p.round(chip, 3.0 * s, crate::gfx::rgb(color));
        p.round_stroke(chip, 3.0 * s, FLYOUT_STROKE, 1.0);
        let text = RectF::new(chip.r + 8.0 * s, row.t, row.r, row.b);
        p.text(&value, text, Font::Ui, 12.0 * s, TEXT, Align::Left);
    }

    fn paint_delay_menu(&self, p: &Painter) {
        let s = self.scale;
        let entries = self.delay_entries();
        let (Some(first), Some(last)) = (entries.first(), entries.last()) else {
            return;
        };
        let panel = RectF::new(first.1.l - 4.0 * s, first.1.t - 4.0 * s, first.1.r + 4.0 * s, last.1.b + 4.0 * s);
        p.round(panel, 8.0 * s, FLYOUT);
        p.round_stroke(panel, 8.0 * s, FLYOUT_STROKE, 1.0);
        let (mx, my) = (self.mouse.0 as f32, self.mouse.1 as f32);
        for (seconds, r) in entries {
            if r.contains(mx, my) {
                p.round(r, 4.0 * s, HOVER);
            }
            let text = RectF::new(r.l + 12.0 * s, r.t, r.r, r.b);
            p.text(&format!("Через {seconds} с"), text, Font::Ui, 13.0 * s, TEXT, Align::Left);
        }
    }

    fn paint_toolbar(&self, p: &Painter) {
        let s = self.scale;
        p.round(self.bar, 8.0 * s, BAR_BG);
        for x in &self.separators {
            let (top, bottom) = (self.bar.t + 12.0 * s, self.bar.b - 12.0 * s);
            p.line((*x, top), (*x, bottom), DIVIDER, 1.0);
        }
        for item in &self.items {
            if item.btn == Btn::Start {
                self.paint_start(p, item.rect);
                continue;
            }
            // The sound switches change their icon; only the modes get a filled tile.
            let selected = !matches!(item.btn, Btn::Mic | Btn::System) && self.is_active(item.btn);
            let hovered = self.hover == Some(item.btn);
            let tile = if selected { accent() } else { BAR_BG };
            if selected {
                p.round(item.rect, 5.0 * s, tile);
            } else if hovered {
                p.round(item.rect, 5.0 * s, HOVER);
            }
            let color = if selected { ON_ACCENT } else { TEXT };
            paint_button_icon(p, item.btn, self.is_active(item.btn), item.rect, s, color, tile);
        }
        if let Some(b) = self.hover.filter(|_| !self.delay_open) {
            if let Some(item) = self.items.iter().find(|i| i.btn == b) {
                let tip = self.tip(b);
                let size = 12.0 * s;
                let w = measure(&tip, Font::Ui, size).0 + 18.0 * s;
                let r = RectF::xywh(item.rect.cx() - w / 2.0, self.bar.b + 6.0 * s, w, 28.0 * s);
                p.round(r, 4.0 * s, FLYOUT);
                p.round_stroke(r, 4.0 * s, FLYOUT_STROKE, 1.0);
                p.text(&tip, r, Font::Ui, size, TEXT, Align::Center);
            }
        }
    }
}

impl Overlay {
    /// The only labeled button, so that starting a video is easy to find.
    fn paint_start(&self, p: &Painter, r: RectF) {
        let s = self.scale;
        let hovered = self.hover == Some(Btn::Start);
        let (tile, c) = match self.selection {
            Some(_) if hovered => (rgb(0xf0585a), TEXT),
            Some(_) => (RED, TEXT),
            None => (CONTROL, TEXT_OFF),
        };
        p.round(r, 5.0 * s, tile);
        p.fill_circle(r.l + 15.0 * s, r.cy(), 4.5 * s, c);
        let text = RectF::new(r.l + 25.0 * s, r.t, r.r, r.b);
        p.text("Начать", text, Font::Ui, 13.0 * s, c, Align::Left);
    }
}

/// A small plus at the lower right corner of a capture shape, on its own clean spot.
fn paint_plus(p: &Painter, x: f32, y: f32, s: f32, c: Color, background: Color) {
    p.fill_circle(x, y, 4.2 * s, background);
    let arm = 2.8 * s;
    let w = 1.2 * s;
    p.line((x - arm, y), (x + arm, y), c, w);
    p.line((x, y - arm), (x, y + arm), c, w);
}

/// A letter inside a selection frame: "take the text from here".
pub fn paint_scan_text_icon(p: &Painter, cx: f32, cy: f32, s: f32, c: Color) {
    let (u, arm, w) = (7.5 * s, 3.8 * s, 1.25 * s);
    for (sx, sy) in [(-1.0, -1.0), (1.0, -1.0), (-1.0, 1.0), (1.0, 1.0)] {
        let corner = (cx + sx * u, cy + sy * u);
        p.line(corner, (corner.0 - sx * arm, corner.1), c, w);
        p.line(corner, (corner.0, corner.1 - sy * arm), c, w);
    }
    let area = RectF::xywh(cx - 8.0 * s, cy - 8.0 * s, 16.0 * s, 16.0 * s);
    p.text("T", area, Font::UiBold, 10.5 * s, c, Align::Center);
}

fn paint_button_icon(p: &Painter, b: Btn, active: bool, r: RectF, s: f32, c: Color, background: Color) {
    let (cx, cy) = (r.cx(), r.cy());
    let w = 1.25 * s;
    let shape = RectF::new(cx - 7.5 * s, cy - 6.5 * s, cx + 7.5 * s, cy + 6.5 * s);
    match b {
        Btn::Photo => p.icon('\u{E722}', cx, cy, 16.0 * s, c),
        Btn::Video => p.icon('\u{E714}', cx, cy, 16.0 * s, c),
        Btn::Text => paint_scan_text_icon(p, cx, cy, s, c),
        Btn::Color => p.icon('\u{EF3C}', cx, cy, 16.0 * s, c),
        Btn::Delay => p.icon('\u{E823}', cx, cy, 16.0 * s, c),
        Btn::Area => {
            p.round_stroke(shape, 2.5 * s, c, w);
            paint_plus(p, shape.r, shape.b, s, c, background);
        }
        Btn::Window => {
            p.round_stroke(shape, 2.5 * s, c, w);
            let bar_y = shape.t + 3.8 * s;
            p.line((shape.l, bar_y), (shape.r, bar_y), c, w);
            paint_plus(p, shape.r, shape.b, s, c, background);
        }
        Btn::Full => {
            let outer = RectF::new(cx - 8.5 * s, cy - 6.0 * s, cx + 8.5 * s, cy + 6.0 * s);
            p.round_stroke(outer, 2.0 * s, c, w);
            let (ix, iy) = (5.0 * s, 2.8 * s);
            let arm = 2.2 * s;
            for (sx, sy) in [(-1.0, -1.0), (1.0, -1.0), (-1.0, 1.0), (1.0, 1.0)] {
                let corner = (cx + sx * ix, cy + sy * iy);
                p.line(corner, (corner.0 - sx * arm, corner.1), c, w);
                p.line(corner, (corner.0, corner.1 - sy * arm), c, w);
            }
        }
        Btn::Mic if active => p.icon('\u{E720}', cx, cy, 16.0 * s, c),
        Btn::Mic => p.icon('\u{F781}', cx, cy, 16.0 * s, TEXT_DIM),
        Btn::System if active => p.icon('\u{E767}', cx, cy, 16.0 * s, c),
        Btn::System => p.icon('\u{E74F}', cx, cy, 16.0 * s, TEXT_DIM),
        Btn::Start => {} // Painted by `paint_start`.
        Btn::Settings => p.icon('\u{E713}', cx, cy, 16.0 * s, c),
        Btn::Close => p.icon('\u{E711}', cx, cy, 13.0 * s, c),
    }
}

impl WndState for Overlay {
    fn on_message(&mut self, hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
        match msg {
            WM_CREATE => {
                self.hwnd = hwnd;
                self.surface = Surface::new(hwnd);
                self.layout();
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
            WM_SETCURSOR => {
                let on_bar = self.drag_from.is_none() && self.on_toolbar(self.mouse.0, self.mouse.1);
                let id = if on_bar { IDC_ARROW } else { IDC_CROSS };
                unsafe {
                    if let Ok(c) = LoadCursorW(None, id) {
                        SetCursor(Some(c));
                    }
                }
                Some(LRESULT(1))
            }
            WM_MOUSEMOVE => {
                self.mouse = win::mouse_xy(lp);
                self.hover = if self.drag_from.is_none() {
                    self.button_at(self.mouse.0, self.mouse.1)
                } else {
                    None
                };
                win::invalidate(hwnd);
                Some(LRESULT(0))
            }
            WM_LBUTTONDOWN => {
                let (x, y) = win::mouse_xy(lp);
                self.mouse = (x, y);
                if self.delay_open {
                    let chosen = self.delay_entries().iter().find(|e| e.1.contains(x as f32, y as f32)).map(|e| e.0);
                    if let Some(seconds) = chosen {
                        let mode = self.mode;
                        self.finish(Outcome::Delay { seconds, mode });
                        return Some(LRESULT(0));
                    }
                    if !self.on_toolbar(x, y) {
                        self.delay_open = false;
                        win::invalidate(hwnd);
                        return Some(LRESULT(0));
                    }
                }
                if self.on_toolbar(x, y) {
                    self.pressed_toolbar = true;
                    return Some(LRESULT(0));
                }
                if self.mode != Mode::Color {
                    self.drag_from = Some((x, y));
                    unsafe { SetCapture(hwnd) };
                    win::invalidate(hwnd);
                }
                Some(LRESULT(0))
            }
            WM_LBUTTONUP => {
                let (x, y) = win::mouse_xy(lp);
                self.mouse = (x, y);
                if self.pressed_toolbar {
                    self.pressed_toolbar = false;
                    if let Some(b) = self.button_at(x, y) {
                        self.press_button(b);
                    }
                    return Some(LRESULT(0));
                }
                if self.drag_from.is_some() {
                    let dragged = self.drag_rect();
                    self.drag_from = None;
                    unsafe {
                        let _ = ReleaseCapture();
                    }
                    let selected = dragged.filter(|r| r.w >= MIN_DRAG && r.h >= MIN_DRAG);
                    match (self.mode, selected) {
                        (Mode::Video, Some(r)) => {
                            self.selection = Some(r);
                            self.shape = Shape::Area;
                        }
                        (Mode::Video, None) if dragged.is_none() => self.pick_under_mouse(),
                        (_, Some(r)) => {
                            self.capture(r);
                            return Some(LRESULT(0));
                        }
                        _ => {}
                    }
                    win::invalidate(hwnd);
                    return Some(LRESULT(0));
                }
                if self.mode == Mode::Color {
                    self.pick_color();
                }
                Some(LRESULT(0))
            }
            WM_RBUTTONUP => {
                self.finish(Outcome::Cancel);
                Some(LRESULT(0))
            }
            WM_KEYDOWN => {
                match VIRTUAL_KEY(wp.0 as u16) {
                    VK_ESCAPE => self.finish(Outcome::Cancel),
                    // Starting a video is a step of its own, even from the keyboard.
                    VK_RETURN if self.mode == Mode::Video => {
                        self.press_button(if self.selection.is_some() { Btn::Start } else { Btn::Full });
                    }
                    VK_RETURN => {
                        let m = self.monitor_under_mouse();
                        self.capture(m.rect);
                    }
                    _ => {}
                }
                Some(LRESULT(0))
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hotkey_right_after_opening_does_not_take_the_whole_screen() {
        let opened = Instant::now();
        assert!(!past_guard(opened, opened + Duration::from_millis(150)));
        assert!(!past_guard(opened, opened + Duration::from_millis(599)));
        assert!(past_guard(opened, opened + Duration::from_millis(600)));
    }

    #[test]
    fn window_button_prefers_the_active_window() {
        let active = Rect::new(3000, 100, 800, 600);
        let top = Rect::new(100, 100, 500, 400);
        let monitor = Rect::new(0, 0, 2560, 1440);
        assert_eq!(pick_window(Some(active), &[top], monitor), Some(active));
    }

    #[test]
    fn without_an_active_window_the_topmost_on_this_monitor_is_taken() {
        let other_monitor = Rect::new(3000, 100, 800, 600);
        let top = Rect::new(100, 100, 500, 400);
        let below = Rect::new(200, 200, 500, 400);
        let monitor = Rect::new(0, 0, 2560, 1440);
        assert_eq!(pick_window(None, &[other_monitor, top, below], monitor), Some(top));
        assert_eq!(pick_window(None, &[other_monitor], monitor), None);
    }

    #[test]
    fn press_that_barely_moved_is_a_click() {
        assert!(is_click((100, 100), (100, 100)));
        assert!(is_click((100, 100), (100 + MIN_DRAG - 1, 100 - MIN_DRAG + 1)));
        assert!(!is_click((100, 100), (100 + MIN_DRAG, 100)));
        assert!(!is_click((100, 100), (100, 100 - MIN_DRAG)));
    }
}
