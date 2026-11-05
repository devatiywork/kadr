//! Small helpers around raw Win32 windows.

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::UI::Controls::MARGINS;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{GetDpiForWindow, GetSystemMetricsForDpi};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::*;

pub const WM_KADR_TAP: u32 = WM_APP + 1;
pub const WM_KADR_OPEN: u32 = WM_APP + 2;
pub const WM_KADR_TRAY: u32 = WM_APP + 3;
pub const WM_KADR_OVERLAY_DONE: u32 = WM_APP + 4;
pub const WM_KADR_REC_DONE: u32 = WM_APP + 5;
pub const WM_KADR_KEY_CAPTURED: u32 = WM_APP + 6;

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn hinstance() -> HINSTANCE {
    unsafe { GetModuleHandleW(None).map(|m| HINSTANCE(m.0)).unwrap_or_default() }
}

pub fn app_icon(size: i32) -> HICON {
    unsafe {
        LoadImageW(Some(hinstance()), PCWSTR(1 as _), IMAGE_ICON, size, size, LR_DEFAULTCOLOR)
            .map(|h| HICON(h.0))
            .unwrap_or_default()
    }
}

/// Signed client coordinates from a mouse message.
pub fn mouse_xy(lp: LPARAM) -> (i32, i32) {
    let v = lp.0 as u32;
    ((v & 0xffff) as u16 as i16 as i32, (v >> 16) as u16 as i16 as i32)
}

pub fn cursor_pos() -> (i32, i32) {
    let mut p = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut p);
    }
    (p.x, p.y)
}

pub fn client_size(hwnd: HWND) -> (i32, i32) {
    let mut r = RECT::default();
    unsafe {
        let _ = GetClientRect(hwnd, &mut r);
    }
    (r.right - r.left, r.bottom - r.top)
}

pub fn dpi_scale(hwnd: HWND) -> f32 {
    unsafe { GetDpiForWindow(hwnd) as f32 / 96.0 }.max(1.0)
}

pub fn invalidate(hwnd: HWND) {
    unsafe {
        let _ = InvalidateRect(Some(hwnd), None, false);
    }
}

pub fn key_down(vk: VIRTUAL_KEY) -> bool {
    unsafe { GetKeyState(vk.0 as i32) as u16 & 0x8000 != 0 }
}

/// Windows refuses the foreground to a background process unless it was the last
/// one to produce input, so an empty input event is sent first.
pub fn force_foreground(hwnd: HWND) {
    unsafe {
        let input = INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 { mi: MOUSEINPUT::default() },
        };
        SendInput(&[input], size_of::<INPUT>() as i32);
        let _ = SetForegroundWindow(hwnd);
        let _ = SetFocus(Some(hwnd));
    }
}

/// Width of the minimize, maximize and close buttons that Windows draws, in DIPs.
pub const CAPTION_BUTTONS_DIP: f32 = 138.0;

/// The Windows 11 window look: Mica under the whole window and a title bar that merges
/// with the client area, so the app's own toolbar sits next to the window buttons.
/// Such a window needs `frame_message` in its message handler.
pub fn fluent_frame(hwnd: HWND) {
    let on: i32 = 1;
    let backdrop = DWMSBT_MAINWINDOW;
    // "No color": stays see-through even when accent-colored title bars are turned on.
    let caption = COLORREF(0xFFFF_FFFE);
    let margins = MARGINS { cxLeftWidth: -1, cxRightWidth: -1, cyTopHeight: -1, cyBottomHeight: -1 };
    unsafe {
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, &on as *const _ as _, size_of::<i32>() as u32);
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_SYSTEMBACKDROP_TYPE,
            &backdrop as *const _ as _,
            size_of::<DWM_SYSTEMBACKDROP_TYPE>() as u32,
        );
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_CAPTION_COLOR, &caption as *const _ as _, size_of::<COLORREF>() as u32);
        let _ = DwmExtendFrameIntoClientArea(hwnd, &margins);
        let _ = SetWindowPos(hwnd, None, 0, 0, 0, 0, SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
    }
}

fn frame_thickness(hwnd: HWND) -> i32 {
    unsafe {
        let dpi = GetDpiForWindow(hwnd);
        GetSystemMetricsForDpi(SM_CYFRAME, dpi) + GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi)
    }
}

/// Non-client behavior of a `fluent_frame` window. `is_drag_area` says whether a client
/// point in the top `caption_height` pixels moves the window rather than hitting a control.
pub fn frame_message(
    hwnd: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
    caption_height: i32,
    resizable: bool,
    is_drag_area: impl Fn(i32, i32) -> bool,
) -> Option<LRESULT> {
    unsafe {
        // Hover, press and clicks of the window buttons drawn by Windows.
        let mut handled = LRESULT(0);
        if DwmDefWindowProc(hwnd, msg, wp, lp, &mut handled).as_bool() {
            return Some(handled);
        }
        match msg {
            // Keeps the side and bottom borders but gives the title bar to the client area.
            WM_NCCALCSIZE if wp.0 != 0 => {
                let params = &mut *(lp.0 as *mut NCCALCSIZE_PARAMS);
                let top = params.rgrc[0].top;
                let result = DefWindowProcW(hwnd, msg, wp, lp);
                let maximized = IsZoomed(hwnd).as_bool();
                params.rgrc[0].top = top + if maximized { frame_thickness(hwnd) } else { 0 };
                Some(result)
            }
            WM_NCHITTEST => {
                let result = DefWindowProcW(hwnd, msg, wp, lp);
                if result.0 != HTCLIENT as isize {
                    return Some(result);
                }
                let (sx, sy) = mouse_xy(lp);
                let mut p = POINT { x: sx, y: sy };
                let _ = ScreenToClient(hwnd, &mut p);
                if resizable && p.y < frame_thickness(hwnd) / 2 && !IsZoomed(hwnd).as_bool() {
                    return Some(LRESULT(HTTOP as isize));
                }
                if p.y < caption_height && is_drag_area(p.x, p.y) {
                    return Some(LRESULT(HTCAPTION as isize));
                }
                Some(LRESULT(HTCLIENT as isize))
            }
            _ => None,
        }
    }
}

/// Dark context menus, like the rest of the app. The system offers this only through
/// these unnamed uxtheme exports; on a Windows without them menus simply stay light.
pub fn enable_dark_menus() {
    type SetPreferredAppMode = unsafe extern "system" fn(i32) -> i32;
    type FlushMenuThemes = unsafe extern "system" fn();
    const FORCE_DARK: i32 = 2;
    unsafe {
        let Ok(theme) = LoadLibraryW(w!("uxtheme.dll")) else {
            return;
        };
        if let Some(f) = GetProcAddress(theme, PCSTR(135 as _)) {
            let set: SetPreferredAppMode = std::mem::transmute(f);
            set(FORCE_DARK);
        }
        if let Some(f) = GetProcAddress(theme, PCSTR(136 as _)) {
            let flush: FlushMenuThemes = std::mem::transmute(f);
            flush();
        }
    }
}

pub fn round_corners(hwnd: HWND) {
    let pref = DWMWCP_ROUND;
    unsafe {
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &pref as *const _ as _,
            size_of::<DWM_WINDOW_CORNER_PREFERENCE>() as u32,
        );
    }
}

/// Keeps the recording controls out of the recording itself. Debug builds leave
/// them visible so that test screenshots can show them.
pub fn exclude_from_capture(hwnd: HWND) {
    if !cfg!(debug_assertions) {
        unsafe {
            let _ = SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE);
        }
    }
}

pub fn message_box(hwnd: Option<HWND>, text: &str, flags: MESSAGEBOX_STYLE) -> MESSAGEBOX_RESULT {
    let text = wide(text);
    unsafe { MessageBoxW(hwnd, PCWSTR(text.as_ptr()), w!("Kadr"), flags) }
}

pub fn show_error(text: &str) {
    crate::paths::log(text);
    message_box(None, text, MB_ICONERROR | MB_OK | MB_TOPMOST);
}

/// A window whose state lives in a heap box owned by the window itself.
pub trait WndState {
    fn on_message(&mut self, hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT>;
}

unsafe extern "system" fn state_proc<T: WndState>(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if msg == WM_NCCREATE {
            let cs = &*(lp.0 as *const CREATESTRUCTW);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize);
            return DefWindowProcW(hwnd, msg, wp, lp);
        }
        let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut T;
        if state.is_null() {
            return DefWindowProcW(hwnd, msg, wp, lp);
        }
        if msg == WM_NCDESTROY {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            drop(Box::from_raw(state));
            return DefWindowProcW(hwnd, msg, wp, lp);
        }
        match (*state).on_message(hwnd, msg, wp, lp) {
            Some(result) => result,
            None => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

pub struct WindowSpec {
    pub class: PCWSTR,
    pub title: PCWSTR,
    pub style: WINDOW_STYLE,
    pub ex_style: WINDOW_EX_STYLE,
    pub rect: crate::geom::Rect,
}

/// Registers the class on first use and creates a window that owns `state`.
pub fn create_window<T: WndState>(spec: WindowSpec, state: T) -> Result<HWND> {
    unsafe {
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(state_proc::<T>),
            hInstance: hinstance(),
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            hIcon: app_icon(32),
            hIconSm: app_icon(16),
            hbrBackground: HBRUSH(std::ptr::null_mut()),
            lpszClassName: spec.class,
            ..Default::default()
        };
        // Fails harmlessly when the class already exists.
        RegisterClassExW(&wc);
        let boxed = Box::into_raw(Box::new(state));
        let created = CreateWindowExW(
            spec.ex_style,
            spec.class,
            spec.title,
            spec.style,
            spec.rect.x,
            spec.rect.y,
            spec.rect.w,
            spec.rect.h,
            None,
            None,
            Some(hinstance()),
            Some(boxed as _),
        );
        if created.is_err() {
            // WM_NCCREATE never stored the pointer if creation failed that early.
            drop(Box::from_raw(boxed));
        }
        created
    }
}
