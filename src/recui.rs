//! What is on screen during a recording: countdown, frame around the region, and a
//! small control panel. All of it is hidden from the recording itself.

use crate::capture::Monitor;
use crate::geom::Rect;
use crate::gfx::*;
use crate::image::Image;
use crate::record::{Options, Recorder};
use crate::win::{self, WindowSpec, WndState};
use std::cell::RefCell;
use std::path::PathBuf;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::*;

const COUNTDOWN_STEP_MS: u32 = 700;
const COUNTDOWN_FROM: i32 = 3;
const PANEL_TIMER: usize = 1;

struct Session {
    monitor: Monitor,
    region: Rect,
    thumb: Image,
    mic: bool,
    system: bool,
    notify: HWND,
    path: PathBuf,
    strips: Vec<HWND>,
    countdown: Option<HWND>,
    panel: Option<HWND>,
    recorder: Option<Recorder>,
}

thread_local! {
    static SESSION: RefCell<Option<Session>> = const { RefCell::new(None) };
}

pub fn is_active() -> bool {
    SESSION.with(|s| s.borrow().is_some())
}

pub fn begin(monitor: Monitor, region: Rect, thumb: Image, mic: bool, system: bool, notify: HWND) {
    if is_active() {
        return;
    }
    let path = crate::paths::recordings_dir().join(format!("Запись экрана {}.mp4", crate::paths::timestamp()));
    let strips = create_strips(region, monitor);
    let start_now = !crate::settings::get().countdown;
    let countdown = if start_now { None } else { create_countdown(region, monitor.scale.max(1.0)) };
    SESSION.with(|s| {
        *s.borrow_mut() = Some(Session {
            monitor,
            region,
            thumb,
            mic,
            system,
            notify,
            path,
            strips,
            countdown,
            panel: None,
            recorder: None,
        });
    });
    if start_now {
        countdown_finished();
    }
}

fn close_later(hwnd: HWND) {
    unsafe {
        let _ = ShowWindow(hwnd, SW_HIDE);
        let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
    }
}

/// Removes everything from the screen; the session itself stays until the file is done.
fn close_windows() {
    SESSION.with(|s| {
        if let Some(session) = s.borrow_mut().as_mut() {
            for hwnd in session.strips.drain(..) {
                close_later(hwnd);
            }
            if let Some(hwnd) = session.countdown.take() {
                close_later(hwnd);
            }
            if let Some(hwnd) = session.panel.take() {
                close_later(hwnd);
            }
        }
    });
}

/// Stops the recording, or cancels it if the countdown is still running.
pub fn stop() {
    let recording = SESSION.with(|s| s.borrow().as_ref().map(|x| x.recorder.is_some()));
    match recording {
        None => {}
        Some(false) => {
            close_windows();
            SESSION.with(|s| *s.borrow_mut() = None);
        }
        Some(true) => {
            close_windows();
            SESSION.with(|s| {
                if let Some(r) = s.borrow().as_ref().and_then(|x| x.recorder.as_ref()) {
                    r.stop();
                }
            });
        }
    }
}

/// The recording thread has finished writing the file.
pub fn finished() {
    close_windows();
    let Some(session) = SESSION.with(|s| s.borrow_mut().take()) else {
        return;
    };
    let Some(recorder) = session.recorder else {
        return;
    };
    match recorder.finish() {
        Ok(stats) => {
            crate::paths::log(&format!("recorded {} frames in {:.1}s", stats.frames, stats.duration.as_secs_f64()));
            let name = session.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let copied = crate::clipboard::set_file(&session.path);
            if let Err(e) = &copied {
                crate::paths::log(&format!("clipboard: {e}"));
            }
            crate::notify::video_saved(session.thumb, name, copied.is_ok());
        }
        Err(e) => win::show_error(&format!("Запись не удалась: {e}")),
    }
}

fn countdown_finished() {
    let settings = crate::settings::get();
    let options = SESSION.with(|s| {
        s.borrow().as_ref().map(|x| Options {
            monitor: x.monitor.handle.0 as isize,
            monitor_rect: x.monitor.rect,
            region: x.region,
            mic: x.mic,
            system_audio: x.system,
            mic_device: settings.mic_device.clone(),
            system_device: settings.system_device.clone(),
            fps: settings.valid_fps(),
            quality: settings.quality,
            cursor: settings.show_cursor,
            path: x.path.clone(),
            notify: x.notify.0 as isize,
        })
    });
    let Some(options) = options else {
        return;
    };
    match Recorder::start(options) {
        Ok(recorder) => {
            let (region, monitor) = SESSION.with(|s| {
                let mut s = s.borrow_mut();
                let x = s.as_mut().expect("session exists while recording starts");
                x.recorder = Some(recorder);
                (x.region, x.monitor)
            });
            let panel = create_panel(region, monitor);
            SESSION.with(|s| {
                if let Some(x) = s.borrow_mut().as_mut() {
                    x.panel = panel;
                }
            });
        }
        Err(e) => {
            close_windows();
            SESSION.with(|s| *s.borrow_mut() = None);
            win::show_error(&format!("Не удалось начать запись: {e}"));
        }
    }
}

// ---- Frame around the region ----

struct Strip {
    brush: HBRUSH,
}

impl WndState for Strip {
    fn on_message(&mut self, hwnd: HWND, msg: u32, wp: WPARAM, _lp: LPARAM) -> Option<LRESULT> {
        match msg {
            WM_ERASEBKGND => {
                let mut r = RECT::default();
                unsafe {
                    let _ = GetClientRect(hwnd, &mut r);
                    FillRect(HDC(wp.0 as *mut _), &r, self.brush);
                }
                Some(LRESULT(1))
            }
            WM_NCDESTROY => None,
            WM_DESTROY => {
                unsafe {
                    let _ = DeleteObject(self.brush.into());
                }
                None
            }
            _ => None,
        }
    }
}

fn create_strips(region: Rect, monitor: Monitor) -> Vec<HWND> {
    if region == monitor.rect {
        return Vec::new(); // The frame would be off-screen.
    }
    let t = (2.0 * monitor.scale.max(1.0)) as i32;
    let rects = [
        Rect::new(region.x - t, region.y - t, region.w + t * 2, t),
        Rect::new(region.x - t, region.bottom(), region.w + t * 2, t),
        Rect::new(region.x - t, region.y, t, region.h),
        Rect::new(region.right(), region.y, t, region.h),
    ];
    let mut out = Vec::new();
    for rect in rects {
        // Red, as 0x00BBGGRR.
        let brush = unsafe { CreateSolidBrush(COLORREF(0x003B38E5)) };
        let spec = WindowSpec {
            class: w!("KadrStrip"),
            title: w!(""),
            style: WS_POPUP,
            // Layered and transparent: clicks go through to the windows below.
            ex_style: WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_LAYERED | WS_EX_TRANSPARENT,
            rect,
        };
        if let Ok(hwnd) = win::create_window(spec, Strip { brush }) {
            unsafe {
                let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 255, LWA_ALPHA);
                win::exclude_from_capture(hwnd);
                let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            }
            out.push(hwnd);
        }
    }
    out
}

// ---- Countdown ----

struct Countdown {
    surface: Surface,
    left: i32,
    scale: f32,
}

impl WndState for Countdown {
    fn on_message(&mut self, hwnd: HWND, msg: u32, _wp: WPARAM, _lp: LPARAM) -> Option<LRESULT> {
        match msg {
            WM_CREATE => {
                self.surface = Surface::new(hwnd);
                Some(LRESULT(0))
            }
            WM_ERASEBKGND => Some(LRESULT(1)),
            WM_PAINT => {
                if let Some(p) = self.surface.begin() {
                    let (w, h) = win::client_size(hwnd);
                    p.clear(FLYOUT);
                    let area = RectF::xywh(0.0, 0.0, w as f32, h as f32);
                    p.text(&self.left.to_string(), area, Font::UiBold, 54.0 * self.scale, TEXT, Align::Center);
                    self.surface.end();
                }
                unsafe {
                    let _ = ValidateRect(Some(hwnd), None);
                }
                Some(LRESULT(0))
            }
            WM_TIMER => {
                self.left -= 1;
                if self.left > 0 {
                    win::invalidate(hwnd);
                    return Some(LRESULT(0));
                }
                unsafe {
                    let _ = KillTimer(Some(hwnd), 1);
                }
                SESSION.with(|s| {
                    if let Some(x) = s.borrow_mut().as_mut() {
                        x.countdown = None;
                    }
                });
                close_later(hwnd);
                countdown_finished();
                Some(LRESULT(0))
            }
            WM_MOUSEACTIVATE => Some(LRESULT(MA_NOACTIVATE as isize)),
            // A click on the countdown cancels the recording.
            WM_LBUTTONUP => {
                stop();
                Some(LRESULT(0))
            }
            _ => None,
        }
    }
}

fn create_countdown(region: Rect, scale: f32) -> Option<HWND> {
    let size = (110.0 * scale) as i32;
    let (cx, cy) = region.center();
    let spec = WindowSpec {
        class: w!("KadrCountdown"),
        title: w!(""),
        style: WS_POPUP,
        ex_style: WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
        rect: Rect::new(cx - size / 2, cy - size / 2, size, size),
    };
    let state = Countdown { surface: Surface::new(HWND::default()), left: COUNTDOWN_FROM, scale };
    let hwnd = win::create_window(spec, state).ok()?;
    win::round_corners(hwnd);
    win::exclude_from_capture(hwnd);
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        SetTimer(Some(hwnd), 1, COUNTDOWN_STEP_MS, None);
    }
    Some(hwnd)
}

// ---- Control panel ----

struct Panel {
    surface: Surface,
    scale: f32,
    mic: RectF,
    pause: RectF,
    stop: RectF,
}

fn format_time(seconds: u64) -> String {
    let (h, m, s) = (seconds / 3600, seconds / 60 % 60, seconds % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m:02}:{s:02}") }
}

impl Panel {
    fn paint(&mut self, hwnd: HWND) {
        let (elapsed, paused, mic_on) = SESSION.with(|s| {
            s.borrow()
                .as_ref()
                .and_then(|x| x.recorder.as_ref())
                .map(|r| (r.elapsed().as_secs(), r.is_paused(), r.is_mic_on()))
                .unwrap_or((0, false, false))
        });
        let Some(p) = self.surface.begin() else {
            return;
        };
        let s = self.scale;
        let (w, h) = win::client_size(hwnd);
        p.clear(FLYOUT);
        let cy = h as f32 / 2.0;
        let dot = if paused { rgb(0xffc107) } else { RED };
        p.fill_circle(16.0 * s, cy, 5.0 * s, dot);
        let time = RectF::new(28.0 * s, 0.0, self.mic.l, h as f32);
        p.text(&format_time(elapsed), time, Font::UiBold, 13.5 * s, TEXT, Align::Left);

        let (mut mx, mut my) = win::cursor_pos();
        let mut rect = RECT::default();
        unsafe {
            let _ = GetWindowRect(hwnd, &mut rect);
        }
        mx -= rect.left;
        my -= rect.top;
        for r in [self.mic, self.pause, self.stop] {
            if r.contains(mx as f32, my as f32) && mx < w {
                p.round(r, 4.0 * s, HOVER);
            }
        }
        let (mic_glyph, mic_color) = if mic_on { ('\u{E720}', TEXT) } else { ('\u{F781}', TEXT_DIM) };
        p.icon(mic_glyph, self.mic.cx(), self.mic.cy(), 15.0 * s, mic_color);
        let glyph = if paused { '\u{E768}' } else { '\u{E769}' };
        p.icon(glyph, self.pause.cx(), self.pause.cy(), 15.0 * s, TEXT);
        let square = RectF::xywh(self.stop.cx() - 6.0 * s, self.stop.cy() - 6.0 * s, 12.0 * s, 12.0 * s);
        p.round(square, 2.0 * s, RED);
        self.surface.end();
    }
}

impl WndState for Panel {
    fn on_message(&mut self, hwnd: HWND, msg: u32, _wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
        match msg {
            WM_CREATE => {
                self.surface = Surface::new(hwnd);
                unsafe { SetTimer(Some(hwnd), PANEL_TIMER, 250, None) };
                Some(LRESULT(0))
            }
            WM_ERASEBKGND => Some(LRESULT(1)),
            WM_PAINT => {
                self.paint(hwnd);
                unsafe {
                    let _ = ValidateRect(Some(hwnd), None);
                }
                Some(LRESULT(0))
            }
            WM_TIMER | WM_MOUSEMOVE => {
                win::invalidate(hwnd);
                Some(LRESULT(0))
            }
            WM_MOUSEACTIVATE => Some(LRESULT(MA_NOACTIVATE as isize)),
            // Everything except the buttons drags the panel out of the way.
            WM_NCHITTEST => {
                let (sx, sy) = win::mouse_xy(lp);
                let mut rect = RECT::default();
                unsafe {
                    let _ = GetWindowRect(hwnd, &mut rect);
                }
                let (x, y) = ((sx - rect.left) as f32, (sy - rect.top) as f32);
                let on_button = [self.mic, self.pause, self.stop].iter().any(|r| r.contains(x, y));
                Some(LRESULT(if on_button { HTCLIENT } else { HTCAPTION } as isize))
            }
            WM_LBUTTONUP => {
                let (x, y) = win::mouse_xy(lp);
                let (x, y) = (x as f32, y as f32);
                if self.stop.contains(x, y) {
                    stop();
                } else if self.mic.contains(x, y) {
                    SESSION.with(|s| {
                        if let Some(r) = s.borrow().as_ref().and_then(|x| x.recorder.as_ref()) {
                            r.toggle_mic();
                        }
                    });
                    win::invalidate(hwnd);
                } else if self.pause.contains(x, y) {
                    SESSION.with(|s| {
                        if let Some(r) = s.borrow().as_ref().and_then(|x| x.recorder.as_ref()) {
                            r.toggle_pause();
                        }
                    });
                    win::invalidate(hwnd);
                }
                Some(LRESULT(0))
            }
            _ => None,
        }
    }
}

fn create_panel(region: Rect, monitor: Monitor) -> Option<HWND> {
    let s = monitor.scale.max(1.0);
    let (w, h) = ((214.0 * s) as i32, (44.0 * s) as i32);
    let gap = (10.0 * s) as i32;
    let x = (region.center().0 - w / 2).clamp(monitor.work.x, monitor.work.right() - w);
    // Above the frame when there is room. Otherwise on top of the region, which is
    // fine because the panel does not appear in the video.
    let above = region.y - h - gap;
    let y = if above >= monitor.work.y { above } else { monitor.work.y + gap };
    let button = 36.0 * s;
    let top = (h as f32 - button) / 2.0;
    let right = w as f32 - top;
    let state = Panel {
        surface: Surface::new(HWND::default()),
        scale: s,
        stop: RectF::xywh(right - button, top, button, button),
        pause: RectF::xywh(right - button * 2.0 - 2.0 * s, top, button, button),
        mic: RectF::xywh(right - button * 3.0 - 4.0 * s, top, button, button),
    };
    let spec = WindowSpec {
        class: w!("KadrPanel"),
        title: w!("Kadr"),
        style: WS_POPUP,
        ex_style: WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
        rect: Rect::new(x, y, w, h),
    };
    let hwnd = win::create_window(spec, state).ok()?;
    win::round_corners(hwnd);
    win::exclude_from_capture(hwnd);
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    }
    Some(hwnd)
}

#[cfg(test)]
mod tests {
    use super::format_time;

    #[test]
    fn time_is_shown_with_hours_only_when_needed() {
        assert_eq!(format_time(7), "00:07");
        assert_eq!(format_time(754), "12:34");
        assert_eq!(format_time(3725), "1:02:05");
    }
}
