//! A delayed capture: time to open a menu or a tooltip before the screen is frozen.

use crate::capture;
use crate::geom::Rect;
use crate::gfx::*;
use crate::overlay::Mode;
use crate::win::{self, WindowSpec, WndState};
use std::cell::Cell;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::DwmFlush;
use windows::Win32::Graphics::Gdi::ValidateRect;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::*;

pub const CHOICES: [u32; 3] = [3, 5, 10];
const TICK: usize = 1;

thread_local! {
    static CURRENT: Cell<Option<HWND>> = const { Cell::new(None) };
}

pub fn is_active() -> bool {
    CURRENT.with(|c| c.get()).is_some()
}

fn close(hwnd: HWND) {
    CURRENT.with(|c| c.set(None));
    unsafe {
        let _ = KillTimer(Some(hwnd), TICK);
        let _ = ShowWindow(hwnd, SW_HIDE);
        let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
    }
}

pub fn cancel() {
    if let Some(hwnd) = CURRENT.with(|c| c.get()) {
        close(hwnd);
    }
}

struct Delay {
    surface: Surface,
    left: u32,
    mode: Mode,
    scale: f32,
}

pub fn start(seconds: u32, mode: Mode) {
    if is_active() {
        return;
    }
    let monitors = capture::monitors();
    let (x, y) = win::cursor_pos();
    let Some(m) = capture::monitor_at(&monitors, x, y) else {
        return;
    };
    let s = m.scale.max(1.0);
    let (w, h) = ((186.0 * s) as i32, (40.0 * s) as i32);
    let rect = Rect::new(m.rect.x + (m.rect.w - w) / 2, m.rect.y + (12.0 * s) as i32, w, h);
    let state = Delay { surface: Surface::new(HWND::default()), left: seconds, mode, scale: s };
    let spec = WindowSpec {
        class: w!("KadrDelay"),
        title: w!("Kadr"),
        style: WS_POPUP,
        ex_style: WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
        rect,
    };
    if let Ok(hwnd) = win::create_window(spec, state) {
        CURRENT.with(|c| c.set(Some(hwnd)));
        win::round_corners(hwnd);
        win::exclude_from_capture(hwnd);
        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            SetTimer(Some(hwnd), TICK, 1000, None);
        }
    }
}

impl WndState for Delay {
    fn on_message(&mut self, hwnd: HWND, msg: u32, _wp: WPARAM, _lp: LPARAM) -> Option<LRESULT> {
        match msg {
            WM_CREATE => {
                self.surface = Surface::new(hwnd);
                Some(LRESULT(0))
            }
            WM_ERASEBKGND => Some(LRESULT(1)),
            WM_PAINT => {
                if let Some(p) = self.surface.begin() {
                    let s = self.scale;
                    let (w, h) = win::client_size(hwnd);
                    p.clear(FLYOUT);
                    p.icon('\u{E823}', 22.0 * s, h as f32 / 2.0, 15.0 * s, TEXT);
                    let text = RectF::new(40.0 * s, 0.0, w as f32, h as f32);
                    let label = format!("Снимок через {} с", self.left);
                    p.text(&label, text, Font::Ui, 13.0 * s, TEXT, Align::Left);
                    self.surface.end();
                }
                unsafe {
                    let _ = ValidateRect(Some(hwnd), None);
                }
                Some(LRESULT(0))
            }
            WM_TIMER => {
                self.left = self.left.saturating_sub(1);
                if self.left > 0 {
                    win::invalidate(hwnd);
                    return Some(LRESULT(0));
                }
                let mode = self.mode;
                close(hwnd);
                unsafe {
                    // The freeze of the screen must not catch this window on its way out.
                    let _ = DwmFlush();
                }
                crate::app::open_overlay(mode);
                Some(LRESULT(0))
            }
            WM_MOUSEACTIVATE => Some(LRESULT(MA_NOACTIVATE as isize)),
            // A click cancels the delayed capture.
            WM_LBUTTONUP => {
                close(hwnd);
                Some(LRESULT(0))
            }
            _ => None,
        }
    }
}
