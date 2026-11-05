//! The settings window. Every change applies immediately, there is nothing to confirm.

use crate::color::ColorFormat;
use crate::gfx::*;
use crate::hook::{self, Hotkey};
use crate::record::{self, AudioDevice, MicTest};
use crate::settings::{self, FPS_CHOICES, Quality};
use crate::win::{self, WM_KADR_KEY_CAPTURED, WindowSpec, WndState};
use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::time::Instant;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::ValidateRect;
use windows::Win32::System::Com::*;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::*;

const CLASS: PCWSTR = w!("KadrSettings");
const LEVEL_TIMER: usize = 1;
const QUALITIES: [(Quality, &str); 3] =
    [(Quality::Standard, "Стандартное"), (Quality::High, "Высокое"), (Quality::Max, "Максимальное")];
/// A microphone that stays below this level for the whole check counts as silent.
const SILENCE: f32 = 0.0005;

const WIDTH_DIP: f32 = 640.0;
const MARGIN_DIP: f32 = 20.0;
const ROW_DIP: f32 = 48.0;
const SECTION_DIP: f32 = 38.0;
const CAPTION_DIP: f32 = 40.0;

thread_local! {
    static CURRENT: Cell<Option<HWND>> = const { Cell::new(None) };
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Id {
    Quality,
    Fps,
    Cursor,
    Countdown,
    VideosDir,
    System,
    SystemDevice,
    Mic,
    MicDevice,
    MicCheck,
    Autosave,
    ShotsDir,
    OpenEditor,
    Autostart,
    Hotkey,
    Presses,
    ColorFormat,
}

const COLOR_FORMATS: [(ColorFormat, &str); 3] =
    [(ColorFormat::Hex, "HEX"), (ColorFormat::Rgb, "RGB"), (ColorFormat::Hsl, "HSL")];

const PRESSES: [&str; 2] = ["Два раза", "Один раз"];

const LAYOUT: [(&str, &[(Id, &str)]); 4] = [
    (
        "Общее",
        &[
            (Id::Hotkey, "Горячая клавиша"),
            (Id::Presses, "Сколько раз нажимать"),
            (Id::Autostart, "Запускать вместе с Windows"),
        ],
    ),
    (
        "Видео",
        &[
            (Id::Quality, "Качество"),
            (Id::Fps, "Частота кадров"),
            (Id::Cursor, "Показывать курсор на видео"),
            (Id::Countdown, "Обратный отсчёт перед записью"),
            (Id::VideosDir, "Папка для видео"),
        ],
    ),
    (
        "Звук",
        &[
            (Id::System, "Записывать звук ПК"),
            (Id::SystemDevice, "Устройство вывода"),
            (Id::Mic, "Записывать микрофон"),
            (Id::MicDevice, "Микрофон"),
            (Id::MicCheck, "Проверка микрофона"),
        ],
    ),
    (
        "Снимки",
        &[
            (Id::Autosave, "Сохранять снимки автоматически"),
            (Id::ShotsDir, "Папка для снимков"),
            (Id::OpenEditor, "Сразу открывать редактор"),
            (Id::ColorFormat, "Формат цвета в пипетке"),
        ],
    ),
];

/// The file name's folder chain, shortened for one line of small text.
fn short_path(path: &Path) -> String {
    let parts: Vec<String> = path.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
    if parts.len() <= 3 {
        return path.display().to_string();
    }
    format!("…\\{}\\{}", parts[parts.len() - 2], parts[parts.len() - 1])
}

/// Approximate upper size of a minute of video at this bitrate, rounded to 5 MB.
fn megabytes_per_minute(bits_per_second: u32) -> u32 {
    let mb = bits_per_second as f64 / 8.0 * 60.0 / 1_000_000.0;
    ((mb / 5.0).round() as u32 * 5).max(5)
}

struct DeviceList {
    id: Id,
    /// `None` is "the default device in Windows".
    entries: Vec<(Option<String>, String)>,
}

struct MicCheck {
    test: MicTest,
    started: Instant,
    heard: bool,
}

struct Prefs {
    hwnd: HWND,
    surface: Surface,
    scale: f32,
    scroll: f32,
    content_h: f32,
    mouse: (i32, i32),
    open_list: Option<DeviceList>,
    check: Option<MicCheck>,
    /// Resolution used for the size estimate next to the quality setting.
    resolution: (i32, i32),
    /// Waiting for the user to press the new hotkey.
    capturing: bool,
    /// The last pressed key could not be used as a hotkey.
    key_refused: bool,
}

pub fn open() {
    if let Some(hwnd) = CURRENT.with(|c| c.get()) {
        win::force_foreground(hwnd);
        return;
    }
    let monitors = crate::capture::monitors();
    let (mx, my) = win::cursor_pos();
    let Some(m) = crate::capture::monitor_at(&monitors, mx, my) else {
        return;
    };
    refresh_accent();
    let s = m.scale.max(1.0);
    let w = (WIDTH_DIP * s) as i32;
    let content = LAYOUT.iter().map(|(_, rows)| SECTION_DIP + rows.len() as f32 * ROW_DIP).sum::<f32>() + MARGIN_DIP;
    let h = (((content + CAPTION_DIP + 8.0) * s) as i32).min(m.work.h * 95 / 100);
    let rect = crate::geom::Rect::new(m.work.x + (m.work.w - w) / 2, m.work.y + (m.work.h - h) / 2, w, h);
    let state = Prefs {
        hwnd: HWND::default(),
        surface: Surface::new(HWND::default()),
        scale: s,
        scroll: 0.0,
        content_h: 0.0,
        mouse: (-1, -1),
        open_list: None,
        check: None,
        resolution: (m.rect.w, m.rect.h),
        capturing: false,
        key_refused: false,
    };
    let spec = WindowSpec {
        class: CLASS,
        title: w!("Kadr — настройки"),
        style: WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX,
        ex_style: WINDOW_EX_STYLE(0),
        rect,
    };
    match win::create_window(spec, state) {
        Ok(hwnd) => {
            CURRENT.with(|c| c.set(Some(hwnd)));
            win::fluent_frame(hwnd);
            unsafe {
                let _ = ShowWindow(hwnd, SW_SHOW);
            }
            win::force_foreground(hwnd);
        }
        Err(e) => win::show_error(&format!("Не удалось открыть настройки: {e}")),
    }
}

/// Where a click landed: a control, plus the option inside it.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Hit {
    Control(Id, usize),
    ListEntry(usize),
    Outside,
}

impl Prefs {
    /// Rows in window coordinates, with the current scroll applied.
    fn rows(&self) -> Vec<(Id, &'static str, RectF)> {
        let s = self.scale;
        let (w, _) = win::client_size(self.hwnd);
        let m = MARGIN_DIP * s;
        let mut y = CAPTION_DIP * s - self.scroll;
        let mut out = Vec::new();
        for (_, rows) in LAYOUT {
            y += SECTION_DIP * s;
            for (id, label) in rows.iter() {
                out.push((*id, *label, RectF::xywh(m, y, w as f32 - m * 2.0, ROW_DIP * s)));
                y += ROW_DIP * s;
            }
        }
        out
    }

    fn update_content_height(&mut self) {
        let s = self.scale;
        self.content_h = (CAPTION_DIP + LAYOUT.iter().map(|(_, r)| SECTION_DIP + r.len() as f32 * ROW_DIP).sum::<f32>() + MARGIN_DIP) * s;
        self.clamp_scroll();
    }

    fn clamp_scroll(&mut self) {
        let (_, h) = win::client_size(self.hwnd);
        self.scroll = self.scroll.clamp(0.0, (self.content_h - h as f32).max(0.0));
    }

    fn hint(&self, id: Id) -> String {
        let s = settings::get();
        match id {
            Id::Quality => {
                let (w, h) = self.resolution;
                let bits = record::video_bitrate(w as u32, h as u32, s.valid_fps(), s.quality);
                format!("{w}×{h}, {} fps: до {} МБ в минуту", s.valid_fps(), megabytes_per_minute(bits))
            }
            Id::Hotkey if self.capturing => "Нажмите новое сочетание. Esc — отмена".into(),
            Id::Hotkey if self.key_refused => "Эта клавиша нужна для ввода: добавьте Ctrl, Alt или Win".into(),
            Id::Hotkey => "Открывает выбор области и останавливает запись".into(),
            Id::Presses if s.hotkey.double => "Случайное нажатие ничего не сделает".into(),
            Id::Presses => "Срабатывает сразу".into(),
            Id::Fps => "Больше кадров — плавнее движение и больше размер файла".into(),
            Id::Countdown => "3 секунды, чтобы приготовиться".into(),
            Id::VideosDir => short_path(&crate::paths::recordings_dir()),
            Id::ShotsDir => short_path(&crate::paths::screenshots_dir()),
            Id::OpenEditor => "Редактор вместо уведомления после снимка".into(),
            Id::MicCheck => match &self.check {
                None => "Покажет, идёт ли сигнал с выбранного микрофона".into(),
                Some(c) if c.test.failed() => "Не удалось открыть микрофон".into(),
                Some(c) if !c.heard && c.started.elapsed().as_secs() >= 2 => {
                    "Сигнала нет: микрофон выключен или выбран не тот".into()
                }
                Some(c) if c.heard => "Микрофон работает".into(),
                Some(_) => "Скажите что-нибудь: полоска должна двигаться".into(),
            },
            _ => String::new(),
        }
    }

    fn toggle_value(&self, id: Id) -> Option<bool> {
        let s = settings::get();
        match id {
            Id::Cursor => Some(s.show_cursor),
            Id::Countdown => Some(s.countdown),
            Id::System => Some(s.system_audio),
            Id::Mic => Some(s.mic),
            Id::Autosave => Some(s.autosave),
            Id::OpenEditor => Some(s.open_editor),
            Id::Autostart => Some(settings::autostart_enabled()),
            _ => None,
        }
    }

    fn set_toggle(&mut self, id: Id, on: bool) {
        match id {
            Id::Autostart => settings::set_autostart(on),
            _ => settings::update(|s| match id {
                Id::Cursor => s.show_cursor = on,
                Id::Countdown => s.countdown = on,
                Id::System => s.system_audio = on,
                Id::Mic => s.mic = on,
                Id::Autosave => s.autosave = on,
                Id::OpenEditor => s.open_editor = on,
                _ => {}
            }),
        }
    }

    fn segments(&self, id: Id, row: RectF) -> Vec<(String, RectF, bool)> {
        let s = settings::get();
        let (labels, active): (Vec<String>, usize) = match id {
            Id::Quality => (
                QUALITIES.iter().map(|q| q.1.to_string()).collect(),
                QUALITIES.iter().position(|q| q.0 == s.quality).unwrap_or(1),
            ),
            Id::Fps => (
                FPS_CHOICES.iter().map(|f| f.to_string()).collect(),
                FPS_CHOICES.iter().position(|f| *f == s.valid_fps()).unwrap_or(1),
            ),
            Id::Presses => (PRESSES.iter().map(|t| t.to_string()).collect(), if s.hotkey.double { 0 } else { 1 }),
            Id::ColorFormat => (
                COLOR_FORMATS.iter().map(|f| f.1.to_string()).collect(),
                COLOR_FORMATS.iter().position(|f| f.0 == s.color_format).unwrap_or(0),
            ),
            _ => return Vec::new(),
        };
        let k = self.scale;
        let font = 13.0 * k;
        let widths: Vec<f32> = labels.iter().map(|l| measure(l, Font::Ui, font).0 + 22.0 * k).collect();
        let total: f32 = widths.iter().sum();
        let h = 32.0 * k;
        let mut x = row.r - 16.0 * k - total;
        let top = row.cy() - h / 2.0;
        let mut out = Vec::new();
        for (i, label) in labels.into_iter().enumerate() {
            out.push((label, RectF::xywh(x, top, widths[i], h), i == active));
            x += widths[i];
        }
        out
    }

    fn toggle_rect(&self, row: RectF) -> RectF {
        let k = self.scale;
        RectF::xywh(row.r - 16.0 * k - 40.0 * k, row.cy() - 10.0 * k, 40.0 * k, 20.0 * k)
    }

    fn dropdown_rect(&self, row: RectF) -> RectF {
        let k = self.scale;
        RectF::xywh(row.r - 16.0 * k - 390.0 * k, row.cy() - 17.0 * k, 390.0 * k, 34.0 * k)
    }

    fn button_rect(&self, row: RectF, label: &str) -> RectF {
        let k = self.scale;
        let w = measure(label, Font::Ui, 13.0 * k).0 + 28.0 * k;
        RectF::xywh(row.r - 16.0 * k - w, row.cy() - 16.0 * k, w, 32.0 * k)
    }

    fn check_label(&self) -> &'static str {
        if self.check.is_some() { "Остановить" } else { "Проверить" }
    }

    fn hotkey_button_label(&self) -> &'static str {
        if self.capturing { "Отмена" } else { "Изменить" }
    }

    fn start_key_capture(&mut self) {
        self.capturing = true;
        self.key_refused = false;
        hook::begin_capture(self.hwnd);
    }

    fn stop_key_capture(&mut self) {
        if self.capturing {
            self.capturing = false;
            hook::cancel_capture();
            win::invalidate(self.hwnd);
        }
    }

    /// `wp` comes from the keyboard hook: 0 for Esc, otherwise the pressed keys.
    fn on_key_captured(&mut self, wp: usize) {
        self.capturing = false;
        let double = settings::get().hotkey.double;
        if let Some(key) = Hotkey::from_captured(wp, double) {
            if key.is_allowed() {
                settings::update(|s| s.hotkey = key);
                crate::app::apply_hotkey();
            } else {
                self.key_refused = true;
            }
        }
        win::invalidate(self.hwnd);
    }

    fn list_rects(&self, list: &DeviceList) -> Vec<RectF> {
        let k = self.scale;
        let Some((_, _, row)) = self.rows().into_iter().find(|r| r.0 == list.id) else {
            return Vec::new();
        };
        let anchor = self.dropdown_rect(row);
        let h = 34.0 * k;
        let total = h * list.entries.len() as f32 + 8.0 * k;
        let (_, ch) = win::client_size(self.hwnd);
        let below = anchor.b + 4.0 * k + total <= ch as f32;
        let top = if below { anchor.b + 4.0 * k + 4.0 * k } else { anchor.t - 4.0 * k - total + 4.0 * k };
        (0..list.entries.len())
            .map(|i| RectF::xywh(anchor.l + 4.0 * k, top + i as f32 * h, anchor.w() - 8.0 * k, h))
            .collect()
    }

    fn hit(&self, x: i32, y: i32) -> Hit {
        let (fx, fy) = (x as f32, y as f32);
        if fy < CAPTION_DIP * self.scale {
            return Hit::Outside;
        }
        if let Some(list) = &self.open_list {
            return self
                .list_rects(list)
                .iter()
                .position(|r| r.contains(fx, fy))
                .map_or(Hit::Outside, Hit::ListEntry);
        }
        for (id, _, row) in self.rows() {
            if !row.contains(fx, fy) {
                continue;
            }
            let on = |r: RectF| r.contains(fx, fy);
            match id {
                Id::Quality | Id::Fps | Id::Presses | Id::ColorFormat => {
                    if let Some(i) = self.segments(id, row).iter().position(|s| on(s.1)) {
                        return Hit::Control(id, i);
                    }
                }
                Id::Hotkey => {
                    if on(self.button_rect(row, self.hotkey_button_label())) {
                        return Hit::Control(id, 0);
                    }
                }
                Id::SystemDevice | Id::MicDevice => {
                    if on(self.dropdown_rect(row)) {
                        return Hit::Control(id, 0);
                    }
                }
                Id::VideosDir | Id::ShotsDir => {
                    if on(self.button_rect(row, "Изменить")) {
                        return Hit::Control(id, 0);
                    }
                }
                Id::MicCheck => {
                    if on(self.button_rect(row, self.check_label())) {
                        return Hit::Control(id, 0);
                    }
                }
                // The whole row of a switch is clickable, as in Windows settings.
                _ => return Hit::Control(id, 0),
            }
        }
        Hit::Outside
    }

    fn device_entries(&self, id: Id) -> Vec<(Option<String>, String)> {
        let loopback = id == Id::SystemDevice;
        let default_name = record::default_device_name(loopback).unwrap_or_else(|| "не найдено".into());
        let mut entries = vec![(None, format!("Как в Windows: {default_name}"))];
        entries.extend(record::audio_devices(loopback).into_iter().map(|AudioDevice { id, name }| (Some(id), name)));
        entries
    }

    fn selected_device_label(&self, id: Id) -> String {
        let s = settings::get();
        let selected = if id == Id::SystemDevice { s.system_device } else { s.mic_device };
        let entries = self.device_entries(id);
        entries
            .iter()
            .find(|e| e.0 == selected)
            .map(|e| e.1.clone())
            .unwrap_or_else(|| "Не подключено: будет использовано устройство Windows".into())
    }

    fn select_device(&mut self, id: Id, device: Option<String>) {
        let changed_mic = id == Id::MicDevice;
        settings::update(|s| {
            if changed_mic {
                s.mic_device = device;
            } else {
                s.system_device = device;
            }
        });
        // A running check must listen to the newly chosen microphone.
        if changed_mic && self.check.is_some() {
            self.start_check();
        }
    }

    fn start_check(&mut self) {
        let test = MicTest::start(settings::get().mic_device);
        self.check = Some(MicCheck { test, started: Instant::now(), heard: false });
        unsafe { SetTimer(Some(self.hwnd), LEVEL_TIMER, 50, None) };
    }

    fn stop_check(&mut self) {
        self.check = None;
        unsafe {
            let _ = KillTimer(Some(self.hwnd), LEVEL_TIMER);
        }
    }

    fn pick_folder(&mut self, id: Id) {
        let current = if id == Id::VideosDir { crate::paths::recordings_dir() } else { crate::paths::screenshots_dir() };
        if let Some(path) = folder_dialog(self.hwnd, &current) {
            settings::update(|s| {
                if id == Id::VideosDir {
                    s.recordings_dir = Some(path);
                } else {
                    s.screenshots_dir = Some(path);
                }
            });
        }
    }

    fn click(&mut self, x: i32, y: i32) {
        let hit = self.hit(x, y);
        // Any click ends the wait for a key; the button itself is handled below.
        let was_capturing = self.capturing;
        self.stop_key_capture();
        if let Some(list) = self.open_list.take() {
            if let Hit::ListEntry(i) = hit {
                let device = list.entries[i].0.clone();
                self.select_device(list.id, device);
            }
            win::invalidate(self.hwnd);
            return;
        }
        let Hit::Control(id, option) = hit else {
            return;
        };
        match id {
            Id::Quality => settings::update(|s| s.quality = QUALITIES[option].0),
            Id::Fps => settings::update(|s| s.fps = FPS_CHOICES[option]),
            Id::ColorFormat => settings::update(|s| s.color_format = COLOR_FORMATS[option].0),
            Id::Hotkey => {
                if !was_capturing {
                    self.start_key_capture();
                }
            }
            Id::Presses => {
                settings::update(|s| s.hotkey.double = option == 0);
                crate::app::apply_hotkey();
            }
            Id::SystemDevice | Id::MicDevice => {
                self.open_list = Some(DeviceList { id, entries: self.device_entries(id) });
            }
            Id::VideosDir | Id::ShotsDir => self.pick_folder(id),
            Id::MicCheck => {
                if self.check.is_some() {
                    self.stop_check();
                } else {
                    self.start_check();
                }
            }
            _ => {
                if let Some(value) = self.toggle_value(id) {
                    self.set_toggle(id, !value);
                }
            }
        }
        win::invalidate(self.hwnd);
    }

    fn paint(&mut self) {
        let Some(p) = self.surface.begin() else {
            return;
        };
        let k = self.scale;
        let (w, h) = win::client_size(self.hwnd);
        p.clear(TRANSPARENT);
        let m = MARGIN_DIP * k;
        let hover = self.hit(self.mouse.0, self.mouse.1);
        let selected_labels = (self.selected_device_label(Id::SystemDevice), self.selected_device_label(Id::MicDevice));
        let caption = CAPTION_DIP * k;

        let title = RectF::new(16.0 * k, 0.0, w as f32, caption);
        p.text("Настройки", title, Font::Ui, 12.0 * k, TEXT, Align::Left);

        // Everything below the title bar scrolls; the bar itself is see-through.
        p.push_clip(RectF::new(0.0, caption, w as f32, h as f32));
        let mut y = caption - self.scroll;
        for (title, rows) in LAYOUT {
            let title_r = RectF::xywh(m + 2.0 * k, y + 11.0 * k, w as f32, 24.0 * k);
            p.text(title, title_r, Font::UiBold, 14.0 * k, TEXT, Align::Left);
            y += SECTION_DIP * k;
            let card = RectF::xywh(m, y, w as f32 - m * 2.0, rows.len() as f32 * ROW_DIP * k);
            p.round(card, 6.0 * k, CARD);
            p.round_stroke(card, 6.0 * k, CARD_STROKE, 1.0);
            for (i, (id, label)) in rows.iter().enumerate() {
                let row = RectF::xywh(card.l, y + i as f32 * ROW_DIP * k, card.w(), ROW_DIP * k);
                if i > 0 {
                    p.line((row.l, row.t), (row.r, row.t), CARD_STROKE, 1.0);
                }
                self.paint_row(&p, *id, label, row, hover, &selected_labels);
            }
            y += rows.len() as f32 * ROW_DIP * k;
        }

        if let Some(list) = &self.open_list {
            self.paint_list(&p, list, hover);
        }
        p.pop_clip();
        if self.content_h > h as f32 {
            let track = h as f32 - caption - 8.0 * k;
            let thumb_h = (h as f32 / self.content_h * track).max(24.0 * k);
            let top = caption + 4.0 * k + self.scroll / (self.content_h - h as f32) * (track - thumb_h);
            let thumb = RectF::xywh(w as f32 - 6.0 * k, top, 3.0 * k, thumb_h);
            p.round(thumb, 1.5 * k, rgba(0xffffff, 0.35));
        }
        self.surface.end();
    }

    fn paint_row(&self, p: &Painter, id: Id, label: &str, row: RectF, hover: Hit, selected: &(String, String)) {
        let k = self.scale;
        let hint = self.hint(id);
        let text_l = row.l + 16.0 * k;
        // Text stops before the control so long hints never run under it.
        let text_r = match id {
            Id::SystemDevice | Id::MicDevice => self.dropdown_rect(row).l - 8.0 * k,
            Id::Quality | Id::Fps | Id::Presses | Id::ColorFormat => self.segments(id, row).first().map_or(row.r, |s| s.1.l - 8.0 * k),
            Id::Hotkey => self.button_rect(row, self.hotkey_button_label()).l - 150.0 * k,
            Id::MicCheck => self.button_rect(row, self.check_label()).l - 122.0 * k,
            _ => row.r - 90.0 * k,
        };
        p.push_clip(RectF::new(text_l, row.t, text_r, row.b));
        if hint.is_empty() {
            p.text(label, RectF::new(text_l, row.t, row.r, row.b), Font::Ui, 14.0 * k, TEXT, Align::Left);
        } else {
            let top = RectF::new(text_l, row.t + 4.0 * k, row.r, row.t + 26.0 * k);
            p.text(label, top, Font::Ui, 14.0 * k, TEXT, Align::Left);
            let under = RectF::new(text_l, row.t + 25.0 * k, row.r, row.b - 4.0 * k);
            p.text(&hint, under, Font::Ui, 12.0 * k, TEXT_DIM, Align::Left);
        }
        p.pop_clip();

        match id {
            Id::Hotkey => {
                let label = self.hotkey_button_label();
                self.paint_button(p, row, label, hover == Hit::Control(id, 0));
                let b = self.button_rect(row, label);
                let text = if self.capturing { "…".to_string() } else { settings::get().hotkey.label() };
                let size = 13.0 * k;
                let w = measure(&text, Font::UiBold, size).0 + 22.0 * k;
                let cap = RectF::xywh(b.l - 8.0 * k - w, row.cy() - 16.0 * k, w, 32.0 * k);
                p.round(cap, 4.0 * k, if self.capturing { accent() } else { CONTROL });
                let color = if self.capturing { ON_ACCENT } else { TEXT };
                p.text(&text, cap, Font::UiBold, size, color, Align::Center);
            }
            Id::Quality | Id::Fps | Id::Presses | Id::ColorFormat => {
                let segments = self.segments(id, row);
                if let (Some(first), Some(last)) = (segments.first(), segments.last()) {
                    let whole = RectF::new(first.1.l, first.1.t, last.1.r, first.1.b);
                    p.round(whole, 4.0 * k, CONTROL);
                    p.round_stroke(whole, 4.0 * k, CONTROL_STROKE, 1.0);
                }
                for (i, (text, r, active)) in segments.iter().enumerate() {
                    if *active {
                        p.round(*r, 4.0 * k, accent());
                    } else if hover == Hit::Control(id, i) {
                        p.round(*r, 4.0 * k, HOVER);
                    }
                    let c = if *active { ON_ACCENT } else { TEXT };
                    p.text(text, *r, Font::Ui, 13.0 * k, c, Align::Center);
                }
            }
            Id::SystemDevice | Id::MicDevice => {
                let r = self.dropdown_rect(row);
                p.round(r, 4.0 * k, if hover == Hit::Control(id, 0) { CONTROL_HOVER } else { CONTROL });
                p.round_stroke(r, 4.0 * k, CONTROL_STROKE, 1.0);
                let name = if id == Id::SystemDevice { &selected.0 } else { &selected.1 };
                let text = RectF::new(r.l + 12.0 * k, r.t, r.r - 30.0 * k, r.b);
                p.push_clip(text);
                p.text(name, RectF::new(text.l, text.t, text.l + 2000.0, text.b), Font::Ui, 13.0 * k, TEXT, Align::Left);
                p.pop_clip();
                p.icon('\u{E70D}', r.r - 16.0 * k, r.cy(), 10.0 * k, TEXT_DIM);
            }
            Id::VideosDir | Id::ShotsDir => self.paint_button(p, row, "Изменить", hover == Hit::Control(id, 0)),
            Id::MicCheck => {
                let label = self.check_label();
                self.paint_button(p, row, label, hover == Hit::Control(id, 0));
                if let Some(c) = &self.check {
                    let b = self.button_rect(row, label);
                    let bar = RectF::xywh(b.l - 112.0 * k, row.cy() - 4.0 * k, 100.0 * k, 8.0 * k);
                    p.round(bar, 4.0 * k, CONTROL);
                    // Speech peaks are small numbers; the square root makes them readable.
                    let level = c.test.level().sqrt().clamp(0.0, 1.0);
                    if level > 0.01 {
                        let fill = RectF::new(bar.l, bar.t, bar.l + bar.w() * level, bar.b);
                        p.round(fill, 4.0 * k, accent());
                    }
                }
            }
            _ => {
                if let Some(on) = self.toggle_value(id) {
                    let r = self.toggle_rect(row);
                    let radius = r.h() / 2.0;
                    if on {
                        p.round(r, radius, accent());
                    } else {
                        p.round_stroke(r.inflate(-0.5), radius, TEXT_DIM, 1.0);
                    }
                    let knob_x = if on { r.r - radius } else { r.l + radius };
                    let (knob, knob_r) = if on { (ON_ACCENT, 7.0 * k) } else { (TEXT_DIM, 5.0 * k) };
                    p.fill_circle(knob_x, r.cy(), knob_r, knob);
                    // The word next to the switch, as in the settings of Windows.
                    let word = RectF::new(r.l - 50.0 * k, row.t, r.l - 10.0 * k, row.b);
                    p.text(if on { "Вкл." } else { "Откл." }, word, Font::Ui, 13.0 * k, TEXT, Align::Left);
                }
            }
        }
    }

    fn paint_button(&self, p: &Painter, row: RectF, label: &str, hovered: bool) {
        let k = self.scale;
        let r = self.button_rect(row, label);
        p.round(r, 4.0 * k, if hovered { CONTROL_HOVER } else { CONTROL });
        p.round_stroke(r, 4.0 * k, CONTROL_STROKE, 1.0);
        p.text(label, r, Font::Ui, 13.0 * k, TEXT, Align::Center);
    }

    fn paint_list(&self, p: &Painter, list: &DeviceList, hover: Hit) {
        let k = self.scale;
        let rects = self.list_rects(list);
        let (Some(first), Some(last)) = (rects.first(), rects.last()) else {
            return;
        };
        let panel = RectF::new(first.l - 4.0 * k, first.t - 4.0 * k, first.r + 4.0 * k, last.b + 4.0 * k);
        p.soft_shadow(panel, k);
        p.round(panel, 8.0 * k, FLYOUT);
        p.round_stroke(panel, 8.0 * k, FLYOUT_STROKE, 1.0);
        let s = settings::get();
        let selected = if list.id == Id::SystemDevice { s.system_device } else { s.mic_device };
        for (i, r) in rects.iter().enumerate() {
            let (device, name) = &list.entries[i];
            let is_selected = *device == selected;
            if is_selected || hover == Hit::ListEntry(i) {
                p.round(*r, 4.0 * k, HOVER);
            }
            if is_selected {
                // The accent mark of a selected item in Windows 11 lists.
                let mark = RectF::xywh(r.l, r.cy() - 8.0 * k, 3.0 * k, 16.0 * k);
                p.round(mark, 1.5 * k, accent());
            }
            let text = RectF::new(r.l + 12.0 * k, r.t, r.r - 8.0 * k, r.b);
            p.push_clip(text);
            p.text(name, RectF::new(text.l, text.t, text.l + 2000.0, text.b), Font::Ui, 13.0 * k, TEXT, Align::Left);
            p.pop_clip();
        }
    }
}

fn folder_dialog(owner: HWND, current: &Path) -> Option<PathBuf> {
    unsafe {
        let dialog: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        dialog.SetOptions(FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM).ok()?;
        let _ = std::fs::create_dir_all(current);
        let start = win::wide(&current.to_string_lossy());
        let folder: Result<IShellItem> = SHCreateItemFromParsingName(PCWSTR(start.as_ptr()), None);
        if let Ok(folder) = folder {
            let _ = dialog.SetFolder(&folder);
        }
        dialog.Show(Some(owner)).ok()?;
        let item = dialog.GetResult().ok()?;
        let path = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let result = path.to_string().ok().map(PathBuf::from);
        CoTaskMemFree(Some(path.0 as _));
        result
    }
}

impl WndState for Prefs {
    fn on_message(&mut self, hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
        let caption = (CAPTION_DIP * self.scale) as i32;
        if let Some(r) = win::frame_message(hwnd, msg, wp, lp, caption, false, |_, _| true) {
            return Some(r);
        }
        match msg {
            WM_CREATE => {
                self.hwnd = hwnd;
                self.surface = Surface::glass(hwnd);
                self.scale = win::dpi_scale(hwnd);
                self.update_content_height();
                Some(LRESULT(0))
            }
            WM_ERASEBKGND => Some(LRESULT(1)),
            WM_PAINT => {
                self.paint();
                unsafe {
                    let _ = ValidateRect(Some(hwnd), None);
                }
                Some(LRESULT(0))
            }
            WM_SIZE => {
                self.surface.resize();
                self.update_content_height();
                win::invalidate(hwnd);
                Some(LRESULT(0))
            }
            WM_DPICHANGED => {
                self.scale = ((wp.0 >> 16) & 0xffff) as f32 / 96.0;
                let r = unsafe { &*(lp.0 as *const RECT) };
                unsafe {
                    let _ = SetWindowPos(
                        hwnd,
                        None,
                        r.left,
                        r.top,
                        r.right - r.left,
                        r.bottom - r.top,
                        SWP_NOZORDER | SWP_NOACTIVATE,
                    );
                }
                self.update_content_height();
                Some(LRESULT(0))
            }
            WM_MOUSEMOVE => {
                self.mouse = win::mouse_xy(lp);
                win::invalidate(hwnd);
                Some(LRESULT(0))
            }
            WM_LBUTTONUP => {
                let (x, y) = win::mouse_xy(lp);
                self.mouse = (x, y);
                self.click(x, y);
                Some(LRESULT(0))
            }
            WM_MOUSEWHEEL => {
                let delta = ((wp.0 >> 16) & 0xffff) as u16 as i16 as f32;
                self.scroll -= delta / 120.0 * 60.0 * self.scale;
                self.open_list = None;
                self.clamp_scroll();
                win::invalidate(hwnd);
                Some(LRESULT(0))
            }
            WM_TIMER if wp.0 == LEVEL_TIMER => {
                if let Some(c) = &mut self.check {
                    if c.test.level() > SILENCE {
                        c.heard = true;
                    }
                }
                win::invalidate(hwnd);
                Some(LRESULT(0))
            }
            WM_KADR_KEY_CAPTURED => {
                self.on_key_captured(wp.0);
                Some(LRESULT(0))
            }
            // Keys pressed in another window must not become the hotkey.
            WM_ACTIVATE if wp.0 & 0xffff == 0 => {
                self.stop_key_capture();
                None
            }
            WM_KEYDOWN if wp.0 == 0x1B => {
                // Esc closes an open list first, then the window.
                if self.open_list.take().is_some() {
                    win::invalidate(hwnd);
                } else {
                    let _ = unsafe { PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0)) };
                }
                Some(LRESULT(0))
            }
            WM_DESTROY => {
                hook::cancel_capture();
                CURRENT.with(|c| c.set(None));
                None
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_paths_keep_the_last_two_folders() {
        assert_eq!(short_path(Path::new(r"C:\Users\name\Videos\Записи экрана")), r"…\Videos\Записи экрана");
        assert_eq!(short_path(Path::new(r"D:\Video")), r"D:\Video");
    }

    #[test]
    fn size_estimate_is_rounded_to_five_megabytes() {
        assert_eq!(megabytes_per_minute(22_118_400), 165);
        assert_eq!(megabytes_per_minute(100_000), 5);
    }
}
