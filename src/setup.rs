//! Installing for the current user and removing again. `Kadr-Setup-*.exe` is this same
//! program under another name, so there is one file to build and no administrator rights.

use crate::notify::set_registry_value;
use crate::paths;
use crate::win;
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::Diagnostics::ToolHelp::*;
use windows::Win32::System::Registry::*;
use windows::Win32::System::Threading::*;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::*;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const UNINSTALL_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\Kadr";
const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// How long a running copy gets to close by itself.
const CLOSE_WAIT: Duration = Duration::from_millis(1500);
/// A copy usually ends within a fraction of a second, but its file has been seen to stay
/// busy for more than 4 seconds.
const COPY_WAIT: Duration = Duration::from_secs(20);

/// `Kadr-Setup-0.1.0.exe`, also when a browser renames it to `Kadr-Setup-0.1.0 (1).exe`.
fn is_installer_name(file_name: &str) -> bool {
    file_name.to_lowercase().starts_with("kadr-setup")
}

pub fn runs_as_installer() -> bool {
    let exe = std::env::current_exe().ok();
    let name = exe.as_ref().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned());
    name.is_some_and(|n| is_installer_name(&n))
}

fn other_running_copies() -> Vec<u32> {
    let mut found = Vec::new();
    unsafe {
        let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return found;
        };
        let mut entry = PROCESSENTRY32W { dwSize: size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        let mut more = Process32FirstW(snapshot, &mut entry).is_ok();
        while more {
            let len = entry.szExeFile.iter().position(|c| *c == 0).unwrap_or(entry.szExeFile.len());
            let name = String::from_utf16_lossy(&entry.szExeFile[..len]);
            if name.eq_ignore_ascii_case("kadr.exe") && entry.th32ProcessID != GetCurrentProcessId() {
                found.push(entry.th32ProcessID);
            }
            more = Process32NextW(snapshot, &mut entry).is_ok();
        }
        let _ = CloseHandle(snapshot);
    }
    found
}

/// The tray copy closes by itself to take its icon along; anything left keeps the file locked.
fn stop_running_copies() {
    unsafe {
        if let Ok(hwnd) = FindWindowW(crate::app::HIDDEN_CLASS, None) {
            let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
    }
    let started = Instant::now();
    while !other_running_copies().is_empty() && started.elapsed() < CLOSE_WAIT {
        std::thread::sleep(Duration::from_millis(50));
    }
    for id in other_running_copies() {
        unsafe {
            if let Ok(process) = OpenProcess(PROCESS_TERMINATE | PROCESS_SYNCHRONIZE, false, id) {
                let _ = TerminateProcess(process, 0);
                WaitForSingleObject(process, 2000);
                let _ = CloseHandle(process);
            }
        }
    }
}

fn copy_program(to: &Path) -> std::io::Result<()> {
    let from = std::env::current_exe()?;
    if let Some(dir) = to.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let started = Instant::now();
    loop {
        match std::fs::copy(&from, to) {
            Ok(_) => return Ok(()),
            Err(_) if started.elapsed() < COPY_WAIT => std::thread::sleep(Duration::from_millis(200)),
            Err(e) => return Err(e),
        }
    }
}

fn create_shortcut(target: &Path, link: &Path) -> Result<()> {
    let target = win::wide(&target.to_string_lossy());
    let link = win::wide(&link.to_string_lossy());
    unsafe {
        let shortcut: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
        shortcut.SetPath(PCWSTR(target.as_ptr()))?;
        shortcut.SetDescription(w!("Снимки и запись экрана"))?;
        let file: IPersistFile = shortcut.cast()?;
        file.Save(PCWSTR(link.as_ptr()), true)
    }
}

fn set_registry_number(key: &str, name: &str, value: u32) {
    let (key, name) = (win::wide(key), win::wide(name));
    unsafe {
        let _ = RegSetKeyValueW(
            HKEY_CURRENT_USER,
            PCWSTR(key.as_ptr()),
            PCWSTR(name.as_ptr()),
            REG_DWORD.0,
            Some(&value as *const u32 as _),
            size_of::<u32>() as u32,
        );
    }
}

fn delete_registry_tree(key: &str) {
    let key = win::wide(key);
    unsafe {
        let _ = RegDeleteTreeW(HKEY_CURRENT_USER, PCWSTR(key.as_ptr()));
    }
}

/// What Settings > Apps shows and runs.
fn register_for_removal(exe: &Path) {
    let path = exe.display().to_string();
    let values = [
        ("DisplayName", "Kadr".to_string()),
        ("DisplayVersion", VERSION.to_string()),
        ("Publisher", "devatiywork".to_string()),
        ("DisplayIcon", path.clone()),
        ("InstallLocation", exe.parent().map(|d| d.display().to_string()).unwrap_or_default()),
        ("UninstallString", format!("\"{path}\" --uninstall")),
    ];
    for (name, value) in values {
        set_registry_value(UNINSTALL_KEY, Some(name), &value);
    }
    set_registry_number(UNINSTALL_KEY, "NoModify", 1);
    set_registry_number(UNINSTALL_KEY, "NoRepair", 1);
    let size_kb = std::fs::metadata(exe).map(|m| m.len() / 1024).unwrap_or(0);
    set_registry_number(UNINSTALL_KEY, "EstimatedSize", size_kb as u32);
}

fn put_in_place(exe: &Path) -> std::result::Result<(), String> {
    stop_running_copies();
    // An update keeps the user's choice about starting with Windows.
    let first_install = !exe.exists();
    copy_program(exe).map_err(|e| e.to_string())?;
    if first_install {
        set_registry_value(RUN_KEY, Some("Kadr"), &format!("\"{}\"", exe.display()));
    }
    register_for_removal(exe);
    // The app works without its Start menu entry, so a failure here does not stop the install.
    if let Err(e) = create_shortcut(exe, &paths::start_menu_shortcut()) {
        paths::log(&format!("start menu shortcut: {e}"));
    }
    Command::new(exe).spawn().map_err(|e| e.to_string())?;
    Ok(())
}

pub fn install() {
    let dir = paths::install_dir();
    let exe = dir.join("kadr.exe");
    let mut question = format!(
        "Установить Kadr {VERSION}?\n\nПрограмма ставится только для вас, права администратора не нужны.\n\nПапка: {}",
        dir.display()
    );
    if !other_running_copies().is_empty() {
        question.push_str("\n\nЗапущенный Kadr будет закрыт, несохранённые правки в его окнах пропадут.");
    }
    if win::message_box(None, &question, MB_YESNO | MB_ICONQUESTION) != IDYES {
        return;
    }
    match put_in_place(&exe) {
        Ok(()) => {
            let key = crate::settings::get().hotkey.action_label();
            let done = format!(
                "Kadr {VERSION} установлен и запущен.\n\nСнимок или запись видео: {key}. Значок программы находится рядом с часами."
            );
            win::message_box(None, &done, MB_OK | MB_ICONINFORMATION);
        }
        Err(e) => win::show_error(&format!("Не удалось установить Kadr: {e}")),
    }
}

fn removal_arguments(dir: &Path) -> String {
    format!("/c ping -n 3 127.0.0.1 >nul & rmdir /s /q \"{}\"", dir.display())
}

/// The folder holds the running program, so it can only go after this process has ended.
fn remove_folder_after_exit(dir: &Path) {
    let _ = Command::new("cmd")
        .raw_arg(removal_arguments(dir))
        .current_dir(std::env::temp_dir())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn();
}

pub fn uninstall() {
    let question = "Удалить Kadr?\n\nСнимки, видео и настройки останутся на компьютере.";
    if win::message_box(None, question, MB_YESNO | MB_ICONQUESTION) != IDYES {
        return;
    }
    stop_running_copies();
    crate::settings::set_autostart(false);
    delete_registry_tree(UNINSTALL_KEY);
    delete_registry_tree("Software\\Classes\\kadr");
    delete_registry_tree(&format!("Software\\Classes\\AppUserModelId\\{}", crate::notify::APP_ID));
    let _ = std::fs::remove_file(paths::start_menu_shortcut());
    let _ = std::fs::remove_dir_all(crate::notify::temp_dir());
    win::message_box(None, "Kadr удалён.", MB_OK | MB_ICONINFORMATION);
    remove_folder_after_exit(&paths::install_dir());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installer_is_recognized_by_its_file_name() {
        assert!(is_installer_name("Kadr-Setup-0.1.0.exe"));
        assert!(is_installer_name("kadr-setup-0.1.0 (1).exe"));
        assert!(!is_installer_name("kadr.exe"));
        assert!(!is_installer_name("setup.exe"));
    }

    #[test]
    fn folder_with_spaces_stays_one_argument_of_the_removal() {
        let dir = Path::new(r"C:\Users\Some One\AppData\Local\Programs\Kadr");
        let arguments = removal_arguments(dir);
        assert!(arguments.ends_with(r#"rmdir /s /q "C:\Users\Some One\AppData\Local\Programs\Kadr""#), "{arguments}");
    }
}
