//! User preferences, stored as JSON next to the log.

use crate::color::ColorFormat;
use crate::hook::Hotkey;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Mutex;
use windows::Win32::Foundation::*;
use windows::Win32::System::Registry::*;
use windows::core::*;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Quality {
    Standard,
    High,
    Max,
}

pub const FPS_CHOICES: [u32; 3] = [30, 60, 120];
const DEFAULT_FPS: u32 = 60;

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub mic: bool,
    pub system_audio: bool,
    pub autosave: bool,
    /// Annotation color as 0xRRGGBB and line width in picture pixels.
    pub ink_color: u32,
    pub ink_width: f32,
    pub quality: Quality,
    pub fps: u32,
    pub show_cursor: bool,
    pub countdown: bool,
    /// Device ids; `None` follows the default device chosen in Windows.
    pub mic_device: Option<String>,
    pub system_device: Option<String>,
    /// `None` keeps the standard folders.
    pub screenshots_dir: Option<PathBuf>,
    pub recordings_dir: Option<PathBuf>,
    pub open_editor: bool,
    pub hotkey: Hotkey,
    pub color_format: ColorFormat,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            mic: false,
            system_audio: true,
            autosave: true,
            ink_color: 0xE81224,
            ink_width: 5.0,
            quality: Quality::High,
            fps: DEFAULT_FPS,
            show_cursor: true,
            countdown: true,
            mic_device: None,
            system_device: None,
            screenshots_dir: None,
            recordings_dir: None,
            open_editor: false,
            hotkey: Hotkey::default(),
            color_format: ColorFormat::Hex,
        }
    }
}

impl Settings {
    /// The frame rate to record at, even if the file was edited by hand.
    pub fn valid_fps(&self) -> u32 {
        if FPS_CHOICES.contains(&self.fps) { self.fps } else { DEFAULT_FPS }
    }
}

static CURRENT: Mutex<Option<Settings>> = Mutex::new(None);

fn file() -> PathBuf {
    crate::paths::config_dir().join("settings.json")
}

fn load() -> Settings {
    std::fs::read_to_string(file())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn get() -> Settings {
    let mut current = CURRENT.lock().unwrap_or_else(|e| e.into_inner());
    current.get_or_insert_with(load).clone()
}

pub fn update(change: impl FnOnce(&mut Settings)) {
    let mut current = CURRENT.lock().unwrap_or_else(|e| e.into_inner());
    let settings = current.get_or_insert_with(load);
    change(settings);
    let path = file();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(json) = serde_json::to_string_pretty(settings) {
        let _ = std::fs::write(path, json);
    }
}

const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const RUN_VALUE: PCWSTR = w!("Kadr");

pub fn autostart_enabled() -> bool {
    unsafe {
        RegGetValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE, RRF_RT_REG_SZ, None, None, None) == ERROR_SUCCESS
    }
}

pub fn set_autostart(enabled: bool) {
    unsafe {
        let mut key = HKEY::default();
        if RegOpenKeyExW(HKEY_CURRENT_USER, RUN_KEY, None, KEY_SET_VALUE, &mut key) != ERROR_SUCCESS {
            return;
        }
        if enabled {
            if let Ok(exe) = std::env::current_exe() {
                let value = crate::win::wide(&format!("\"{}\"", exe.display()));
                let bytes = std::slice::from_raw_parts(value.as_ptr() as *const u8, value.len() * 2);
                let _ = RegSetValueExW(key, RUN_VALUE, None, REG_SZ, Some(bytes));
            }
        } else {
            let _ = RegDeleteValueW(key, RUN_VALUE);
        }
        let _ = RegCloseKey(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_saved_by_an_older_version_still_load() {
        let s: Settings = serde_json::from_str(r#"{"shape":"Window","mic":true}"#).unwrap();
        assert!(s.mic);
        assert_eq!(s.quality, Quality::High);
        assert_eq!(s.valid_fps(), 60);
        assert!(s.show_cursor && s.countdown);
        assert_eq!(s.mic_device, None);
        assert_eq!(s.hotkey, Hotkey::default());
    }

    #[test]
    fn unsupported_frame_rate_falls_back_to_the_default() {
        let s = Settings { fps: 77, ..Default::default() };
        assert_eq!(s.valid_fps(), 60);
        let s = Settings { fps: 120, ..Default::default() };
        assert_eq!(s.valid_fps(), 120);
    }
}
