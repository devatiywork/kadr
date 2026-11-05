//! The background part of the app: tray icon, key presses, and what happens to
//! the result of a capture.

use crate::hook::{self, DOUBLE_PRESS_MS, DoubleTap, TAP_ACTIVATE, TAP_DOUBLE, Tap};
use crate::image::Image;
use crate::overlay::{self, Mode, Outcome};
use crate::settings;
use crate::win::{self, *};
use std::cell::{Cell, RefCell};
use std::path::Path;
use windows::Win32::Foundation::*;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::*;

pub const HIDDEN_CLASS: PCWSTR = w!("KadrHidden");
const TRAY_ID: u32 = 1;
const HOTKEY_ID: i32 = 1;

const CMD_SHOT: usize = 1;
const CMD_VIDEO: usize = 2;
const CMD_STOP: usize = 3;
const CMD_SHOTS_DIR: usize = 4;
const CMD_VIDEOS_DIR: usize = 5;
const CMD_SETTINGS: usize = 6;
const CMD_EXIT: usize = 7;

thread_local! {
    static HIDDEN: Cell<HWND> = const { Cell::new(HWND(std::ptr::null_mut())) };
    static TASKBAR_CREATED: Cell<u32> = const { Cell::new(0) };
    /// Fallback path for Print presses, see `WM_HOTKEY`.
    static HOTKEY_TAP: RefCell<DoubleTap> = const { RefCell::new(DoubleTap::new(DOUBLE_PRESS_MS)) };
    static STANDALONE: Cell<bool> = const { Cell::new(false) };
    static OPEN_WINDOWS: Cell<u32> = const { Cell::new(0) };
}

pub fn hidden_window() -> HWND {
    HIDDEN.with(|h| h.get())
}

pub fn window_opened() {
    OPEN_WINDOWS.with(|n| n.set(n.get() + 1));
}

pub fn window_closed() {
    OPEN_WINDOWS.with(|n| n.set(n.get() - 1));
    if STANDALONE.with(|s| s.get()) && OPEN_WINDOWS.with(|n| n.get()) == 0 {
        unsafe { PostQuitMessage(0) };
    }
}

fn tray_data(hwnd: HWND) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_ID,
        ..Default::default()
    }
}

fn set_tip(data: &mut NOTIFYICONDATAW, text: &str) {
    for (slot, c) in data.szTip.iter_mut().zip(text.encode_utf16().chain(Some(0))) {
        *slot = c;
    }
}

fn tray_add(hwnd: HWND) {
    let mut data = tray_data(hwnd);
    data.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
    data.uCallbackMessage = WM_KADR_TRAY;
    data.hIcon = win::app_icon(unsafe { GetSystemMetrics(SM_CXSMICON) });
    set_tip(&mut data, &tray_tip());
    unsafe {
        let _ = Shell_NotifyIconW(NIM_ADD, &data);
    }
}

fn tray_tip() -> String {
    format!("Kadr: {}", settings::get().hotkey.action_label())
}

/// Makes the keyboard hook, the fallback hotkey and the tray follow the settings.
pub fn apply_hotkey() {
    let key = settings::get().hotkey;
    hook::set_hotkey(key);
    let hwnd = hidden_window();
    let mut modifiers = MOD_NOREPEAT;
    for (on, flag) in [(key.ctrl, MOD_CONTROL), (key.alt, MOD_ALT), (key.shift, MOD_SHIFT), (key.win, MOD_WIN)] {
        if on {
            modifiers |= flag;
        }
    }
    let mut data = tray_data(hwnd);
    data.uFlags = NIF_TIP;
    set_tip(&mut data, &tray_tip());
    unsafe {
        let _ = UnregisterHotKey(Some(hwnd), HOTKEY_ID);
        if let Err(e) = RegisterHotKey(Some(hwnd), HOTKEY_ID, modifiers, key.vk) {
            // Only the fallback for administrator windows is lost.
            crate::paths::log(&format!("hotkey {} is not available: {e}", key.label()));
        }
        let _ = Shell_NotifyIconW(NIM_MODIFY, &data);
    }
}

fn tray_remove(hwnd: HWND) {
    let data = tray_data(hwnd);
    unsafe {
        let _ = Shell_NotifyIconW(NIM_DELETE, &data);
    }
}

fn open_folder(path: &Path) {
    let _ = std::fs::create_dir_all(path);
    let wide_path = win::wide(&path.to_string_lossy());
    unsafe {
        ShellExecuteW(None, w!("open"), PCWSTR(wide_path.as_ptr()), None, None, SW_SHOWNORMAL);
    }
}

fn add_menu_item(menu: HMENU, id: usize, text: &str, checked: bool) {
    let text = win::wide(text);
    let flags = if checked { MF_STRING | MF_CHECKED } else { MF_STRING };
    unsafe {
        let _ = AppendMenuW(menu, flags, id, PCWSTR(text.as_ptr()));
    }
}

fn show_menu(hwnd: HWND) {
    unsafe {
        let Ok(menu) = CreatePopupMenu() else {
            return;
        };
        if crate::recui::is_active() {
            add_menu_item(menu, CMD_STOP, "Остановить запись", false);
        } else {
            let shot = format!("Сделать снимок\t{}", settings::get().hotkey.action_label());
            add_menu_item(menu, CMD_SHOT, &shot, false);
            add_menu_item(menu, CMD_VIDEO, "Записать видео", false);
        }
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
        add_menu_item(menu, CMD_SHOTS_DIR, "Папка со снимками", false);
        add_menu_item(menu, CMD_VIDEOS_DIR, "Папка с видео", false);
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
        add_menu_item(menu, CMD_SETTINGS, "Настройки", false);
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
        add_menu_item(menu, CMD_EXIT, "Выход", false);

        let (x, y) = win::cursor_pos();
        // Without this the menu would not close when clicking elsewhere.
        win::force_foreground(hwnd);
        let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON, x, y, None, hwnd, None);
        let _ = DestroyMenu(menu);
        run_command(hwnd, cmd.0 as usize);
    }
}

fn run_command(hwnd: HWND, cmd: usize) {
    match cmd {
        CMD_SHOT => open_overlay(Mode::Photo),
        CMD_VIDEO => open_overlay(Mode::Video),
        CMD_STOP => crate::recui::stop(),
        CMD_SHOTS_DIR => open_folder(&crate::paths::screenshots_dir()),
        CMD_VIDEOS_DIR => open_folder(&crate::paths::recordings_dir()),
        CMD_SETTINGS => crate::prefs::open(),
        CMD_EXIT => unsafe {
            tray_remove(hwnd);
            PostQuitMessage(0);
        },
        _ => {}
    }
}

pub fn open_overlay(mode: Mode) {
    if crate::recui::is_active() || overlay::is_open() || crate::delay::is_active() {
        return;
    }
    overlay::open(mode, hidden_window());
}

fn on_tap(tap: Tap) {
    let opens = matches!(tap, Tap::Double | Tap::Activate);
    if crate::delay::is_active() {
        if opens {
            crate::delay::cancel();
        }
    } else if overlay::is_open() {
        if matches!(tap, Tap::Single | Tap::Activate) {
            overlay::capture_full_screen();
        }
    } else if crate::recui::is_active() {
        if opens {
            crate::recui::stop();
        }
    } else if opens {
        open_overlay(Mode::Photo);
    }
}

fn on_photo(img: Image) {
    if let Err(e) = crate::clipboard::set_image(&img) {
        crate::paths::log(&format!("clipboard: {e}"));
    }
    let s = settings::get();
    if s.open_editor {
        crate::editor::open(img.clone());
    }
    crate::notify::screenshot_taken(img, s.autosave, !s.open_editor);
}

unsafe extern "system" fn hidden_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_KADR_TAP => on_tap(match wp.0 {
                TAP_DOUBLE => Tap::Double,
                TAP_ACTIVATE => Tap::Activate,
                _ => Tap::Single,
            }),
            WM_KADR_OPEN => {
                if crate::recui::is_active() {
                    crate::recui::stop();
                } else {
                    open_overlay(Mode::Photo);
                }
            }
            // Keyboard hooks do not see keys pressed while a window running as
            // administrator is active, but registered hotkeys still arrive.
            WM_HOTKEY => {
                if settings::get().hotkey.double {
                    let now = GetMessageTime() as u32;
                    let tap = HOTKEY_TAP.with(|t| {
                        let mut t = t.borrow_mut();
                        t.key_up();
                        t.key_down(now)
                    });
                    on_tap(tap);
                } else {
                    on_tap(Tap::Activate);
                }
            }
            WM_KADR_TRAY => match lp.0 as u32 {
                WM_LBUTTONUP => open_overlay(Mode::Photo),
                WM_RBUTTONUP => show_menu(hwnd),
                _ => {}
            },
            WM_KADR_OVERLAY_DONE => match overlay::take_outcome() {
                Some(Outcome::Photo { img }) => on_photo(img),
                Some(Outcome::Video { monitor, region, thumb, mic, system }) => {
                    crate::recui::begin(monitor, region, thumb, mic, system, hwnd);
                }
                Some(Outcome::OpenSettings) => crate::prefs::open(),
                Some(Outcome::Text { img }) => crate::ocr::copy_text(img),
                Some(Outcome::Color { value, rgb }) => crate::notify::color_copied(value, rgb),
                Some(Outcome::Delay { seconds, mode }) => crate::delay::start(seconds, mode),
                Some(Outcome::Cancel) | None => {}
            },
            WM_KADR_REC_DONE => crate::recui::finished(),
            // The installer asks a running copy to make room for the new one.
            WM_CLOSE => run_command(hwnd, CMD_EXIT),
            WM_DESTROY => tray_remove(hwnd),
            _ if msg == TASKBAR_CREATED.with(|t| t.get()) => tray_add(hwnd),
            _ => return DefWindowProcW(hwnd, msg, wp, lp),
        }
        LRESULT(0)
    }
}

/// Starts a new capture in the tray copy of the app; an editor may run in a process of its own.
pub fn request_capture() {
    unsafe {
        if let Ok(hwnd) = FindWindowW(HIDDEN_CLASS, None) {
            let _ = PostMessageW(Some(hwnd), WM_KADR_OPEN, WPARAM(0), LPARAM(0));
        }
    }
}

/// Normal start: lives in the tray until "Exit".
pub fn run() {
    unsafe {
        let _mutex = CreateMutexW(None, false, w!("Local\\KadrSingleInstance"));
        if GetLastError() == ERROR_ALREADY_EXISTS {
            // Starting Kadr again just opens the capture in the running copy.
            if let Ok(hwnd) = FindWindowW(HIDDEN_CLASS, None) {
                let _ = PostMessageW(Some(hwnd), WM_KADR_OPEN, WPARAM(0), LPARAM(0));
            }
            return;
        }

        win::enable_dark_menus();
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(hidden_proc),
            hInstance: win::hinstance(),
            lpszClassName: HIDDEN_CLASS,
            ..Default::default()
        };
        RegisterClassExW(&wc);
        let hwnd = match CreateWindowExW(
            WINDOW_EX_STYLE(0),
            HIDDEN_CLASS,
            w!("Kadr"),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(win::hinstance()),
            None,
        ) {
            Ok(h) => h,
            Err(e) => {
                win::show_error(&format!("Не удалось запустить Kadr: {e}"));
                return;
            }
        };
        HIDDEN.with(|h| h.set(hwnd));
        TASKBAR_CREATED.with(|t| t.set(RegisterWindowMessageW(w!("TaskbarCreated"))));
        tray_add(hwnd);
        crate::notify::register();
        hook::start(hwnd);
        apply_hotkey();
        message_loop();
        let _ = UnregisterHotKey(Some(hwnd), HOTKEY_ID);
    }
}

/// `kadr --edit file.png`: just the editor, without the tray.
pub fn run_editor(path: &Path) {
    STANDALONE.with(|s| s.set(true));
    match Image::from_png_file(path) {
        Ok(img) => {
            crate::editor::open(img);
            message_loop();
        }
        Err(e) => win::show_error(&format!("Не удалось открыть {}: {e}", path.display())),
    }
}

/// `kadr:video?...`: just the player, without the tray.
pub fn run_player(path: &Path) {
    STANDALONE.with(|s| s.set(true));
    win::enable_dark_menus();
    crate::player::open(path.to_path_buf());
    if OPEN_WINDOWS.with(|n| n.get()) > 0 {
        message_loop();
    }
}

fn message_loop() {
    unsafe {
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}
