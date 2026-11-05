//! Screen pixels and the layout of monitors and windows, all in physical pixels.

use crate::geom::Rect;
use crate::image::Image;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::*;

#[derive(Clone, Copy, Debug)]
pub struct Monitor {
    pub handle: HMONITOR,
    pub rect: Rect,
    pub work: Rect,
    pub scale: f32,
}

fn rect_of(r: RECT) -> Rect {
    Rect::new(r.left, r.top, r.right - r.left, r.bottom - r.top)
}

pub fn monitors() -> Vec<Monitor> {
    unsafe extern "system" fn each(h: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
        unsafe {
            let list = &mut *(data.0 as *mut Vec<Monitor>);
            let mut info = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
            if GetMonitorInfoW(h, &mut info).as_bool() {
                let (mut dx, mut dy) = (96u32, 96u32);
                let _ = GetDpiForMonitor(h, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
                list.push(Monitor {
                    handle: h,
                    rect: rect_of(info.rcMonitor),
                    work: rect_of(info.rcWork),
                    scale: dx as f32 / 96.0,
                });
            }
            TRUE
        }
    }
    let mut list: Vec<Monitor> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(each), LPARAM(&mut list as *mut _ as isize));
    }
    list
}

pub fn monitor_at(list: &[Monitor], x: i32, y: i32) -> Option<Monitor> {
    list.iter().copied().find(|m| m.rect.contains(x, y)).or_else(|| list.first().copied())
}

pub fn virtual_screen() -> Rect {
    unsafe {
        Rect::new(
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    }
}

/// Copies part of the desktop. The mouse pointer is not included.
pub fn grab(r: Rect) -> Result<Image> {
    unsafe {
        let screen = GetDC(None);
        let mem = CreateCompatibleDC(Some(screen));
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: r.w,
                biHeight: -r.h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        let dib = CreateDIBSection(Some(screen), &bmi, DIB_RGB_COLORS, &mut bits, None, 0)?;
        let old = SelectObject(mem, dib.into());
        let copied = BitBlt(mem, 0, 0, r.w, r.h, Some(screen), r.x, r.y, SRCCOPY | CAPTUREBLT);
        let mut bgra = Vec::new();
        if copied.is_ok() {
            let len = r.w as usize * r.h as usize * 4;
            bgra = std::slice::from_raw_parts(bits as *const u8, len).to_vec();
            for px in bgra.chunks_exact_mut(4) {
                px[3] = 255; // GDI leaves the alpha byte at zero.
            }
        }
        SelectObject(mem, old);
        let _ = DeleteObject(dib.into());
        let _ = DeleteDC(mem);
        ReleaseDC(None, screen);
        copied?;
        Ok(Image::new(r.w as u32, r.h as u32, bgra))
    }
}

/// Visible top-level windows from topmost to bottom, for "pick a window" mode.
pub fn visible_windows() -> Vec<Rect> {
    unsafe extern "system" fn each(hwnd: HWND, data: LPARAM) -> BOOL {
        unsafe {
            let list = &mut *(data.0 as *mut Vec<Rect>);
            if let Some(r) = window_bounds(hwnd) {
                list.push(r);
            }
            TRUE
        }
    }
    let mut list: Vec<Rect> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(each), LPARAM(&mut list as *mut _ as isize));
    }
    list
}

/// The window the user is working in, if it is one that can be captured.
pub fn active_window() -> Option<Rect> {
    unsafe { window_bounds(GetForegroundWindow()) }
}

unsafe fn window_bounds(hwnd: HWND) -> Option<Rect> {
    unsafe {
        if !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() {
            return None;
        }
        let ex = WINDOW_EX_STYLE(GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32);
        if ex.contains(WS_EX_TRANSPARENT) {
            return None; // Click-through overlays of other programs.
        }
        let mut cloaked = 0u32;
        let _ = DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, &mut cloaked as *mut _ as _, 4);
        if cloaked != 0 {
            return None; // Windows of other virtual desktops and suspended apps.
        }
        let mut class = [0u16; 64];
        let n = GetClassNameW(hwnd, &mut class) as usize;
        let class = String::from_utf16_lossy(&class[..n]);
        if class == "Progman" || class == "WorkerW" || class.starts_with("Kadr") {
            return None;
        }
        if ex.contains(WS_EX_TOOLWINDOW) && GetWindowTextLengthW(hwnd) == 0 {
            return None;
        }
        let mut r = RECT::default();
        DwmGetWindowAttribute(hwnd, DWMWA_EXTENDED_FRAME_BOUNDS, &mut r as *mut _ as _, size_of::<RECT>() as u32)
            .ok()?;
        let rect = rect_of(r);
        (rect.w >= 8 && rect.h >= 8).then_some(rect)
    }
}
