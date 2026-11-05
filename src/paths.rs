//! Where Kadr keeps its files, plus a small log for errors nobody would see otherwise.

use std::io::Write;
use std::path::PathBuf;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::UI::Shell::*;
use windows::core::GUID;

fn known_folder(id: &GUID) -> Option<PathBuf> {
    unsafe {
        let p = SHGetKnownFolderPath(id, KNOWN_FOLDER_FLAG(0), None).ok()?;
        let path = p.to_string().ok().map(PathBuf::from);
        CoTaskMemFree(Some(p.0 as _));
        path
    }
}

/// The folder chosen in the settings, otherwise the standard one.
pub fn screenshots_dir() -> PathBuf {
    crate::settings::get().screenshots_dir.unwrap_or_else(default_screenshots_dir)
}

pub fn recordings_dir() -> PathBuf {
    crate::settings::get().recordings_dir.unwrap_or_else(default_recordings_dir)
}

pub fn default_screenshots_dir() -> PathBuf {
    known_folder(&FOLDERID_Screenshots)
        .or_else(|| known_folder(&FOLDERID_Pictures).map(|p| p.join("Screenshots")))
        .unwrap_or_else(|| PathBuf::from("Screenshots"))
}

pub fn default_recordings_dir() -> PathBuf {
    // The folder the standard tool used, so all recordings stay in one place.
    known_folder(&FOLDERID_Videos)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Записи экрана")
}

/// Where the app lives when installed.
pub fn install_dir() -> PathBuf {
    known_folder(&FOLDERID_LocalAppData).unwrap_or_else(|| PathBuf::from(".")).join("Programs").join("Kadr")
}

pub fn start_menu_shortcut() -> PathBuf {
    known_folder(&FOLDERID_Programs).unwrap_or_else(|| PathBuf::from(".")).join("Kadr.lnk")
}

pub fn config_dir() -> PathBuf {
    known_folder(&FOLDERID_RoamingAppData).unwrap_or_else(|| PathBuf::from(".")).join("Kadr")
}

/// `YYYY-MM-DD HHMMSS` in local time, the same pattern Windows uses for captures.
pub fn timestamp() -> String {
    let t = unsafe { GetLocalTime() };
    format!(
        "{:04}-{:02}-{:02} {:02}{:02}{:02}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
    )
}

pub fn log(message: &str) {
    let dir = config_dir();
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("kadr.log")) {
        let _ = writeln!(f, "{} {message}", timestamp());
    }
}

pub fn log_panics() {
    std::panic::set_hook(Box::new(|info| log(&format!("panic: {info}"))));
}
