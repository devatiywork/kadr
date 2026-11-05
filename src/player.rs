//! Watching a recording, choosing a piece on the timeline and saving it as a video
//! or a GIF. Playback, including sound, is done by the Windows Media Engine.

use crate::geom::Rect;
use crate::gfx::*;
use crate::videoedit::{self, GIF_HIGH, GIF_LOW, GIF_MAX_SECONDS, GifQuality, JobState};
use crate::win::{self, WindowSpec, WndState};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::*;
use windows_core::implement;

const CLASS: PCWSTR = w!("KadrPlayer");
const VIDEO_CLASS: PCWSTR = w!("KadrVideoSurface");
const TICK: usize = 1;
const TICK_MS: u32 = 33;
const WM_JOB_DONE: u32 = WM_APP + 20;
const BAR_DIP: f32 = 48.0;
const CONTROLS_DIP: f32 = 96.0;
const MIN_PIECE_SECONDS: f64 = 0.1;

/// Time with tenths, because a cut is chosen more precisely than whole seconds.
fn clock(t: f64) -> String {
    let tenths = (t.max(0.0) * 10.0).round() as u64;
    format!("{:02}:{:02}.{}", tenths / 600, tenths / 10 % 60, tenths % 10)
}

/// The largest rectangle with the proportions of the video that fits the area, centered.
fn fit_video(area: RectF, vw: u32, vh: u32) -> RectF {
    if vw == 0 || vh == 0 {
        return area;
    }
    let k = (area.w() / vw as f32).min(area.h() / vh as f32);
    let (w, h) = (vw as f32 * k, vh as f32 * k);
    RectF::xywh(area.cx() - w / 2.0, area.cy() - h / 2.0, w, h)
}

#[implement(IMFMediaEngineNotify)]
struct EngineEvents;

impl IMFMediaEngineNotify_Impl for EngineEvents_Impl {
    // The window reads the state of the engine on its timer, so no event is needed.
    fn EventNotify(&self, _event: u32, _param1: usize, _param2: u32) -> Result<()> {
        Ok(())
    }
}

/// The engine draws into this child window; clicks go through to the player.
unsafe extern "system" fn video_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if msg == WM_NCHITTEST {
            return LRESULT(HTTRANSPARENT as isize);
        }
        DefWindowProcW(hwnd, msg, wp, lp)
    }
}

fn create_video_surface(parent: HWND) -> Result<HWND> {
    unsafe {
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(video_proc),
            hInstance: win::hinstance(),
            hbrBackground: HBRUSH(GetStockObject(BLACK_BRUSH).0),
            lpszClassName: VIDEO_CLASS,
            ..Default::default()
        };
        RegisterClassExW(&wc);
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            VIDEO_CLASS,
            w!(""),
            WS_CHILD | WS_VISIBLE,
            0,
            0,
            16,
            16,
            Some(parent),
            None,
            Some(win::hinstance()),
            None,
        )
    }
}

fn create_engine(video: HWND, path: &Path) -> Result<IMFMediaEngine> {
    unsafe {
        MFStartup(MF_VERSION, MFSTARTUP_FULL)?;
        let factory: IMFMediaEngineClassFactory =
            CoCreateInstance(&CLSID_MFMediaEngineClassFactory, None, CLSCTX_ALL)?;
        let mut attrs = None;
        MFCreateAttributes(&mut attrs, 2)?;
        let attrs = attrs.ok_or_else(|| Error::from(E_FAIL))?;
        let events: IMFMediaEngineNotify = EngineEvents.into();
        attrs.SetUnknown(&MF_MEDIA_ENGINE_CALLBACK, &events)?;
        attrs.SetUINT64(&MF_MEDIA_ENGINE_PLAYBACK_HWND, video.0 as u64)?;
        let engine = factory.CreateInstance(0, &attrs)?;
        engine.SetSource(&BSTR::from(path.to_string_lossy().as_ref()))?;
        Ok(engine)
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Hit {
    SavePiece,
    GifMenu,
    OpenExternal,
    Play,
    Mute,
}

struct Btn {
    hit: Hit,
    r: RectF,
    label: Option<&'static str>,
    glyph: char,
    tip: &'static str,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Grip {
    Start,
    End,
    Position,
}

struct Job {
    title: &'static str,
    state: Arc<JobState>,
    result: Arc<Mutex<Option<std::result::Result<PathBuf, String>>>>,
    is_gif: bool,
}

struct Player {
    hwnd: HWND,
    video: HWND,
    surface: Surface,
    scale: f32,
    path: PathBuf,
    name: String,
    engine: Option<IMFMediaEngine>,
    duration: f64,
    position: f64,
    paused: bool,
    muted: bool,
    /// The chosen piece, in seconds.
    start: f64,
    end: f64,
    grip: Option<Grip>,
    mouse: (i32, i32),
    btns: Vec<Btn>,
    job: Option<Job>,
    closing: bool,
    video_size: (u32, u32),
}

/// Opens a player window for a video file.
pub fn open(path: PathBuf) {
    let monitors = crate::capture::monitors();
    let (mx, my) = win::cursor_pos();
    let m = crate::capture::monitor_at(&monitors, mx, my);
    let (work, scale) = m.map(|m| (m.work, m.scale.max(1.0))).unwrap_or((Rect::new(0, 0, 1280, 800), 1.0));
    let w = work.w * 7 / 10;
    let h = work.h * 8 / 10;
    let rect = Rect::new(work.x + (work.w - w) / 2, work.y + (work.h - h) / 2, w, h);
    refresh_accent();
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let state = Player {
        hwnd: HWND::default(),
        video: HWND::default(),
        surface: Surface::new(HWND::default()),
        scale,
        path,
        name,
        engine: None,
        duration: 0.0,
        position: 0.0,
        paused: true,
        muted: false,
        start: 0.0,
        end: 0.0,
        grip: None,
        mouse: (-1, -1),
        btns: Vec::new(),
        job: None,
        closing: false,
        video_size: (0, 0),
    };
    let spec = WindowSpec {
        class: CLASS,
        title: w!("Kadr — видео"),
        style: WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
        ex_style: WINDOW_EX_STYLE(0),
        rect,
    };
    match win::create_window(spec, state) {
        Ok(hwnd) => {
            crate::app::window_opened();
            win::fluent_frame(hwnd);
            unsafe {
                let _ = ShowWindow(hwnd, SW_SHOW);
            }
            win::force_foreground(hwnd);
        }
        Err(e) => win::show_error(&format!("Не удалось открыть плеер: {e}")),
    }
}

impl Player {
    fn bar_height(&self) -> f32 {
        BAR_DIP * self.scale
    }

    fn timeline(&self) -> RectF {
        let k = self.scale;
        let (w, h) = win::client_size(self.hwnd);
        let top = h as f32 - CONTROLS_DIP * k + 14.0 * k;
        RectF::new(24.0 * k, top, w as f32 - 24.0 * k, top + 26.0 * k)
    }

    fn x_of(&self, t: f64) -> f32 {
        let line = self.timeline();
        line.l + line.w() * (t / self.duration.max(0.001)) as f32
    }

    fn time_at(&self, x: i32) -> f64 {
        let line = self.timeline();
        (((x as f32 - line.l) / line.w()) as f64).clamp(0.0, 1.0) * self.duration
    }

    fn is_trimmed(&self) -> bool {
        self.duration > 0.0 && (self.start > 0.05 || self.end < self.duration - 0.05)
    }

    fn layout(&mut self) {
        let k = self.scale;
        let (w, h) = win::client_size(self.hwnd);
        let size = 36.0 * k;
        let top = (self.bar_height() - size) / 2.0;
        self.btns.clear();
        let mut right = w as f32 - win::CAPTION_BUTTONS_DIP * k - 8.0 * k;

        right -= size;
        self.btns.push(Btn {
            hit: Hit::OpenExternal,
            r: RectF::xywh(right, top, size, size),
            label: None,
            glyph: '\u{E8A7}',
            tip: "Открыть в другом плеере",
        });
        let labelled = [
            (Hit::GifMenu, "Создать GIF", '\u{E786}', "Сохранить выбранный отрывок как GIF"),
            (Hit::SavePiece, "Сохранить отрывок", '\u{E8C6}', "Сдвиньте ручки на шкале и сохраните отрывок"),
        ];
        for (hit, label, glyph, tip) in labelled {
            let bw = measure(label, Font::Ui, 13.0 * k).0 + 44.0 * k;
            right -= bw + 2.0 * k;
            self.btns.push(Btn { hit, r: RectF::xywh(right, top, bw, size), label: Some(label), glyph, tip });
        }

        let row = h as f32 - 50.0 * k;
        self.btns.push(Btn {
            hit: Hit::Play,
            r: RectF::xywh(20.0 * k, row, size, size),
            label: None,
            glyph: '\u{E768}',
            tip: "Пуск и пауза (пробел)",
        });
        self.btns.push(Btn {
            hit: Hit::Mute,
            r: RectF::xywh(w as f32 - 20.0 * k - size, row, size, size),
            label: None,
            glyph: '\u{E767}',
            tip: "Звук",
        });
        self.place_video();
    }

    fn place_video(&self) {
        let k = self.scale;
        let (w, h) = win::client_size(self.hwnd);
        let area = RectF::new(0.0, self.bar_height(), w as f32, h as f32 - CONTROLS_DIP * k).inflate(-8.0 * k);
        let r = fit_video(area, self.video_size.0, self.video_size.1);
        unsafe {
            let _ = MoveWindow(self.video, r.l as i32, r.t as i32, r.w() as i32, r.h() as i32, true);
            // Otherwise the engine keeps drawing for the previous size of the window.
            if let Some(engine) = self.engine.as_ref().and_then(|e| e.cast::<IMFMediaEngineEx>().ok()) {
                let target = RECT { left: 0, top: 0, right: r.w() as i32, bottom: r.h() as i32 };
                let _ = engine.UpdateVideoStream(None, Some(&target), None);
            }
        }
    }

    /// Follows the engine: the length once it is known, and the end of the chosen piece.
    fn on_tick(&mut self) {
        let Some(engine) = &self.engine else {
            return;
        };
        unsafe {
            let duration = engine.GetDuration();
            if duration.is_finite() && duration > 0.0 && self.duration == 0.0 {
                self.duration = duration;
                self.end = duration;
                let (mut vw, mut vh) = (0u32, 0u32);
                if engine.GetNativeVideoSize(Some(&mut vw), Some(&mut vh)).is_ok() {
                    self.video_size = (vw, vh);
                    self.place_video();
                }
            }
            self.paused = engine.IsPaused().as_bool();
            if self.grip.is_none() {
                self.position = engine.GetCurrentTime();
                if !self.paused && self.position >= self.end {
                    let _ = engine.Pause();
                    let _ = engine.SetCurrentTime(self.start);
                    self.paused = true;
                }
            }
        }
        win::invalidate(self.hwnd);
    }

    fn seek(&mut self, t: f64) {
        self.position = t.clamp(0.0, self.duration);
        if let Some(engine) = &self.engine {
            unsafe {
                let _ = engine.SetCurrentTime(self.position);
            }
        }
    }

    fn toggle_play(&mut self) {
        let Some(engine) = &self.engine else {
            return;
        };
        unsafe {
            if engine.IsPaused().as_bool() {
                // Playing always stays inside the chosen piece.
                if self.position < self.start || self.position >= self.end - 0.02 {
                    let _ = engine.SetCurrentTime(self.start);
                }
                let _ = engine.Play();
            } else {
                let _ = engine.Pause();
            }
        }
    }

    fn toggle_mute(&mut self) {
        self.muted = !self.muted;
        if let Some(engine) = &self.engine {
            unsafe {
                let _ = engine.SetMuted(self.muted);
            }
        }
    }

    fn open_external(&self) {
        let file = win::wide(&self.path.to_string_lossy());
        unsafe {
            ShellExecuteW(None, w!("open"), PCWSTR(file.as_ptr()), None, None, SW_SHOWNORMAL);
        }
    }

    fn start_job(&mut self, title: &'static str, is_gif: bool, work: JobWork) {
        if self.job.is_some() || self.duration == 0.0 {
            return;
        }
        if let Some(engine) = &self.engine {
            unsafe {
                let _ = engine.Pause();
            }
        }
        let state = Arc::new(JobState::default());
        let result = Arc::new(Mutex::new(None));
        let (job_state, job_result) = (state.clone(), result.clone());
        let source = self.path.clone();
        let (start, end) = (self.start, self.end);
        let hwnd = self.hwnd.0 as isize;
        std::thread::spawn(move || {
            let outcome = work.run(&source, start, end, &job_state);
            if let Ok(mut slot) = job_result.lock() {
                *slot = Some(outcome);
            }
            unsafe {
                let _ = PostMessageW(Some(HWND(hwnd as *mut _)), WM_JOB_DONE, WPARAM(0), LPARAM(0));
            }
        });
        self.job = Some(Job { title, state, result, is_gif });
    }

    fn job_finished(&mut self) {
        let Some(job) = self.job.take() else {
            return;
        };
        let outcome = job.result.lock().ok().and_then(|mut r| r.take());
        if self.closing {
            unsafe {
                let _ = DestroyWindow(self.hwnd);
            }
            return;
        }
        match outcome {
            Some(Ok(path)) => {
                let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                if job.is_gif {
                    crate::notify::gif_saved(name);
                } else {
                    crate::notify::piece_saved(name);
                }
            }
            Some(Err(e)) => win::show_error(&format!("Не удалось сохранить: {e}")),
            None => {}
        }
        win::invalidate(self.hwnd);
    }

    /// A system menu: anything drawn in this window would stay under the video.
    fn show_gif_menu(&mut self) {
        const HIGH: i32 = 1;
        const LOW: i32 = 2;
        let Some(button) = self.btns.iter().find(|b| b.hit == Hit::GifMenu).map(|b| b.r) else {
            return;
        };
        let describe = |name: &str, q: GifQuality| {
            format!("{name}: {} кадров/с, до {} пикс.", 100 / q.delay_cs, q.max_width)
        };
        let too_long = self.end - self.start > GIF_MAX_SECONDS;
        let mut at = POINT { x: button.l as i32, y: button.b as i32 };
        let chosen = unsafe {
            let _ = ClientToScreen(self.hwnd, &mut at);
            let Ok(menu) = CreatePopupMenu() else {
                return;
            };
            let add = |id: i32, text: &str, flags: MENU_ITEM_FLAGS| {
                let text = win::wide(text);
                let _ = AppendMenuW(menu, flags, id as usize, PCWSTR(text.as_ptr()));
            };
            add(HIGH, &describe("Высокое качество", GIF_HIGH), MF_STRING);
            add(LOW, &describe("Маленький файл", GIF_LOW), MF_STRING);
            if too_long {
                let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
                add(0, "В GIF войдут первые 30 секунд", MF_STRING | MF_GRAYED);
            }
            let chosen = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_LEFTALIGN | TPM_TOPALIGN, at.x, at.y, None, self.hwnd, None);
            let _ = DestroyMenu(menu);
            chosen.0
        };
        match chosen {
            HIGH => self.start_job("Создаю GIF", true, JobWork::Gif(GIF_HIGH)),
            LOW => self.start_job("Создаю GIF", true, JobWork::Gif(GIF_LOW)),
            _ => {}
        }
    }

    fn hit_at(&self, x: i32, y: i32) -> Option<Hit> {
        self.btns.iter().find(|b| b.r.contains(x as f32, y as f32)).map(|b| b.hit)
    }

    fn grip_at(&self, x: i32, y: i32) -> Option<Grip> {
        if self.duration == 0.0 || !self.timeline().inflate(4.0 * self.scale).contains(x as f32, y as f32) {
            return None;
        }
        let near = 12.0 * self.scale;
        let fx = x as f32;
        let (to_start, to_end) = ((fx - self.x_of(self.start)).abs(), (fx - self.x_of(self.end)).abs());
        if to_start <= near && to_start <= to_end {
            Some(Grip::Start)
        } else if to_end <= near {
            Some(Grip::End)
        } else {
            Some(Grip::Position)
        }
    }

    fn drag_grip(&mut self, grip: Grip, x: i32) {
        let t = self.time_at(x);
        match grip {
            Grip::Start => {
                self.start = t.min(self.end - MIN_PIECE_SECONDS).max(0.0);
                self.seek(self.start);
            }
            Grip::End => {
                self.end = t.max(self.start + MIN_PIECE_SECONDS).min(self.duration);
                self.seek(self.end);
            }
            Grip::Position => self.seek(t.clamp(self.start, self.end)),
        }
    }

    fn press(&mut self, hit: Hit) {
        match hit {
            Hit::Play => self.toggle_play(),
            Hit::Mute => self.toggle_mute(),
            Hit::OpenExternal => self.open_external(),
            Hit::GifMenu => self.show_gif_menu(),
            Hit::SavePiece => {
                if self.is_trimmed() {
                    self.start_job("Сохраняю отрывок", false, JobWork::Piece);
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
        p.clear(crate::gfx::TRANSPARENT);
        let controls_top = h as f32 - CONTROLS_DIP * k;
        p.fill(RectF::new(0.0, self.bar_height(), w as f32, controls_top), rgba(0x000000, 0.2));

        let first_button = self.btns.iter().filter(|b| b.r.t < self.bar_height()).map(|b| b.r.l).fold(w as f32, f32::min);
        p.push_clip(RectF::new(0.0, 0.0, first_button - 8.0 * k, self.bar_height()));
        let title = RectF::new(16.0 * k, 0.0, w as f32, self.bar_height());
        p.text(&self.name, title, Font::Ui, 13.0 * k, TEXT, Align::Left);
        p.pop_clip();

        self.paint_buttons(&p);
        self.paint_timeline(&p);
        let row = h as f32 - 50.0 * k;
        let piece = self.end - self.start;
        let time = format!("{}  /  {}", clock(self.position), clock(self.duration));
        let time_r = RectF::xywh(62.0 * k, row, 300.0 * k, 36.0 * k);
        p.text(&time, time_r, Font::Ui, 13.0 * k, TEXT, Align::Left);
        if let Some(job) = &self.job {
            self.paint_job(&p, job, row);
        } else if self.is_trimmed() {
            let label = format!("Отрывок: {} – {}  ({:.1} с)", clock(self.start), clock(self.end), piece);
            let r = RectF::new(0.0, row, w as f32, row + 36.0 * k);
            p.text(&label, r, Font::Ui, 13.0 * k, accent(), Align::Center);
        } else {
            let r = RectF::new(0.0, row, w as f32, row + 36.0 * k);
            p.text("Сдвиньте ручки на шкале, чтобы выбрать отрывок", r, Font::Ui, 12.5 * k, TEXT_DIM, Align::Center);
        }
        if self.job.is_none() {
            self.paint_tooltip(&p);
        }
        self.surface.end();
    }

    fn paint_buttons(&self, p: &Painter) {
        let k = self.scale;
        let (fx, fy) = (self.mouse.0 as f32, self.mouse.1 as f32);
        for b in &self.btns {
            let enabled = self.job.is_none() && (b.hit != Hit::SavePiece || self.is_trimmed());
            if enabled && b.r.contains(fx, fy) {
                p.round(b.r, 4.0 * k, HOVER);
            }
            let color = if enabled { TEXT } else { TEXT_OFF };
            match b.label {
                Some(label) => {
                    p.icon(b.glyph, b.r.l + 18.0 * k, b.r.cy(), 15.0 * k, color);
                    let text = RectF::new(b.r.l + 34.0 * k, b.r.t, b.r.r, b.r.b);
                    p.text(label, text, Font::Ui, 13.0 * k, color, Align::Left);
                }
                None => {
                    let glyph = match b.hit {
                        Hit::Play if !self.paused => '\u{E769}',
                        Hit::Mute if self.muted => '\u{E74F}',
                        _ => b.glyph,
                    };
                    p.icon(glyph, b.r.cx(), b.r.cy(), 16.0 * k, color);
                }
            }
        }
    }

    fn paint_tooltip(&self, p: &Painter) {
        let k = self.scale;
        let (fx, fy) = (self.mouse.0 as f32, self.mouse.1 as f32);
        let Some(b) = self.btns.iter().find(|b| b.r.contains(fx, fy)) else {
            return;
        };
        let (w, _) = win::client_size(self.hwnd);
        let size = 12.0 * k;
        let tw = measure(b.tip, Font::Ui, size).0 + 18.0 * k;
        // Anything over the video would be hidden by it, so a tip stays on the bars:
        // left of the buttons in the top bar, above the buttons in the bottom row.
        let tip = if b.r.t < self.bar_height() {
            let first = self.btns.iter().filter(|o| o.r.t < self.bar_height()).map(|o| o.r.l).fold(w as f32, f32::min);
            RectF::xywh(first - tw - 6.0 * k, b.r.cy() - 14.0 * k, tw, 28.0 * k)
        } else {
            let left = (b.r.cx() - tw / 2.0).clamp(6.0 * k, (w as f32 - tw - 6.0 * k).max(6.0 * k));
            RectF::xywh(left, b.r.t - 32.0 * k, tw, 28.0 * k)
        };
        p.round(tip, 4.0 * k, FLYOUT);
        p.round_stroke(tip, 4.0 * k, FLYOUT_STROKE, 1.0);
        p.text(b.tip, tip, Font::Ui, size, TEXT, Align::Center);
    }

    fn paint_timeline(&self, p: &Painter) {
        let k = self.scale;
        let line = self.timeline();
        let cy = line.cy();
        let track = RectF::new(line.l, cy - 3.0 * k, line.r, cy + 3.0 * k);
        p.round(track, 3.0 * k, rgba(0xffffff, 0.16));
        if self.duration == 0.0 {
            return;
        }
        let (a, b) = (self.x_of(self.start), self.x_of(self.end));
        p.round(RectF::new(a, track.t, b, track.b), 3.0 * k, accent());
        for x in [a, b] {
            let grip = RectF::xywh(x - 4.0 * k, line.t, 8.0 * k, line.h());
            p.round(grip, 3.0 * k, accent());
            p.line((x, cy - 5.0 * k), (x, cy + 5.0 * k), ON_ACCENT, 1.5 * k);
        }
        let x = self.x_of(self.position);
        p.fill_circle(x, cy, 8.0 * k, rgb(0x454545));
        p.fill_circle(x, cy, 5.0 * k, TEXT);
    }

    /// Progress in the bottom bar, in place of the description of the piece.
    fn paint_job(&self, p: &Painter, job: &Job, row: f32) {
        let k = self.scale;
        let (w, _) = win::client_size(self.hwnd);
        let percent = job.state.percent.load(Ordering::Relaxed);
        let width = 300.0 * k;
        let left = (w as f32 - width) / 2.0;
        let label = format!("{}…  {percent}%", job.title);
        let text = RectF::new(left, row - 2.0 * k, left + width, row + 22.0 * k);
        p.text(&label, text, Font::Ui, 13.0 * k, TEXT, Align::Center);
        let bar = RectF::xywh(left, row + 26.0 * k, width, 4.0 * k);
        p.round(bar, 2.0 * k, rgba(0xffffff, 0.16));
        let done = RectF::new(bar.l, bar.t, bar.l + bar.w() * percent as f32 / 100.0, bar.b);
        p.round(done, 2.0 * k, accent());
    }

    fn set_piece_edge(&mut self, is_start: bool) {
        if self.duration == 0.0 {
            return;
        }
        if is_start {
            self.start = self.position.min(self.end - MIN_PIECE_SECONDS).max(0.0);
        } else {
            self.end = self.position.max(self.start + MIN_PIECE_SECONDS).min(self.duration);
        }
    }

    fn on_key(&mut self, vk: VIRTUAL_KEY) {
        let step = if win::key_down(VK_SHIFT) { 5.0 } else { 1.0 };
        match vk {
            VK_SPACE => self.toggle_play(),
            VK_LEFT => self.seek((self.position - step).max(self.start)),
            VK_RIGHT => self.seek((self.position + step).min(self.end)),
            VK_HOME => self.seek(self.start),
            VK_END => self.seek(self.end),
            _ => match vk.0 as u8 as char {
                // The usual keys of video editors for the start and the end of a piece.
                'I' => self.set_piece_edge(true),
                'O' => self.set_piece_edge(false),
                'M' => self.toggle_mute(),
                _ => {}
            },
        }
        win::invalidate(self.hwnd);
    }
}

enum JobWork {
    Piece,
    Gif(GifQuality),
}

impl JobWork {
    fn run(self, source: &Path, start: f64, end: f64, state: &JobState) -> std::result::Result<PathBuf, String> {
        match self {
            JobWork::Piece => {
                let output = videoedit::free_path(source, "отрывок", "mp4");
                videoedit::trim(source, &output, start, end, state)?;
                Ok(output)
            }
            JobWork::Gif(quality) => {
                let output = videoedit::free_path(source, "GIF", "gif");
                videoedit::export_gif(source, &output, start, end, quality, state)?;
                Ok(output)
            }
        }
    }
}

impl WndState for Player {
    fn on_message(&mut self, hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
        let caption = self.bar_height() as i32;
        let on_control = |x: i32, y: i32| self.btns.iter().any(|b| b.r.contains(x as f32, y as f32));
        if let Some(r) = win::frame_message(hwnd, msg, wp, lp, caption, true, |x, y| !on_control(x, y)) {
            return Some(r);
        }
        match msg {
            WM_CREATE => {
                self.hwnd = hwnd;
                self.surface = Surface::glass(hwnd);
                self.scale = win::dpi_scale(hwnd);
                match create_video_surface(hwnd).and_then(|video| {
                    self.video = video;
                    create_engine(video, &self.path)
                }) {
                    Ok(engine) => self.engine = Some(engine),
                    Err(e) => {
                        crate::paths::log(&format!("player: {e}"));
                        win::show_error(&format!("Не удалось открыть видео: {e}"));
                    }
                }
                self.layout();
                unsafe { SetTimer(Some(hwnd), TICK, TICK_MS, None) };
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
                self.layout();
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
                self.layout();
                Some(LRESULT(0))
            }
            WM_GETMINMAXINFO => {
                let info = unsafe { &mut *(lp.0 as *mut MINMAXINFO) };
                info.ptMinTrackSize = POINT { x: (760.0 * self.scale) as i32, y: (420.0 * self.scale) as i32 };
                Some(LRESULT(0))
            }
            WM_TIMER if wp.0 == TICK => {
                self.on_tick();
                Some(LRESULT(0))
            }
            WM_JOB_DONE => {
                self.job_finished();
                Some(LRESULT(0))
            }
            WM_MOUSEMOVE => {
                self.mouse = win::mouse_xy(lp);
                if let Some(grip) = self.grip {
                    self.drag_grip(grip, self.mouse.0);
                }
                win::invalidate(hwnd);
                Some(LRESULT(0))
            }
            WM_LBUTTONDOWN => {
                let (x, y) = win::mouse_xy(lp);
                self.mouse = (x, y);
                if self.job.is_some() {
                    return Some(LRESULT(0));
                }
                if self.hit_at(x, y).is_some() {
                    return Some(LRESULT(0));
                }
                if let Some(grip) = self.grip_at(x, y) {
                    self.grip = Some(grip);
                    self.drag_grip(grip, x);
                    unsafe { SetCapture(hwnd) };
                }
                Some(LRESULT(0))
            }
            WM_LBUTTONUP => {
                let (x, y) = win::mouse_xy(lp);
                self.mouse = (x, y);
                if self.grip.take().is_some() {
                    unsafe {
                        let _ = ReleaseCapture();
                    }
                    return Some(LRESULT(0));
                }
                if self.job.is_some() {
                    return Some(LRESULT(0));
                }
                if let Some(hit) = self.hit_at(x, y) {
                    self.press(hit);
                } else if (y as f32) > self.bar_height() && (y as f32) < self.timeline().t - 14.0 * self.scale {
                    // A click on the picture, as in any player.
                    self.toggle_play();
                }
                Some(LRESULT(0))
            }
            WM_KEYDOWN => {
                if self.job.is_none() {
                    self.on_key(VIRTUAL_KEY(wp.0 as u16));
                }
                Some(LRESULT(0))
            }
            WM_CLOSE => {
                // A running job first stops and removes its unfinished file.
                if let Some(job) = &self.job {
                    self.closing = true;
                    job.state.cancel.store(true, Ordering::Relaxed);
                    return Some(LRESULT(0));
                }
                None
            }
            WM_DESTROY => {
                unsafe {
                    let _ = KillTimer(Some(hwnd), TICK);
                }
                if let Some(engine) = self.engine.take() {
                    unsafe {
                        let _ = engine.Shutdown();
                        let _ = MFShutdown();
                    }
                }
                crate::app::window_closed();
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
    fn clock_shows_minutes_seconds_and_tenths() {
        assert_eq!(clock(0.0), "00:00.0");
        assert_eq!(clock(12.34), "00:12.3");
        assert_eq!(clock(75.96), "01:16.0");
        assert_eq!(clock(-1.0), "00:00.0");
    }

    #[test]
    fn video_is_letterboxed_and_centered() {
        let area = RectF::new(0.0, 0.0, 1000.0, 1000.0);
        let r = fit_video(area, 1600, 900);
        assert_eq!((r.w(), r.h()), (1000.0, 562.5));
        assert_eq!(r.cy(), 500.0);
    }

    #[test]
    fn unknown_video_size_uses_the_whole_area() {
        let area = RectF::new(1.0, 2.0, 3.0, 4.0);
        assert_eq!(fit_video(area, 0, 0), area);
    }
}
