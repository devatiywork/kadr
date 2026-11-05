//! Real Windows notifications with a picture. Clicking one reaches Kadr through the
//! `kadr:` link protocol, which also works from the notification center long after
//! the notification has left the screen.

use crate::image::Image;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, SystemTime};
use windows::Data::Xml::Dom::XmlDocument;
use windows::UI::Notifications::{ToastNotification, ToastNotificationManager};
use windows::Win32::Foundation::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::Registry::*;
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::*;

pub const APP_ID: &str = "Kadr.App";
const SCHEME: &str = "kadr:";
const ICON: &[u8] = include_bytes!("../assets/kadr.png");
const PREVIEW_MAX: (u32, u32) = (720, 400);
const TEMP_FILE_LIFETIME: Duration = Duration::from_secs(7 * 24 * 3600);

pub fn set_registry_value(key: &str, name: Option<&str>, value: &str) {
    let key = crate::win::wide(key);
    let name = name.map(crate::win::wide);
    let value = crate::win::wide(value);
    let name_ptr = name.as_ref().map_or(PCWSTR::null(), |n| PCWSTR(n.as_ptr()));
    unsafe {
        let result = RegSetKeyValueW(
            HKEY_CURRENT_USER,
            PCWSTR(key.as_ptr()),
            name_ptr,
            REG_SZ.0,
            Some(value.as_ptr() as _),
            (value.len() * 2) as u32,
        );
        if result != ERROR_SUCCESS {
            crate::paths::log(&format!("registry write failed: {result:?}"));
        }
    }
}

pub fn temp_dir() -> PathBuf {
    std::env::temp_dir().join("Kadr")
}

/// Tells Windows the app's name and icon for notifications, and who opens `kadr:` links.
pub fn register() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let icon = crate::paths::config_dir().join("icon.png");
    let _ = std::fs::create_dir_all(crate::paths::config_dir());
    let _ = std::fs::write(&icon, ICON);

    let app_key = format!("Software\\Classes\\AppUserModelId\\{APP_ID}");
    set_registry_value(&app_key, Some("DisplayName"), "Kadr");
    set_registry_value(&app_key, Some("IconUri"), &icon.to_string_lossy());

    set_registry_value("Software\\Classes\\kadr", None, "URL:Kadr");
    set_registry_value("Software\\Classes\\kadr", Some("URL Protocol"), "");
    let command = format!("\"{}\" \"%1\"", exe.display());
    set_registry_value("Software\\Classes\\kadr\\shell\\open\\command", None, &command);

    remove_old_temp_files();
}

fn remove_old_temp_files() {
    let Ok(entries) = std::fs::read_dir(temp_dir()) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let age = entry.metadata().ok().and_then(|m| m.modified().ok()).and_then(|t| now.duration_since(t).ok());
        if age.is_some_and(|a| a > TEMP_FILE_LIFETIME) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

fn encode(text: &str, keep: &str) -> String {
    let mut out = String::new();
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) || keep.as_bytes().contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = text.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn escape_xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[derive(Debug, PartialEq, Eq)]
pub enum Activation {
    /// File name of a screenshot to open in the editor.
    Edit(String),
    /// File name of a recording to play.
    Video(String),
    /// File name of a GIF to show.
    Gif(String),
}

/// A bare file name with the expected extension. Any website can open a `kadr:` link,
/// so links must never be able to point at arbitrary paths or programs.
fn safe_file_name(name: &str, extension: &str) -> bool {
    !name.is_empty()
        && !name.contains(['/', '\\', ':', '*', '?', '"', '<', '>', '|'])
        && !name.contains("..")
        && name.to_lowercase().ends_with(extension)
}

pub fn parse_link(link: &str) -> Option<Activation> {
    let rest = link.strip_prefix(SCHEME)?.trim_end_matches('/');
    let (action, query) = rest.split_once('?')?;
    let name = decode(query.strip_prefix("name=")?)?;
    match action {
        "edit" if safe_file_name(&name, ".png") => Some(Activation::Edit(name)),
        "video" if safe_file_name(&name, ".mp4") => Some(Activation::Video(name)),
        "gif" if safe_file_name(&name, ".gif") => Some(Activation::Gif(name)),
        _ => None,
    }
}

pub fn is_link(arg: &str) -> bool {
    arg.starts_with(SCHEME)
}

fn find_recording(name: &str) -> Option<PathBuf> {
    [crate::paths::recordings_dir(), crate::paths::default_recordings_dir()]
        .into_iter()
        .map(|dir| dir.join(name))
        .find(|path| path.exists())
}

/// Runs when a notification was clicked: Windows starts Kadr with the link.
pub fn handle_link(link: &str) {
    match parse_link(link) {
        Some(Activation::Edit(name)) => {
            // A notification may outlive a change of the folder in the settings.
            let found = [crate::paths::screenshots_dir(), crate::paths::default_screenshots_dir(), temp_dir()]
                .into_iter()
                .map(|dir| dir.join(&name))
                .find(|path| path.exists());
            match found {
                Some(path) => crate::app::run_editor(&path),
                None => crate::win::show_error(&format!("Снимок «{name}» больше не существует.")),
            }
        }
        Some(Activation::Video(name)) => match find_recording(&name) {
            Some(path) => crate::app::run_player(&path),
            None => crate::win::show_error(&format!("Видео «{name}» больше не существует.")),
        },
        Some(Activation::Gif(name)) => match find_recording(&name) {
            Some(path) => {
                let file = crate::win::wide(&path.to_string_lossy());
                unsafe {
                    ShellExecuteW(None, w!("open"), PCWSTR(file.as_ptr()), None, None, SW_SHOWNORMAL);
                }
            }
            None => crate::win::show_error(&format!("Файл «{name}» больше не существует.")),
        },
        None => crate::paths::log(&format!("ignored link: {link}")),
    }
}

fn notification_xml(title: &str, text: &str, link: Option<&str>, picture: Option<&Path>) -> String {
    let image = picture.map_or(String::new(), |p| {
        let uri = format!("file:///{}", encode(&p.to_string_lossy().replace('\\', "/"), "/:"));
        format!("<image src=\"{}\"/>", escape_xml(&uri))
    });
    let action = link.map_or(String::new(), |l| format!(" activationType=\"protocol\" launch=\"{}\"", escape_xml(l)));
    format!(
        "<toast{action}><visual><binding template=\"ToastGeneric\">\
         <text>{}</text><text>{}</text>{image}</binding></visual><audio silent=\"true\"/></toast>",
        escape_xml(title),
        escape_xml(text),
    )
}

fn show(xml: &str) -> Result<()> {
    let doc = XmlDocument::new()?;
    doc.LoadXml(&HSTRING::from(xml))?;
    let toast = ToastNotification::CreateToastNotification(&doc)?;
    ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(APP_ID))?.Show(&toast)
}

/// Small picture for the notification: Windows ignores big image files.
fn save_preview(img: &Image) -> Option<PathBuf> {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let name = format!("preview-{}-{n}.png", crate::paths::timestamp().replace(' ', "-"));
    let path = temp_dir().join(name);
    match img.thumbnail(PREVIEW_MAX.0, PREVIEW_MAX.1).save_png(&path) {
        Ok(()) => Some(path),
        Err(e) => {
            crate::paths::log(&format!("notification preview: {e}"));
            None
        }
    }
}

/// File work and the notification run off the UI thread to keep capturing instant.
pub fn in_background(work: impl FnOnce() -> Result<()> + Send + 'static) {
    std::thread::spawn(move || unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        if let Err(e) = work() {
            crate::paths::log(&format!("notification failed: {e}"));
        }
        CoUninitialize();
    });
}

/// Saves the screenshot and announces it. With autosave off the file goes to the temp
/// folder, because clicking the notification still has to open it.
pub fn screenshot_taken(img: Image, autosave: bool, announce: bool) {
    in_background(move || {
        let dir = if autosave { crate::paths::screenshots_dir() } else { temp_dir() };
        let name = format!("Снимок экрана {}.png", crate::paths::timestamp());
        if let Err(e) = img.save_png(&dir.join(&name)) {
            crate::paths::log(&format!("saving a screenshot failed: {e}"));
        }
        if !announce {
            return Ok(());
        }
        let preview = save_preview(&img);
        let link = format!("{SCHEME}edit?name={}", encode(&name, ""));
        let xml = notification_xml(
            "Снимок скопирован в буфер обмена",
            "Нажмите, чтобы открыть в редакторе",
            Some(&link),
            preview.as_deref(),
        );
        show(&xml)
    });
}

pub fn video_saved(thumb: Image, name: String) {
    in_background(move || {
        let preview = save_preview(&thumb);
        let link = format!("{SCHEME}video?name={}", encode(&name, ""));
        show(&notification_xml("Видео сохранено", &name, Some(&link), preview.as_deref()))
    });
}

/// A notification that only informs: clicking it does nothing but close it.
pub fn info(title: &str, text: &str, picture: Option<Image>) -> Result<()> {
    let preview = picture.as_ref().and_then(save_preview);
    show(&notification_xml(title, text, None, preview.as_deref()))
}

pub fn piece_saved(name: String) {
    in_background(move || {
        let link = format!("{SCHEME}video?name={}", encode(&name, ""));
        show(&notification_xml("Отрывок сохранён", &name, Some(&link), None))
    });
}

pub fn gif_saved(name: String) {
    in_background(move || {
        let link = format!("{SCHEME}gif?name={}", encode(&name, ""));
        show(&notification_xml("GIF сохранён", &name, Some(&link), None))
    });
}

/// Puts a picked color on the clipboard and shows it.
pub fn color_copied(value: String, rgb: [u8; 3]) {
    in_background(move || {
        crate::clipboard::set_text(&value)?;
        let swatch = Image::new(96, 32, [rgb[2], rgb[1], rgb[0], 255].repeat(96 * 32));
        info("Цвет скопирован", &value, Some(swatch))
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_survive_cyrillic_and_spaces() {
        let name = "Снимок экрана 2026-11-05 200112.png";
        let link = format!("{SCHEME}edit?name={}", encode(name, ""));
        assert!(link.is_ascii());
        assert_eq!(parse_link(&link), Some(Activation::Edit(name.to_string())));
    }

    #[test]
    fn video_link_is_recognized() {
        let link = format!("{SCHEME}video?name={}", encode("Запись экрана 1.mp4", ""));
        assert_eq!(parse_link(&link), Some(Activation::Video("Запись экрана 1.mp4".to_string())));
    }

    #[test]
    fn gif_link_only_opens_gif_files() {
        assert_eq!(parse_link("kadr:gif?name=a.gif"), Some(Activation::Gif("a.gif".to_string())));
        assert_eq!(parse_link("kadr:gif?name=a.exe"), None);
    }

    #[test]
    fn trailing_slash_added_by_browsers_is_ignored() {
        assert_eq!(parse_link("kadr:edit?name=a.png/"), Some(Activation::Edit("a.png".to_string())));
    }

    #[test]
    fn links_cannot_reach_outside_of_the_kadr_folders() {
        for bad in [
            "kadr:video?name=..%5C..%5CWindows%5Csystem32%5Ccalc.exe",
            "kadr:video?name=C%3A%5CWindows%5Cnotepad.mp4",
            "kadr:edit?name=%5C%5Cevil-server%5Cshare%5Ca.png",
            "kadr:edit?name=..%2F..%2Fa.png",
            "kadr:edit?name=a/b.png",
            "kadr:edit?name=",
        ] {
            assert_eq!(parse_link(bad), None, "{bad}");
        }
    }

    #[test]
    fn links_only_open_the_expected_file_types() {
        assert_eq!(parse_link("kadr:video?name=virus.exe"), None);
        assert_eq!(parse_link("kadr:video?name=a.png"), None);
        assert_eq!(parse_link("kadr:edit?name=a.mp4"), None);
        assert_eq!(parse_link("kadr:delete?name=a.png"), None);
        assert_eq!(parse_link("other:edit?name=a.png"), None);
    }

    #[test]
    fn broken_escapes_are_rejected() {
        assert_eq!(parse_link("kadr:edit?name=a%2.png"), None);
        assert_eq!(parse_link("kadr:edit?name=%FF%FE.png"), None);
    }

    #[test]
    fn notification_text_is_escaped() {
        let xml = notification_xml("a & <b>", "\"c\"", Some("kadr:edit?name=a.png"), None);
        assert!(xml.contains("<text>a &amp; &lt;b&gt;</text>"));
        assert!(xml.contains("<text>&quot;c&quot;</text>"));
        assert!(XmlDocument::new().unwrap().LoadXml(&HSTRING::from(xml)).is_ok());
    }

    #[test]
    fn informing_notification_starts_nothing() {
        let xml = notification_xml("t", "x", None, None);
        assert!(!xml.contains("launch"), "{xml}");
        assert!(XmlDocument::new().unwrap().LoadXml(&HSTRING::from(xml)).is_ok());
    }

    #[test]
    fn notification_picture_is_a_valid_file_uri() {
        let xml = notification_xml("t", "x", Some("kadr:edit?name=a.png"), Some(Path::new(r"C:\Temp\Папка\a b.png")));
        assert!(xml.contains("src=\"file:///C:/Temp/%D0%9F"), "{xml}");
        assert!(xml.contains("a%20b.png"), "{xml}");
    }
}
