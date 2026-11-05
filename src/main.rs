#![cfg_attr(not(test), windows_subsystem = "windows")]

mod app;
mod capture;
mod clipboard;
mod color;
mod delay;
mod editor;
mod geom;
mod gfx;
mod hook;
mod image;
mod notify;
mod ocr;
mod overlay;
mod paths;
mod player;
mod prefs;
mod record;
mod recui;
mod settings;
mod setup;
mod smart;
mod videoedit;
mod win;

use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};

fn main() {
    paths::log_panics();
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
    let args: Vec<String> = std::env::args().collect();
    if let Some(link) = args.get(1).filter(|a| notify::is_link(a)) {
        notify::handle_link(link);
        return;
    }
    if args.iter().any(|a| a == "--uninstall") {
        setup::uninstall();
        return;
    }
    if setup::runs_as_installer() {
        setup::install();
        return;
    }
    match args.iter().position(|a| a == "--edit") {
        Some(i) => match args.get(i + 1) {
            Some(path) => app::run_editor(std::path::Path::new(path)),
            None => win::show_error("Использование: kadr --edit файл.png"),
        },
        None => app::run(),
    }
}
