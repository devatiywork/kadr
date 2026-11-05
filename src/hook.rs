//! Global keyboard handling. The configured hotkey (double Print by default) is
//! swallowed, so a stray press opens nothing and reaches no other program.

use crate::win::{WM_KADR_KEY_CAPTURED, WM_KADR_OPEN, WM_KADR_TAP};
use serde::{Deserialize, Serialize};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, AtomicU64, Ordering};
use windows::Win32::Foundation::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

pub const DOUBLE_PRESS_MS: u32 = 400;
pub const TAP_SINGLE: usize = 1;
pub const TAP_DOUBLE: usize = 2;
pub const TAP_ACTIVATE: usize = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tap {
    /// Key auto-repeat while held down: not a new press.
    Repeat,
    Single,
    Double,
    /// A press of a hotkey that is set to work with one press.
    Activate,
}

/// The key that opens the capture, with its modifiers.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Hotkey {
    pub vk: u32,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub win: bool,
    /// Needs two quick presses, which protects against accidental ones.
    pub double: bool,
}

impl Default for Hotkey {
    fn default() -> Self {
        Self { vk: VK_SNAPSHOT.0 as u32, ctrl: false, alt: false, shift: false, win: false, double: true }
    }
}

impl Hotkey {
    fn pack(&self) -> u64 {
        self.vk as u64
            | (self.ctrl as u64) << 32
            | (self.alt as u64) << 33
            | (self.shift as u64) << 34
            | (self.win as u64) << 35
            | (self.double as u64) << 36
    }

    fn unpack(v: u64) -> Self {
        Self {
            vk: v as u32,
            ctrl: v >> 32 & 1 != 0,
            alt: v >> 33 & 1 != 0,
            shift: v >> 34 & 1 != 0,
            win: v >> 35 & 1 != 0,
            double: v >> 36 & 1 != 0,
        }
    }

    /// Message value of a key caught in the settings window.
    pub fn from_captured(wp: usize, double: bool) -> Option<Self> {
        (wp != 0).then(|| Self { double, ..Self::unpack(wp as u64) })
    }

    fn same_modifiers(&self, ctrl: bool, alt: bool, shift: bool, win: bool) -> bool {
        self.ctrl == ctrl && self.alt == alt && self.shift == shift && self.win == win
    }

    /// Keys used for typing need Ctrl, Alt or Win: alone or with Shift they would stop
    /// working everywhere else. Print, Pause, Scroll Lock, Insert and F-keys are free.
    pub fn is_allowed(&self) -> bool {
        if self.vk == VK_ESCAPE.0 as u32 || is_modifier(self.vk) {
            return false;
        }
        let free = self.vk == VK_SNAPSHOT.0 as u32
            || self.vk == VK_PAUSE.0 as u32
            || self.vk == VK_SCROLL.0 as u32
            || self.vk == VK_INSERT.0 as u32
            || (VK_F1.0 as u32..=VK_F24.0 as u32).contains(&self.vk);
        free || self.ctrl || self.alt || self.win
    }

    /// For example `Ctrl + Shift + F9`.
    pub fn label(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        for (on, name) in [(self.ctrl, "Ctrl"), (self.alt, "Alt"), (self.shift, "Shift"), (self.win, "Win")] {
            if on {
                parts.push(name.to_string());
            }
        }
        parts.push(key_name(self.vk));
        parts.join(" + ")
    }

    /// For example `Print × 2`.
    pub fn action_label(&self) -> String {
        if self.double { format!("{} × 2", self.label()) } else { self.label() }
    }
}

fn is_modifier(vk: u32) -> bool {
    [
        VK_SHIFT, VK_LSHIFT, VK_RSHIFT, VK_CONTROL, VK_LCONTROL, VK_RCONTROL, VK_MENU, VK_LMENU, VK_RMENU, VK_LWIN,
        VK_RWIN,
    ]
    .iter()
    .any(|m| m.0 as u32 == vk)
}

fn key_name(vk: u32) -> String {
    match vk {
        v if v == VK_SNAPSHOT.0 as u32 => "Print".into(),
        v if v == VK_PAUSE.0 as u32 => "Pause".into(),
        v if v == VK_SCROLL.0 as u32 => "Scroll Lock".into(),
        v if v == VK_INSERT.0 as u32 => "Insert".into(),
        v if v == VK_DELETE.0 as u32 => "Delete".into(),
        v if v == VK_HOME.0 as u32 => "Home".into(),
        v if v == VK_END.0 as u32 => "End".into(),
        v if v == VK_PRIOR.0 as u32 => "Page Up".into(),
        v if v == VK_NEXT.0 as u32 => "Page Down".into(),
        v if v == VK_SPACE.0 as u32 => "Space".into(),
        v if v == VK_RETURN.0 as u32 => "Enter".into(),
        v if v == VK_TAB.0 as u32 => "Tab".into(),
        v if v == VK_BACK.0 as u32 => "Backspace".into(),
        v if (VK_F1.0 as u32..=VK_F24.0 as u32).contains(&v) => format!("F{}", v - VK_F1.0 as u32 + 1),
        v if (0x30..=0x39).contains(&v) || (0x41..=0x5A).contains(&v) => char::from_u32(v).unwrap_or('?').to_string(),
        v if (VK_NUMPAD0.0 as u32..=VK_NUMPAD9.0 as u32).contains(&v) => format!("Num {}", v - VK_NUMPAD0.0 as u32),
        v => system_key_name(v),
    }
}

/// The name Windows gives a key, for punctuation and everything else.
fn system_key_name(vk: u32) -> String {
    unsafe {
        let scan = MapVirtualKeyW(vk, MAPVK_VK_TO_VSC);
        let mut name = [0u16; 32];
        let n = GetKeyNameTextW((scan << 16) as i32, &mut name);
        if n > 0 { String::from_utf16_lossy(&name[..n as usize]) } else { format!("Клавиша {vk}") }
    }
}

/// Turns key events into single and double presses.
#[derive(Debug)]
pub struct DoubleTap {
    window_ms: u32,
    last_press: Option<u32>,
    held: bool,
}

impl DoubleTap {
    pub const fn new(window_ms: u32) -> Self {
        Self { window_ms, last_press: None, held: false }
    }

    /// `now_ms` is the system tick count, which wraps around every 49 days.
    pub fn key_down(&mut self, now_ms: u32) -> Tap {
        if self.held {
            return Tap::Repeat;
        }
        self.held = true;
        match self.last_press {
            Some(prev) if now_ms.wrapping_sub(prev) <= self.window_ms => {
                self.last_press = None;
                Tap::Double
            }
            _ => {
                self.last_press = Some(now_ms);
                Tap::Single
            }
        }
    }

    pub fn key_up(&mut self) {
        self.held = false;
    }
}

static TAP: Mutex<DoubleTap> = Mutex::new(DoubleTap::new(DOUBLE_PRESS_MS));
static TARGET: AtomicIsize = AtomicIsize::new(0);
static HOTKEY: AtomicU64 = AtomicU64::new(DEFAULT_PACKED);
static KEY_SWALLOWED: AtomicBool = AtomicBool::new(false);
/// While not zero, the next key goes to this window instead of working as usual.
static CAPTURE_WINDOW: AtomicIsize = AtomicIsize::new(0);
/// A key whose press was caught for the settings: its release is swallowed too.
static CAPTURED_KEY: AtomicU32 = AtomicU32::new(0);
/// Print pressed twice, the same value as `Hotkey::default().pack()`.
const DEFAULT_PACKED: u64 = VK_SNAPSHOT.0 as u64 | 1 << 36;

pub fn set_hotkey(hotkey: Hotkey) {
    HOTKEY.store(hotkey.pack(), Ordering::Relaxed);
}

/// The next key pressed anywhere is sent to `window` as `WM_KADR_KEY_CAPTURED`:
/// wParam 0 means Esc, otherwise the packed key.
pub fn begin_capture(window: HWND) {
    CAPTURE_WINDOW.store(window.0 as isize, Ordering::Relaxed);
}

pub fn cancel_capture() {
    CAPTURE_WINDOW.store(0, Ordering::Relaxed);
}

fn post(msg: u32, wp: usize) {
    let hwnd = HWND(TARGET.load(Ordering::Relaxed) as *mut _);
    unsafe {
        let _ = PostMessageW(Some(hwnd), msg, WPARAM(wp), LPARAM(0));
    }
}

fn is_down(vk: VIRTUAL_KEY) -> bool {
    unsafe { GetAsyncKeyState(vk.0 as i32) as u16 & 0x8000 != 0 }
}

/// An unassigned key sent to make Windows forget a lone Win or Alt press.
const MASK_VK: VIRTUAL_KEY = VIRTUAL_KEY(0xE8);

/// Stops the Start menu or a window's menu bar from opening when a Win+key or Alt+key
/// combination was swallowed.
fn mask_win_key() {
    let key = |flags| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT { wVk: MASK_VK, dwFlags: flags, ..Default::default() },
        },
    };
    unsafe {
        SendInput(&[key(KEYBD_EVENT_FLAGS(0)), key(KEYEVENTF_KEYUP)], size_of::<INPUT>() as i32);
    }
}

fn modifiers_down() -> (bool, bool, bool, bool) {
    (
        is_down(VK_CONTROL),
        is_down(VK_MENU),
        is_down(VK_SHIFT),
        is_down(VK_LWIN) || is_down(VK_RWIN),
    )
}

/// Handles a key while the settings window waits for a new hotkey.
/// Returns true when the key is used up.
fn capture_key(vk: u32, down: bool) -> bool {
    let window = CAPTURE_WINDOW.load(Ordering::Relaxed);
    if window == 0 || !down || is_modifier(vk) || vk == MASK_VK.0 as u32 {
        return false;
    }
    CAPTURE_WINDOW.store(0, Ordering::Relaxed);
    CAPTURED_KEY.store(vk, Ordering::Relaxed);
    let (ctrl, alt, shift, win) = modifiers_down();
    let key = Hotkey { vk, ctrl, alt, shift, win, double: false };
    if alt || win {
        mask_win_key();
    }
    let wp = if vk == VK_ESCAPE.0 as u32 { 0 } else { key.pack() as usize };
    unsafe {
        let _ = PostMessageW(Some(HWND(window as *mut _)), WM_KADR_KEY_CAPTURED, WPARAM(wp), LPARAM(0));
    }
    true
}

unsafe extern "system" fn hook_proc(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if code == HC_ACTION as i32 {
            let k = &*(lp.0 as *const KBDLLHOOKSTRUCT);
            let msg = wp.0 as u32;
            let down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;

            if capture_key(k.vkCode, down) {
                return LRESULT(1);
            }
            if !down && CAPTURED_KEY.load(Ordering::Relaxed) == k.vkCode {
                CAPTURED_KEY.store(0, Ordering::Relaxed);
                return LRESULT(1);
            }

            let (ctrl, alt, shift, win) = modifiers_down();
            let hotkey = Hotkey::unpack(HOTKEY.load(Ordering::Relaxed));
            // Windows has its own capture tool built into the system, which cannot be
            // removed and opens on plain Print. A stray press must stay harmless even
            // when another key is chosen as the hotkey.
            let plain_print = k.vkCode == VK_SNAPSHOT.0 as u32 && !ctrl && !alt && !shift && !win;
            if plain_print && hotkey.vk != k.vkCode {
                return LRESULT(1);
            }
            if k.vkCode == hotkey.vk {
                if down {
                    // With other modifiers this is another shortcut, such as Alt+Print.
                    if !hotkey.same_modifiers(ctrl, alt, shift, win) && !KEY_SWALLOWED.load(Ordering::Relaxed) {
                        return CallNextHookEx(None, code, wp, lp);
                    }
                    KEY_SWALLOWED.store(true, Ordering::Relaxed);
                    let tap = TAP.lock().map(|mut t| t.key_down(k.time)).unwrap_or(Tap::Repeat);
                    if tap != Tap::Repeat {
                        if hotkey.alt || hotkey.win {
                            mask_win_key();
                        }
                        match (hotkey.double, tap) {
                            (false, _) => post(WM_KADR_TAP, TAP_ACTIVATE),
                            (true, Tap::Double) => post(WM_KADR_TAP, TAP_DOUBLE),
                            (true, _) => post(WM_KADR_TAP, TAP_SINGLE),
                        }
                    }
                    return LRESULT(1);
                }
                if KEY_SWALLOWED.swap(false, Ordering::Relaxed) {
                    if let Ok(mut t) = TAP.lock() {
                        t.key_up();
                    }
                    return LRESULT(1);
                }
            } else if down && k.vkCode == 'S' as u32 && win && shift && !ctrl && !alt {
                // The system shortcut of the standard tool now opens Kadr.
                mask_win_key();
                post(WM_KADR_OPEN, 0);
                return LRESULT(1);
            }
        }
        CallNextHookEx(None, code, wp, lp)
    }
}

/// The hook runs on its own thread: a low-level hook that answers slowly gets
/// removed by Windows, so it must never wait for UI work.
pub fn start(target: HWND) {
    TARGET.store(target.0 as isize, Ordering::Relaxed);
    std::thread::Builder::new()
        .name("kadr-hook".into())
        .spawn(|| unsafe {
            let module = GetModuleHandleW(None).ok().map(|m| HINSTANCE(m.0));
            if let Err(e) = SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), module, 0) {
                crate::paths::log(&format!("keyboard hook failed: {e}"));
                return;
            }
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {}
        })
        .expect("failed to start the keyboard thread");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(t: &mut DoubleTap, at: u32) -> Tap {
        let r = t.key_down(at);
        t.key_up();
        r
    }

    #[test]
    fn one_press_is_only_a_single() {
        let mut t = DoubleTap::new(400);
        assert_eq!(press(&mut t, 1000), Tap::Single);
    }

    #[test]
    fn quick_second_press_is_a_double() {
        let mut t = DoubleTap::new(400);
        press(&mut t, 1000);
        assert_eq!(press(&mut t, 1300), Tap::Double);
    }

    #[test]
    fn slow_second_press_starts_over() {
        let mut t = DoubleTap::new(400);
        press(&mut t, 1000);
        assert_eq!(press(&mut t, 1401), Tap::Single);
        assert_eq!(press(&mut t, 1700), Tap::Double);
    }

    #[test]
    fn holding_the_key_is_not_a_double() {
        let mut t = DoubleTap::new(400);
        assert_eq!(t.key_down(1000), Tap::Single);
        assert_eq!(t.key_down(1030), Tap::Repeat);
        assert_eq!(t.key_down(1060), Tap::Repeat);
        t.key_up();
        assert_eq!(t.key_down(2000), Tap::Single);
    }

    #[test]
    fn third_press_after_a_double_is_single_again() {
        let mut t = DoubleTap::new(400);
        press(&mut t, 1000);
        assert_eq!(press(&mut t, 1100), Tap::Double);
        assert_eq!(press(&mut t, 1200), Tap::Single);
    }

    #[test]
    fn default_hotkey_is_double_print() {
        let k = Hotkey::default();
        assert_eq!(k.pack(), DEFAULT_PACKED);
        assert_eq!(k.action_label(), "Print × 2");
    }

    #[test]
    fn hotkey_survives_packing() {
        let k = Hotkey { vk: 0x78, ctrl: true, alt: false, shift: true, win: true, double: false };
        assert_eq!(Hotkey::unpack(k.pack()), k);
    }

    #[test]
    fn captured_key_keeps_the_chosen_press_count() {
        let k = Hotkey { vk: 0x78, ctrl: true, alt: false, shift: false, win: false, double: false };
        assert_eq!(Hotkey::from_captured(k.pack() as usize, true), Some(Hotkey { double: true, ..k }));
        assert_eq!(Hotkey::from_captured(0, true), None);
    }

    #[test]
    fn labels_read_naturally() {
        let k = Hotkey { vk: VK_F9.0 as u32, ctrl: true, alt: false, shift: true, win: false, double: true };
        assert_eq!(k.label(), "Ctrl + Shift + F9");
        assert_eq!(k.action_label(), "Ctrl + Shift + F9 × 2");
        assert_eq!(Hotkey { double: false, ..k }.action_label(), "Ctrl + Shift + F9");
        let letter = Hotkey { vk: 'K' as u32, win: true, ..Hotkey::default() };
        assert_eq!(letter.label(), "Win + K");
    }

    #[test]
    fn typing_keys_need_a_real_modifier() {
        let bare = |vk: u32| Hotkey { vk, ..Hotkey::default() };
        assert!(!bare('A' as u32).is_allowed());
        assert!(!bare('5' as u32).is_allowed());
        assert!(!bare(VK_SPACE.0 as u32).is_allowed());
        assert!(!bare(VK_RETURN.0 as u32).is_allowed());
        // Shift + letter is just a capital letter.
        assert!(!Hotkey { shift: true, ..bare('A' as u32) }.is_allowed());
        assert!(Hotkey { ctrl: true, ..bare('A' as u32) }.is_allowed());
        assert!(Hotkey { win: true, ..bare('A' as u32) }.is_allowed());
    }

    #[test]
    fn free_keys_work_alone() {
        for vk in [VK_SNAPSHOT, VK_PAUSE, VK_SCROLL, VK_INSERT, VK_F1, VK_F9, VK_F24] {
            assert!(Hotkey { vk: vk.0 as u32, ..Hotkey::default() }.is_allowed(), "{vk:?}");
        }
    }

    #[test]
    fn escape_and_lone_modifiers_are_refused() {
        assert!(!Hotkey { vk: VK_ESCAPE.0 as u32, ctrl: true, ..Hotkey::default() }.is_allowed());
        assert!(!Hotkey { vk: VK_LWIN.0 as u32, ctrl: true, ..Hotkey::default() }.is_allowed());
    }

    #[test]
    fn survives_tick_counter_wraparound() {
        let mut t = DoubleTap::new(400);
        press(&mut t, u32::MAX - 100);
        assert_eq!(press(&mut t, 100), Tap::Double);
    }
}
