//! Screen recording: frames from Windows.Graphics.Capture and sound from WASAPI go
//! into a Media Foundation sink writer, which encodes on the GPU into an MP4 file.

use crate::geom::Rect;
use crate::settings::Quality;
use crate::win::WM_KADR_REC_DONE;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use windows::Foundation::{TimeSpan, TypedEventHandler};
use windows::Graphics::Capture::*;
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::*;
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Graphics::Gdi::HMONITOR;
use windows::Win32::Media::Audio::*;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::StructuredStorage::PropVariantToStringAlloc;
use windows::Win32::System::Com::*;
use windows::Win32::System::Power::*;
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::System::WinRT::Direct3D11::*;
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;
use windows::core::*;

const TICKS_PER_SECOND: i64 = 10_000_000;
const SAMPLE_RATE: u32 = 48_000;
const CHANNELS: usize = 2;
/// The encoder may still be reading a frame after WriteSample returns, so frames
/// rotate through several textures instead of overwriting one.
const TEXTURE_POOL: usize = 8;
/// Audio is mixed slightly behind real time so late packets are not lost.
const AUDIO_DELAY_FRAMES: i64 = (SAMPLE_RATE as i64) * 30 / 1000;
/// More queued sound than this means a device drifted: old samples are dropped.
const MAX_QUEUED_SAMPLES: usize = SAMPLE_RATE as usize * CHANNELS / 4;

pub struct Options {
    /// `HMONITOR` value of the captured monitor.
    pub monitor: isize,
    pub monitor_rect: Rect,
    /// Screen coordinates, with even width and height.
    pub region: Rect,
    pub mic: bool,
    pub system_audio: bool,
    /// Device ids; `None` uses the default device in Windows.
    pub mic_device: Option<String>,
    pub system_device: Option<String>,
    pub fps: u32,
    pub quality: Quality,
    pub cursor: bool,
    pub path: PathBuf,
    /// Window that receives `WM_KADR_REC_DONE` when the file is complete; 0 for none.
    pub notify: isize,
}

#[derive(Debug, Clone, Copy)]
pub struct Stats {
    pub frames: u64,
    pub duration: Duration,
}

/// Recording time, which stands still during a pause.
#[derive(Default)]
struct Clock {
    start: Option<Instant>,
    paused_at: Option<Instant>,
    paused_total: Duration,
}

impl Clock {
    fn start(&mut self, now: Instant) {
        self.start = Some(now);
    }

    fn elapsed(&self, now: Instant) -> Duration {
        let Some(start) = self.start else {
            return Duration::ZERO;
        };
        let end = self.paused_at.unwrap_or(now);
        end.saturating_duration_since(start).saturating_sub(self.paused_total)
    }

    fn pause(&mut self, now: Instant) {
        if self.paused_at.is_none() {
            self.paused_at = Some(now);
        }
    }

    fn resume(&mut self, now: Instant) {
        if let Some(at) = self.paused_at.take() {
            self.paused_total += now.saturating_duration_since(at);
        }
    }

    fn is_paused(&self) -> bool {
        self.paused_at.is_some()
    }
}

#[derive(Default)]
struct Shared {
    stop: AtomicBool,
    clock: Mutex<Clock>,
    /// Whether the sound thread should be listening to the microphone.
    mic_on: AtomicBool,
}

impl Shared {
    fn elapsed(&self) -> Duration {
        self.clock.lock().map(|c| c.elapsed(Instant::now())).unwrap_or_default()
    }

    fn is_paused(&self) -> bool {
        self.clock.lock().map(|c| c.is_paused()).unwrap_or(false)
    }
}

pub struct Recorder {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<std::result::Result<Stats, String>>>,
}

impl Recorder {
    /// Returns once the first frame has been captured and time is running.
    pub fn start(opts: Options) -> std::result::Result<Recorder, String> {
        let shared = Arc::new(Shared::default());
        let (ready_tx, ready_rx) = mpsc::channel();
        let worker_shared = shared.clone();
        let thread = thread::Builder::new()
            .name("kadr-video".into())
            .spawn(move || run(opts, worker_shared, ready_tx))
            .map_err(|e| e.to_string())?;
        match ready_rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(())) => Ok(Recorder { shared, thread: Some(thread) }),
            Ok(Err(message)) => {
                let _ = thread.join();
                Err(message)
            }
            Err(_) => {
                shared.stop.store(true, Ordering::Relaxed);
                Err("запись не запустилась за 10 секунд".into())
            }
        }
    }

    /// Asks the recording to end; `WM_KADR_REC_DONE` follows when the file is ready.
    pub fn stop(&self) {
        self.shared.stop.store(true, Ordering::Relaxed);
    }

    pub fn toggle_pause(&self) {
        if let Ok(mut c) = self.shared.clock.lock() {
            let now = Instant::now();
            if c.is_paused() { c.resume(now) } else { c.pause(now) }
        }
    }

    pub fn is_paused(&self) -> bool {
        self.shared.is_paused()
    }

    pub fn elapsed(&self) -> Duration {
        self.shared.elapsed()
    }

    /// Takes effect at once and works even if the recording began without a microphone.
    pub fn toggle_mic(&self) {
        self.shared.mic_on.fetch_xor(true, Ordering::Relaxed);
    }

    /// Turns back off by itself if the microphone cannot be opened.
    pub fn is_mic_on(&self) -> bool {
        self.shared.mic_on.load(Ordering::Relaxed)
    }

    pub fn finish(mut self) -> std::result::Result<Stats, String> {
        self.stop();
        match self.thread.take().map(|t| t.join()) {
            Some(Ok(result)) => result,
            _ => Err("поток записи завершился аварийно".into()),
        }
    }
}

/// Average bitrate while the picture moves; a still screen takes far less.
/// `High` is about what sharp text in motion needs.
pub fn video_bitrate(w: u32, h: u32, fps: u32, quality: Quality) -> u32 {
    let bits_per_pixel = match quality {
        Quality::Standard => 0.05,
        Quality::High => 0.1,
        Quality::Max => 0.2,
    };
    (w as f64 * h as f64 * fps as f64 * bits_per_pixel).clamp(4_000_000.0, 120_000_000.0) as u32
}

fn pack(high: u32, low: u32) -> u64 {
    ((high as u64) << 32) | low as u64
}

fn run(
    opts: Options,
    shared: Arc<Shared>,
    ready: mpsc::Sender<std::result::Result<(), String>>,
) -> std::result::Result<Stats, String> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        // Keeps the screen on, like a video player does.
        SetThreadExecutionState(ES_CONTINUOUS | ES_DISPLAY_REQUIRED | ES_SYSTEM_REQUIRED);
    }
    let notify = opts.notify;
    let result = match unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) } {
        Ok(()) => {
            let r = pipeline(&opts, &shared, &ready);
            unsafe {
                let _ = MFShutdown();
            }
            r
        }
        Err(e) => Err(e),
    };
    let result = result.map_err(|e| e.message().to_string());
    if let Err(message) = &result {
        // Ignored when the start already succeeded and nobody listens anymore.
        let _ = ready.send(Err(message.clone()));
        crate::paths::log(&format!("recording failed: {message}"));
    }
    unsafe {
        SetThreadExecutionState(ES_CONTINUOUS);
        CoUninitialize();
        if notify != 0 {
            let hwnd = HWND(notify as *mut _);
            let _ = PostMessageW(Some(hwnd), WM_KADR_REC_DONE, WPARAM(0), LPARAM(0));
        }
    }
    result
}

fn create_device() -> Result<ID3D11Device> {
    let mut device = None;
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            None,
        )?;
    }
    let device = device.ok_or_else(|| Error::from(E_FAIL))?;
    // The encoder uses the same device from its own threads.
    unsafe {
        let _ = device.cast::<ID3D11Multithread>()?.SetMultithreadProtected(true);
    }
    Ok(device)
}

/// The sink writer, shared by the video and audio threads.
struct Writer(Mutex<IMFSinkWriter>);
// SAFETY: created on an MTA thread and only used from MTA threads, with calls serialized by the mutex.
unsafe impl Send for Writer {}
unsafe impl Sync for Writer {}

impl Writer {
    fn write(&self, stream: u32, sample: &IMFSample) -> Result<()> {
        let w = self.0.lock().map_err(|_| Error::from(E_FAIL))?;
        unsafe { w.WriteSample(stream, sample) }
    }
}

struct Sink {
    writer: Arc<Writer>,
    video_stream: u32,
    audio_stream: u32,
    /// The writer needs the manager to stay alive.
    _manager: IMFDXGIDeviceManager,
}

pub(crate) fn video_types(w: u32, h: u32, fps: u32, bitrate: u32) -> Result<(IMFMediaType, IMFMediaType)> {
    unsafe {
        let out = MFCreateMediaType()?;
        out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        out.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
        out.SetUINT32(&MF_MT_AVG_BITRATE, bitrate)?;
        out.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        out.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32)?;
        out.SetUINT64(&MF_MT_FRAME_SIZE, pack(w, h))?;
        out.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1))?;
        out.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
        out.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)?;
        out.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32)?;
        out.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32)?;
        out.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)?;

        let input = MFCreateMediaType()?;
        input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_ARGB32)?;
        input.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        input.SetUINT64(&MF_MT_FRAME_SIZE, pack(w, h))?;
        input.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1))?;
        input.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
        // A positive stride marks the frames as top-down; without it they come out flipped.
        input.SetUINT32(&MF_MT_DEFAULT_STRIDE, w * 4)?;
        Ok((out, input))
    }
}

pub(crate) fn audio_types() -> Result<(IMFMediaType, IMFMediaType)> {
    unsafe {
        let out = MFCreateMediaType()?;
        out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
        out.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC)?;
        out.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
        out.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, SAMPLE_RATE)?;
        out.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, CHANNELS as u32)?;
        out.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, 24_000)?; // 192 kbit/s

        let input = MFCreateMediaType()?;
        input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
        input.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM)?;
        input.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
        input.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, SAMPLE_RATE)?;
        input.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, CHANNELS as u32)?;
        input.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, (CHANNELS * 2) as u32)?;
        input.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, SAMPLE_RATE * (CHANNELS as u32) * 2)?;
        Ok((out, input))
    }
}

/// Quality-driven rate control: a still screen costs almost nothing, and the bitrate
/// rises only when the picture moves. Encoders without this option keep the average.
pub(crate) fn tune_encoder(writer: &IMFSinkWriter, stream: u32, bitrate: u32, fps: u32) {
    unsafe {
        let mut raw = std::ptr::null_mut();
        if writer.GetServiceForStream(stream, &GUID::zeroed(), &ICodecAPI::IID, &mut raw).is_err() || raw.is_null() {
            return;
        }
        let api = ICodecAPI::from_raw(raw);
        let set = |key: &GUID, value: u32| {
            let _ = api.SetValue(key, &VARIANT::from(value));
        };
        set(&CODECAPI_AVEncCommonRateControlMode, eAVEncCommonRateControlMode_PeakConstrainedVBR.0 as u32);
        set(&CODECAPI_AVEncCommonMeanBitRate, bitrate);
        set(&CODECAPI_AVEncCommonMaxBitRate, bitrate.saturating_mul(2));
        set(&CODECAPI_AVEncMPVGOPSize, fps * 2);
    }
}

fn create_sink(device: &ID3D11Device, opts: &Options) -> Result<Sink> {
    if let Some(dir) = opts.path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let path = crate::win::wide(&opts.path.to_string_lossy());
    let (w, h) = (opts.region.w as u32, opts.region.h as u32);
    unsafe {
        let mut token = 0u32;
        let mut manager = None;
        MFCreateDXGIDeviceManager(&mut token, &mut manager)?;
        let manager = manager.ok_or_else(|| Error::from(E_FAIL))?;
        manager.ResetDevice(device, token)?;

        let mut attrs = None;
        MFCreateAttributes(&mut attrs, 4)?;
        let attrs = attrs.ok_or_else(|| Error::from(E_FAIL))?;
        attrs.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)?;
        attrs.SetGUID(&MF_TRANSCODE_CONTAINERTYPE, &MFTranscodeContainerType_MPEG4)?;
        attrs.SetUnknown(&MF_SINK_WRITER_D3D_MANAGER, &manager)?;

        let writer = MFCreateSinkWriterFromURL(PCWSTR(path.as_ptr()), None, &attrs)?;
        let bitrate = video_bitrate(w, h, opts.fps, opts.quality);
        let (video_out, video_in) = video_types(w, h, opts.fps, bitrate)?;
        let video_stream = writer.AddStream(&video_out)?;
        writer.SetInputMediaType(video_stream, &video_in, None)?;
        tune_encoder(&writer, video_stream, bitrate, opts.fps);

        // Always present: a file cannot get a new track once writing began, and the
        // microphone can be switched on at any moment of the recording.
        let (audio_out, audio_in) = audio_types()?;
        let audio_stream = writer.AddStream(&audio_out)?;
        writer.SetInputMediaType(audio_stream, &audio_in, None)?;
        writer.BeginWriting()?;
        Ok(Sink {
            writer: Arc::new(Writer(Mutex::new(writer))),
            video_stream,
            audio_stream,
            _manager: manager,
        })
    }
}

fn create_textures(device: &ID3D11Device, w: u32, h: u32) -> Result<Vec<ID3D11Texture2D>> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
        ..Default::default()
    };
    (0..TEXTURE_POOL)
        .map(|_| unsafe {
            let mut tex = None;
            device.CreateTexture2D(&desc, None, Some(&mut tex))?;
            tex.ok_or_else(|| Error::from(E_FAIL))
        })
        .collect()
}

fn write_video_frame(sink: &Sink, tex: &ID3D11Texture2D, index: i64, fps: u32) -> Result<()> {
    unsafe {
        let buffer = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, tex, 0, false)?;
        let len = buffer.cast::<IMF2DBuffer>()?.GetContiguousLength()?;
        buffer.SetCurrentLength(len)?;
        let sample = MFCreateSample()?;
        sample.AddBuffer(&buffer)?;
        sample.SetSampleTime(index * TICKS_PER_SECOND / fps as i64)?;
        sample.SetSampleDuration(TICKS_PER_SECOND / fps as i64)?;
        sink.writer.write(sink.video_stream, &sample)
    }
}

/// Copies the recorded part of a captured monitor frame into `dst`.
fn copy_region(
    context: &ID3D11DeviceContext,
    frame: &Direct3D11CaptureFrame,
    dst: &ID3D11Texture2D,
    origin: (i32, i32),
    size: (u32, u32),
) -> Result<()> {
    unsafe {
        let access: IDirect3DDxgiInterfaceAccess = frame.Surface()?.cast()?;
        let src: ID3D11Texture2D = access.GetInterface()?;
        let content = frame.ContentSize()?;
        let right = (origin.0 + size.0 as i32).min(content.Width);
        let bottom = (origin.1 + size.1 as i32).min(content.Height);
        if right <= origin.0 || bottom <= origin.1 {
            return Ok(());
        }
        let area = D3D11_BOX {
            left: origin.0 as u32,
            top: origin.1 as u32,
            front: 0,
            right: right as u32,
            bottom: bottom as u32,
            back: 1,
        };
        context.CopySubresourceRegion(dst, 0, 0, 0, 0, &src, 0, Some(&area));
    }
    Ok(())
}

fn pipeline(
    opts: &Options,
    shared: &Arc<Shared>,
    ready: &mpsc::Sender<std::result::Result<(), String>>,
) -> Result<Stats> {
    let device = create_device()?;
    let context = unsafe { device.GetImmediateContext()? };

    let mut sources = SendSources { system: None, mic: None };
    if opts.system_audio {
        match AudioSource::open(true, opts.system_device.as_deref()) {
            Ok(s) => sources.system = Some(s),
            Err(e) => crate::paths::log(&format!("system audio is not available: {e}")),
        }
    }
    if opts.mic {
        match AudioSource::open(false, opts.mic_device.as_deref()) {
            Ok(s) => sources.mic = Some(s),
            Err(e) => crate::paths::log(&format!("microphone is not available: {e}")),
        }
    }
    shared.mic_on.store(sources.mic.is_some(), Ordering::Relaxed);

    let sink = create_sink(&device, opts)?;
    let (w, h) = (opts.region.w as u32, opts.region.h as u32);
    let textures = create_textures(&device, w, h)?;
    let origin = (opts.region.x - opts.monitor_rect.x, opts.region.y - opts.monitor_rect.y);

    // Capture of the whole monitor; the region is cut out of each frame on the GPU.
    let (item, winrt_device, pool, session) = unsafe {
        let interop = factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
        let item: GraphicsCaptureItem = interop.CreateForMonitor(HMONITOR(opts.monitor as *mut _))?;
        let dxgi: IDXGIDevice = device.cast()?;
        let winrt_device: IDirect3DDevice = CreateDirect3D11DeviceFromDXGIDevice(&dxgi)?.cast()?;
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &winrt_device,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            2,
            item.Size()?,
        )?;
        let session = pool.CreateCaptureSession(&item)?;
        (item, winrt_device, pool, session)
    };
    let closed = Arc::new(AtomicBool::new(false));
    let closed_flag = closed.clone();
    item.Closed(&TypedEventHandler::new(move |_, _| {
        closed_flag.store(true, Ordering::Relaxed);
        Ok(())
    }))?;
    // Options that older Windows builds lack; recording works without them.
    if let Ok(request) = GraphicsCaptureAccess::RequestAccessAsync(GraphicsCaptureAccessKind::Borderless) {
        let _ = request.join();
    }
    let _ = session.SetIsBorderRequired(false);
    let _ = session.SetIsCursorCaptureEnabled(opts.cursor);
    let _ = session.SetMinUpdateInterval(TimeSpan { Duration: TICKS_PER_SECOND / opts.fps as i64 });
    session.StartCapture()?;

    // Time starts with the first real frame, so the video never opens on black.
    let waiting_since = Instant::now();
    let mut current = 0usize;
    let mut pool_size = item.Size()?;
    let first = loop {
        if let Ok(frame) = pool.TryGetNextFrame() {
            break frame;
        }
        if waiting_since.elapsed() > Duration::from_secs(5) {
            return Err(Error::new(E_FAIL, "экран не отдаёт кадры"));
        }
        thread::sleep(Duration::from_millis(2));
    };
    copy_region(&context, &first, &textures[current], origin, (w, h))?;
    drop(first);

    shared.clock.lock().map_err(|_| Error::from(E_FAIL))?.start(Instant::now());

    let audio_thread = {
        let writer = sink.writer.clone();
        let stream = sink.audio_stream;
        let shared = shared.clone();
        let mic_device = opts.mic_device.clone();
        thread::Builder::new()
            .name("kadr-audio".into())
            .spawn(move || audio_loop(sources, mic_device, writer, stream, shared))
            .expect("failed to start the audio thread")
    };
    let _ = ready.send(Ok(()));

    let interval = Duration::from_secs_f64(1.0 / opts.fps as f64);
    let mut next_tick = Instant::now();
    let mut last_index = -1i64;
    let mut frames = 0u64;
    let mut result = Ok(());
    while !shared.stop.load(Ordering::Relaxed) && !closed.load(Ordering::Relaxed) {
        next_tick += interval;
        let now = Instant::now();
        if next_tick > now {
            thread::sleep(next_tick - now);
        } else if now - next_tick > interval * 4 {
            next_tick = now; // Fell behind: skip ahead instead of rushing to catch up.
        }

        let mut newest = None;
        while let Ok(frame) = pool.TryGetNextFrame() {
            newest = Some(frame);
        }
        if let Some(frame) = newest {
            // The monitor's resolution changed during the recording.
            let content = frame.ContentSize()?;
            if content != pool_size {
                let size = SizeInt32 { Width: content.Width, Height: content.Height };
                pool.Recreate(&winrt_device, DirectXPixelFormat::B8G8R8A8UIntNormalized, 2, size)?;
                pool_size = size;
            }
            let target = (current + 1) % TEXTURE_POOL;
            copy_region(&context, &frame, &textures[target], origin, (w, h))?;
            current = target;
        }

        if shared.is_paused() {
            continue;
        }
        let index = (shared.elapsed().as_secs_f64() * opts.fps as f64).round() as i64;
        if index <= last_index {
            continue;
        }
        last_index = index;
        // An unchanged screen still gets frames: the file stays at a constant rate.
        if let Err(e) = write_video_frame(&sink, &textures[current], index, opts.fps) {
            result = Err(e);
            break;
        }
        frames += 1;
    }

    shared.stop.store(true, Ordering::Relaxed);
    let duration = shared.elapsed();
    let _ = audio_thread.join();
    let _ = session.Close();
    let _ = pool.Close();
    result?;
    unsafe {
        let writer = sink.writer.0.lock().map_err(|_| Error::from(E_FAIL))?;
        writer.Finalize()?;
    }
    Ok(Stats { frames, duration })
}

/// One sound device, delivering 48 kHz stereo floats.
struct AudioSource {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    queue: VecDeque<f32>,
}

struct SendSources {
    system: Option<AudioSource>,
    mic: Option<AudioSource>,
}
// SAFETY: the interfaces are created on an MTA thread and used from one MTA thread at a time.
unsafe impl Send for SendSources {}

impl AudioSource {
    /// `loopback` records what the PC plays; otherwise a microphone. A chosen device
    /// that is no longer connected falls back to the default one.
    fn open(loopback: bool, device_id: Option<&str>) -> Result<Self> {
        const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
        const BUFFER_100NS: i64 = 2_000_000;
        unsafe {
            let devices: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            let flow = if loopback { eRender } else { eCapture };
            let chosen = device_id.and_then(|id| {
                let id = crate::win::wide(id);
                devices.GetDevice(PCWSTR(id.as_ptr())).ok()
            });
            let device = match chosen {
                Some(device) => device,
                None => devices.GetDefaultAudioEndpoint(flow, eConsole)?,
            };
            let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
            let format = WAVEFORMATEX {
                wFormatTag: WAVE_FORMAT_IEEE_FLOAT,
                nChannels: CHANNELS as u16,
                nSamplesPerSec: SAMPLE_RATE,
                nAvgBytesPerSec: SAMPLE_RATE * CHANNELS as u32 * 4,
                nBlockAlign: (CHANNELS * 4) as u16,
                wBitsPerSample: 32,
                cbSize: 0,
            };
            let mut flags = AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
            if loopback {
                flags |= AUDCLNT_STREAMFLAGS_LOOPBACK;
            }
            client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, BUFFER_100NS, 0, &format, None)?;
            let capture: IAudioCaptureClient = client.GetService()?;
            client.Start()?;
            Ok(Self { client, capture, queue: VecDeque::new() })
        }
    }

    fn drain_device(&mut self) -> Result<()> {
        unsafe {
            while self.capture.GetNextPacketSize()? > 0 {
                let mut data = std::ptr::null_mut();
                let mut frames = 0u32;
                let mut flags = 0u32;
                self.capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;
                let count = frames as usize * CHANNELS;
                if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 {
                    self.queue.extend(std::iter::repeat_n(0.0, count));
                } else {
                    self.queue.extend(std::slice::from_raw_parts(data as *const f32, count));
                }
                self.capture.ReleaseBuffer(frames)?;
            }
        }
        if self.queue.len() > MAX_QUEUED_SAMPLES {
            let extra = self.queue.len() - MAX_QUEUED_SAMPLES / 4;
            self.queue.drain(..extra);
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct AudioDevice {
    pub id: String,
    pub name: String,
}

unsafe fn take_co_string(p: PWSTR) -> Option<String> {
    unsafe {
        let text = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as _));
        text
    }
}

unsafe fn device_name(device: &IMMDevice) -> Option<String> {
    unsafe {
        let store = device.OpenPropertyStore(STGM_READ).ok()?;
        let value = store.GetValue(&PKEY_Device_FriendlyName).ok()?;
        take_co_string(PropVariantToStringAlloc(&value).ok()?)
    }
}

/// Connected devices: outputs when `loopback` is set, otherwise microphones.
pub fn audio_devices(loopback: bool) -> Vec<AudioDevice> {
    let flow = if loopback { eRender } else { eCapture };
    let list = || -> Result<Vec<AudioDevice>> {
        unsafe {
            let devices: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            let collection = devices.EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE)?;
            let mut out = Vec::new();
            for i in 0..collection.GetCount()? {
                let device = collection.Item(i)?;
                let id = take_co_string(device.GetId()?);
                if let (Some(id), Some(name)) = (id, device_name(&device)) {
                    out.push(AudioDevice { id, name });
                }
            }
            Ok(out)
        }
    };
    list().unwrap_or_default()
}

pub fn default_device_name(loopback: bool) -> Option<String> {
    let flow = if loopback { eRender } else { eCapture };
    unsafe {
        let devices: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()?;
        device_name(&devices.GetDefaultAudioEndpoint(flow, eConsole).ok()?)
    }
}

/// Listens to a microphone only to show its level; nothing is stored. Stops when dropped.
pub struct MicTest {
    stop: Arc<AtomicBool>,
    failed: Arc<AtomicBool>,
    level_bits: Arc<AtomicU32>,
    thread: Option<JoinHandle<()>>,
}

impl MicTest {
    pub fn start(device: Option<String>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let failed = Arc::new(AtomicBool::new(false));
        let level_bits = Arc::new(AtomicU32::new(0));
        let (s, f, l) = (stop.clone(), failed.clone(), level_bits.clone());
        let thread = thread::spawn(move || unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            match AudioSource::open(false, device.as_deref()) {
                Ok(mut mic) => {
                    let mut level = 0f32;
                    while !s.load(Ordering::Relaxed) {
                        thread::sleep(Duration::from_millis(30));
                        if mic.drain_device().is_err() {
                            f.store(true, Ordering::Relaxed);
                            break;
                        }
                        let peak = mic.queue.drain(..).fold(0f32, |p, v| p.max(v.abs()));
                        // Falls slowly, so that short sounds are visible.
                        level = peak.max(level * 0.8);
                        l.store(level.to_bits(), Ordering::Relaxed);
                    }
                    let _ = mic.client.Stop();
                    drop(mic);
                }
                Err(_) => f.store(true, Ordering::Relaxed),
            }
            CoUninitialize();
        });
        Self { stop, failed, level_bits, thread: Some(thread) }
    }

    pub fn level(&self) -> f32 {
        f32::from_bits(self.level_bits.load(Ordering::Relaxed))
    }

    pub fn failed(&self) -> bool {
        self.failed.load(Ordering::Relaxed)
    }
}

impl Drop for MicTest {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Adds queued samples into `out`. A device with nothing queued contributes
/// silence: Windows delivers no system sound at all while nothing is playing.
fn mix_into(out: &mut [f32], queue: &mut VecDeque<f32>) {
    for slot in out {
        match queue.pop_front() {
            Some(v) => *slot += v,
            None => break,
        }
    }
}

fn to_pcm16(mix: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(mix.len() * 2);
    for v in mix {
        let sample = (v.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
        bytes.extend_from_slice(&sample.to_le_bytes());
    }
    bytes
}

fn write_audio(writer: &Writer, stream: u32, pcm: &[u8], first_frame: i64, frames: i64) -> Result<()> {
    unsafe {
        let buffer = MFCreateMemoryBuffer(pcm.len() as u32)?;
        let mut data = std::ptr::null_mut();
        buffer.Lock(&mut data, None, None)?;
        std::ptr::copy_nonoverlapping(pcm.as_ptr(), data, pcm.len());
        buffer.Unlock()?;
        buffer.SetCurrentLength(pcm.len() as u32)?;
        let sample = MFCreateSample()?;
        sample.AddBuffer(&buffer)?;
        sample.SetSampleTime(first_frame * TICKS_PER_SECOND / SAMPLE_RATE as i64)?;
        sample.SetSampleDuration(frames * TICKS_PER_SECOND / SAMPLE_RATE as i64)?;
        writer.write(stream, &sample)
    }
}

/// Follows the button on the recording panel. A switched off microphone is released rather
/// than ignored, so Windows no longer shows it as being in use.
fn follow_mic_switch(mic: &mut Option<AudioSource>, device: Option<&str>, shared: &Shared) {
    let wanted = shared.mic_on.load(Ordering::Relaxed);
    if wanted && mic.is_none() {
        match AudioSource::open(false, device) {
            Ok(opened) => *mic = Some(opened),
            Err(e) => {
                crate::paths::log(&format!("microphone is not available: {e}"));
                shared.mic_on.store(false, Ordering::Relaxed);
                crate::notify::in_background(|| {
                    crate::notify::info("Микрофон недоступен", "Запись продолжается без него", None)
                });
            }
        }
    } else if !wanted {
        if let Some(released) = mic.take() {
            unsafe {
                let _ = released.client.Stop();
            }
        }
    }
}

/// Reads what the device has. A device that disappeared mid-recording is left out.
fn read_source(source: &mut Option<AudioSource>) {
    if let Some(device) = source {
        if let Err(e) = device.drain_device() {
            crate::paths::log(&format!("audio device lost: {e}"));
            *source = None;
        }
    }
}

fn audio_loop(sources: SendSources, mic_device: Option<String>, writer: Arc<Writer>, stream: u32, shared: Arc<Shared>) {
    let SendSources { mut system, mut mic } = sources;
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let mut written = 0i64;
    let mut mix: Vec<f32> = Vec::new();
    while !shared.stop.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_millis(10));
        follow_mic_switch(&mut mic, mic_device.as_deref(), &shared);
        read_source(&mut system);
        read_source(&mut mic);
        if shared.is_paused() {
            for s in [&mut system, &mut mic].into_iter().flatten() {
                s.queue.clear();
            }
            continue;
        }
        let target = (shared.elapsed().as_secs_f64() * SAMPLE_RATE as f64) as i64 - AUDIO_DELAY_FRAMES;
        let frames = target - written;
        if frames < (SAMPLE_RATE / 100) as i64 {
            continue;
        }
        mix.clear();
        mix.resize(frames as usize * CHANNELS, 0.0);
        for s in [&mut system, &mut mic].into_iter().flatten() {
            mix_into(&mut mix, &mut s.queue);
        }
        if let Err(e) = write_audio(&writer, stream, &to_pcm16(&mix), written, frames) {
            crate::paths::log(&format!("audio write failed: {e}"));
            break;
        }
        written += frames;
    }
    for s in [&system, &mic].into_iter().flatten() {
        unsafe {
            let _ = s.client.Stop();
        }
    }
    drop(system);
    drop(mic);
    unsafe { CoUninitialize() };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_does_not_run_during_a_pause() {
        let t0 = Instant::now();
        let mut c = Clock::default();
        c.start(t0);
        c.pause(t0 + Duration::from_secs(5));
        assert_eq!(c.elapsed(t0 + Duration::from_secs(50)), Duration::from_secs(5));
        c.resume(t0 + Duration::from_secs(60));
        assert_eq!(c.elapsed(t0 + Duration::from_secs(61)), Duration::from_secs(6));
    }

    #[test]
    fn clock_is_zero_before_the_start() {
        assert_eq!(Clock::default().elapsed(Instant::now()), Duration::ZERO);
    }

    #[test]
    fn mix_sums_sources_and_treats_missing_data_as_silence() {
        let mut out = vec![0.0f32; 4];
        let mut a: VecDeque<f32> = [0.25, 0.25, 0.25, 0.25].into();
        let mut b: VecDeque<f32> = [0.5].into();
        mix_into(&mut out, &mut a);
        mix_into(&mut out, &mut b);
        assert_eq!(out, vec![0.75, 0.25, 0.25, 0.25]);
    }

    #[test]
    fn loud_mix_is_limited_instead_of_wrapping_around() {
        let pcm = to_pcm16(&[2.0, -2.0]);
        assert_eq!(i16::from_le_bytes([pcm[0], pcm[1]]), i16::MAX);
        assert_eq!(i16::from_le_bytes([pcm[2], pcm[3]]), -i16::MAX);
    }

    #[test]
    fn bitrate_stays_in_a_sane_range() {
        assert_eq!(video_bitrate(160, 90, 60, Quality::High), 4_000_000);
        assert_eq!(video_bitrate(2560, 1440, 60, Quality::High), 22_118_400);
        assert_eq!(video_bitrate(7680, 4320, 120, Quality::Max), 120_000_000);
    }

    #[test]
    fn better_quality_and_more_frames_mean_more_bits() {
        let at = |fps, q| video_bitrate(2560, 1440, fps, q);
        assert!(at(60, Quality::Standard) < at(60, Quality::High));
        assert!(at(60, Quality::High) < at(60, Quality::Max));
        assert!(at(30, Quality::High) < at(60, Quality::High));
    }

    /// Reads the default microphone for 2 seconds; nothing is stored.
    /// Run with `cargo test microphone_ -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn microphone_level() {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        let mut mic = AudioSource::open(false, None).expect("open microphone");
        let (mut samples, mut peak) = (0usize, 0f32);
        for _ in 0..200 {
            thread::sleep(Duration::from_millis(10));
            mic.drain_device().expect("read microphone");
            samples += mic.queue.len();
            peak = mic.queue.drain(..).fold(peak, |p, v| p.max(v.abs()));
        }
        eprintln!("samples: {samples}, peak: {peak}");
        assert!(samples > 0, "the microphone delivered no data");
    }

    fn clip_options(path: PathBuf, fps: u32, quality: Quality) -> Options {
        let monitors = crate::capture::monitors();
        let m = monitors.first().expect("no monitor");
        Options {
            monitor: m.handle.0 as isize,
            monitor_rect: m.rect,
            region: crate::geom::fit_for_video(Rect::new(m.rect.x + 100, m.rect.y + 100, 1280, 720), m.rect),
            mic: false,
            system_audio: true,
            mic_device: None,
            system_device: None,
            fps,
            quality,
            cursor: true,
            path,
            notify: 0,
        }
    }

    /// Records the real screen. Run with `cargo test records_ -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn records_a_short_clip() {
        let path = std::env::temp_dir().join(format!("kadr-test-{}.mp4", std::process::id()));
        let rec = Recorder::start(clip_options(path.clone(), 60, Quality::High)).expect("start");
        thread::sleep(Duration::from_secs(3));
        let stats = rec.finish().expect("finish");
        eprintln!("{stats:?} -> {}", path.display());
        assert!(stats.frames >= 170, "expected about 180 frames, got {}", stats.frames);
        assert!(std::fs::metadata(&path).expect("file").len() > 1000);
        std::fs::remove_file(&path).ok();
    }

    /// Begins without any sound, then switches the microphone on and off again. Keeps the
    /// clip for ffprobe. Run with `cargo test microphone_switch -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn microphone_switch_during_a_silent_recording() {
        let path = std::env::temp_dir().join("kadr-mic-switch.mp4");
        let mut options = clip_options(path.clone(), 30, Quality::Standard);
        options.system_audio = false;
        let rec = Recorder::start(options).expect("start");
        assert!(!rec.is_mic_on());
        thread::sleep(Duration::from_secs(1));
        rec.toggle_mic();
        thread::sleep(Duration::from_secs(1));
        assert!(rec.is_mic_on(), "the microphone did not open");
        rec.toggle_mic();
        thread::sleep(Duration::from_secs(1));
        assert!(!rec.is_mic_on());
        let stats = rec.finish().expect("finish");
        eprintln!("{stats:?} -> {}", path.display());
    }

    /// Writes one clip per setting and keeps them for inspection with ffprobe.
    /// Run with `cargo test settings_matrix -- --ignored --nocapture --test-threads=1`.
    #[test]
    #[ignore]
    fn settings_matrix() {
        let dir = std::env::temp_dir().join("kadr-matrix");
        std::fs::create_dir_all(&dir).unwrap();
        let cases = [
            (30, Quality::High),
            (60, Quality::Standard),
            (60, Quality::High),
            (60, Quality::Max),
            (120, Quality::High),
        ];
        for (fps, quality) in cases {
            let path = dir.join(format!("{fps}-{quality:?}.mp4"));
            let rec = Recorder::start(clip_options(path.clone(), fps, quality)).expect("start");
            thread::sleep(Duration::from_secs(4));
            let stats = rec.finish().expect("finish");
            let size = std::fs::metadata(&path).unwrap().len() / 1024;
            eprintln!("{fps} fps {quality:?}: {} frames, {size} KB, {}", stats.frames, path.display());
        }
    }

    #[test]
    #[ignore]
    fn lists_audio_devices() {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        for loopback in [false, true] {
            eprintln!("default ({loopback}): {:?}", default_device_name(loopback));
            for d in audio_devices(loopback) {
                eprintln!("  {}", d.name);
                assert!(!d.id.is_empty());
            }
        }
    }
}
