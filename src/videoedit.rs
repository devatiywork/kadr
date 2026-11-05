//! Cutting a piece out of a recording and turning a piece into a GIF.
//! Both run on a background thread and read the file through Media Foundation.

use crate::geom::Rect;
use crate::image::Image;
use crate::record;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use windows::Win32::Foundation::E_ABORT;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::*;
use windows::core::*;

const TICKS_PER_SECOND: f64 = 10_000_000.0;
const FIRST_VIDEO: u32 = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
const FIRST_AUDIO: u32 = MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32;
const ALL_STREAMS: u32 = MF_SOURCE_READER_ALL_STREAMS.0 as u32;

/// A GIF has no sound and no real compression between frames, so it stays short.
pub const GIF_MAX_SECONDS: f64 = 30.0;

/// What the job shares with the window that started it.
#[derive(Default)]
pub struct JobState {
    pub percent: AtomicU32,
    pub cancel: AtomicBool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct GifQuality {
    /// Time between frames in hundredths of a second, the unit of the GIF format.
    pub delay_cs: u16,
    pub max_width: u32,
    pub max_height: u32,
    /// Palette search effort: 1 is the slowest and best, 30 the fastest.
    pub speed: i32,
}

pub const GIF_HIGH: GifQuality = GifQuality { delay_cs: 4, max_width: 1280, max_height: 720, speed: 10 };
pub const GIF_LOW: GifQuality = GifQuality { delay_cs: 8, max_width: 640, max_height: 360, speed: 20 };

/// A free file name next to the source: `name (отрывок).mp4`, `name (отрывок 2).mp4`.
pub fn free_path(source: &Path, label: &str, extension: &str) -> PathBuf {
    let dir = source.parent().unwrap_or(Path::new("."));
    let stem = source.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let first = dir.join(format!("{stem} ({label}).{extension}"));
    if !first.exists() {
        return first;
    }
    (2..)
        .map(|n| dir.join(format!("{stem} ({label} {n}).{extension}")))
        .find(|p| !p.exists())
        .expect("an unused file name exists")
}

fn unpack(v: u64) -> (u32, u32) {
    ((v >> 32) as u32, v as u32)
}

fn open_reader(path: &Path, convert_to_rgb: bool) -> Result<IMFSourceReader> {
    let url = crate::win::wide(&path.to_string_lossy());
    unsafe {
        let mut attrs = None;
        MFCreateAttributes(&mut attrs, 2)?;
        let attrs = attrs.ok_or_else(|| Error::from(E_ABORT))?;
        attrs.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)?;
        if convert_to_rgb {
            attrs.SetUINT32(&MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, 1)?;
        }
        MFCreateSourceReaderFromURL(PCWSTR(url.as_ptr()), &attrs)
    }
}

fn request_type(reader: &IMFSourceReader, stream: u32, major: &GUID, subtype: &GUID) -> Result<IMFMediaType> {
    unsafe {
        let wanted = MFCreateMediaType()?;
        wanted.SetGUID(&MF_MT_MAJOR_TYPE, major)?;
        wanted.SetGUID(&MF_MT_SUBTYPE, subtype)?;
        reader.SetCurrentMediaType(stream, None, &wanted)?;
        reader.GetCurrentMediaType(stream)
    }
}

fn seek(reader: &IMFSourceReader, seconds: f64) -> Result<()> {
    let position = PROPVARIANT::from((seconds * TICKS_PER_SECOND) as i64);
    unsafe { reader.SetCurrentPosition(&GUID::zeroed(), &position) }
}

fn report(state: &JobState, done: f64, total: f64) {
    let percent = if total > 0.0 { (done / total * 100.0).clamp(0.0, 100.0) } else { 0.0 };
    state.percent.store(percent as u32, Ordering::Relaxed);
}

fn cancelled(state: &JobState) -> Result<()> {
    if state.cancel.load(Ordering::Relaxed) { Err(Error::from(E_ABORT)) } else { Ok(()) }
}

/// Starts Media Foundation for one job on this thread and cleans up a failed output.
fn run_job(output: &Path, job: impl FnOnce() -> Result<()>) -> std::result::Result<(), String> {
    let result = unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let result = match MFStartup(MF_VERSION, MFSTARTUP_FULL) {
            Ok(()) => {
                let r = job();
                let _ = MFShutdown();
                r
            }
            Err(e) => Err(e),
        };
        CoUninitialize();
        result
    };
    result.map_err(|e| {
        let _ = std::fs::remove_file(output);
        e.message().to_string()
    })
}

/// Saves the piece from `start` to `end` as a new file. The video is encoded again,
/// because only that allows cutting exactly at the chosen frame.
pub fn trim(source: &Path, output: &Path, start: f64, end: f64, state: &JobState) -> std::result::Result<(), String> {
    run_job(output, || trim_inner(source, output, start, end, state))
}

fn trim_inner(source: &Path, output: &Path, start: f64, end: f64, state: &JobState) -> Result<()> {
    let start_ticks = (start * TICKS_PER_SECOND) as i64;
    let end_ticks = (end * TICKS_PER_SECOND) as i64;
    let reader = open_reader(source, false)?;
    let path = crate::win::wide(&output.to_string_lossy());
    unsafe {
        let video_in = request_type(&reader, FIRST_VIDEO, &MFMediaType_Video, &MFVideoFormat_NV12)?;
        let (w, h) = unpack(video_in.GetUINT64(&MF_MT_FRAME_SIZE)?);
        let (num, den) = unpack(video_in.GetUINT64(&MF_MT_FRAME_RATE)?);
        let fps = (num as f64 / den.max(1) as f64).round().max(1.0) as u32;
        let audio_in = request_type(&reader, FIRST_AUDIO, &MFMediaType_Audio, &MFAudioFormat_PCM).ok();

        let mut attrs = None;
        MFCreateAttributes(&mut attrs, 2)?;
        let attrs = attrs.ok_or_else(|| Error::from(E_ABORT))?;
        attrs.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)?;
        attrs.SetGUID(&MF_TRANSCODE_CONTAINERTYPE, &MFTranscodeContainerType_MPEG4)?;
        let writer = MFCreateSinkWriterFromURL(PCWSTR(path.as_ptr()), None, &attrs)?;

        let bitrate = record::video_bitrate(w, h, fps, crate::settings::get().quality);
        let (video_out, _) = record::video_types(w, h, fps, bitrate)?;
        let video_stream = writer.AddStream(&video_out)?;
        writer.SetInputMediaType(video_stream, &video_in, None)?;
        record::tune_encoder(&writer, video_stream, bitrate, fps);
        let audio_stream = match &audio_in {
            Some(input) => {
                let (audio_out, _) = record::audio_types()?;
                let stream = writer.AddStream(&audio_out)?;
                writer.SetInputMediaType(stream, input, None)?;
                Some(stream)
            }
            None => None,
        };
        writer.BeginWriting()?;
        seek(&reader, start)?;

        // The two streams are read in turns, whichever is behind, to keep them together.
        let mut video_time = start_ticks;
        let mut audio_time = start_ticks;
        let mut video_done = false;
        let mut audio_done = audio_stream.is_none();
        while !(video_done && audio_done) {
            cancelled(state)?;
            let read_video = !video_done && (audio_done || video_time <= audio_time);
            let from = if read_video { FIRST_VIDEO } else { FIRST_AUDIO };
            let (mut flags, mut time) = (0u32, 0i64);
            let mut sample = None;
            reader.ReadSample(from, 0, None, Some(&mut flags), Some(&mut time), Some(&mut sample))?;
            let ended = flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0;
            if ended || time >= end_ticks {
                if read_video { video_done = true } else { audio_done = true }
                continue;
            }
            if read_video { video_time = time } else { audio_time = time }
            let Some(sample) = sample else {
                continue;
            };
            let duration = sample.GetSampleDuration().unwrap_or(0);
            // A seek lands on an earlier key frame: everything before the cut is skipped.
            if time + duration <= start_ticks || (read_video && time < start_ticks) {
                continue;
            }
            sample.SetSampleTime((time - start_ticks).max(0))?;
            let target = if read_video { Some(video_stream) } else { audio_stream };
            if let Some(stream) = target {
                writer.WriteSample(stream, &sample)?;
            }
            if read_video {
                report(state, (time - start_ticks) as f64, (end_ticks - start_ticks) as f64);
            }
        }
        writer.Finalize()?;
    }
    state.percent.store(100, Ordering::Relaxed);
    Ok(())
}

/// The smallest rectangle containing every pixel that differs between two equal-size frames.
fn changed_bounds(previous: &Image, current: &Image) -> Option<Rect> {
    let (w, h) = (current.w as usize, current.h as usize);
    let (mut x0, mut y0, mut x1, mut y1) = (w, h, 0usize, 0usize);
    for y in 0..h {
        let start = y * w * 4;
        let (a, b) = (&previous.bgra[start..start + w * 4], &current.bgra[start..start + w * 4]);
        if a == b {
            continue;
        }
        y0 = y0.min(y);
        y1 = y1.max(y);
        for x in 0..w {
            if a[x * 4..x * 4 + 3] != b[x * 4..x * 4 + 3] {
                x0 = x0.min(x);
                x1 = x1.max(x);
            }
        }
    }
    (x1 >= x0 && y1 >= y0 && x0 < w).then(|| Rect::new(x0 as i32, y0 as i32, (x1 - x0 + 1) as i32, (y1 - y0 + 1) as i32))
}

/// The pixels of a decoded frame, top row first, whichever way the decoder stores them.
fn frame_pixels(sample: &IMFSample, w: u32, h: u32) -> Result<Image> {
    unsafe {
        let buffer = sample.ConvertToContiguousBuffer()?;
        let buffer2d: IMF2DBuffer = buffer.cast()?;
        let mut first_row = std::ptr::null_mut();
        let mut pitch = 0i32;
        buffer2d.Lock2D(&mut first_row, &mut pitch)?;
        let row_bytes = w as usize * 4;
        let mut bgra = Vec::with_capacity(row_bytes * h as usize);
        for y in 0..h as isize {
            let row = first_row.offset(y * pitch as isize);
            bgra.extend_from_slice(std::slice::from_raw_parts(row, row_bytes));
        }
        buffer2d.Unlock2D()?;
        for px in bgra.chunks_exact_mut(4) {
            px[3] = 255;
        }
        Ok(Image::new(w, h, bgra))
    }
}

struct PendingFrame {
    rgba: Vec<u8>,
    area: Rect,
    delay_cs: u16,
}

fn write_gif_frame<W: std::io::Write>(encoder: &mut gif::Encoder<W>, mut pending: PendingFrame, speed: i32) -> Result<()> {
    let mut frame =
        gif::Frame::from_rgba_speed(pending.area.w as u16, pending.area.h as u16, &mut pending.rgba, speed);
    frame.left = pending.area.x as u16;
    frame.top = pending.area.y as u16;
    frame.delay = pending.delay_cs;
    frame.dispose = gif::DisposalMethod::Keep;
    encoder.write_frame(&frame).map_err(|e| Error::new(E_ABORT, e.to_string()))
}

fn rgba_of(img: &Image) -> Vec<u8> {
    let mut rgba = img.bgra.clone();
    for px in rgba.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    rgba
}

/// Saves the piece as an animated GIF. Only the part of each frame that changed is
/// stored, which keeps recordings of a mostly still screen small.
pub fn export_gif(
    source: &Path,
    output: &Path,
    start: f64,
    end: f64,
    quality: GifQuality,
    state: &JobState,
) -> std::result::Result<(), String> {
    run_job(output, || gif_inner(source, output, start, end.min(start + GIF_MAX_SECONDS), quality, state))
}

fn gif_inner(source: &Path, output: &Path, start: f64, end: f64, q: GifQuality, state: &JobState) -> Result<()> {
    let to_error = |e: String| Error::new(E_ABORT, e);
    let reader = open_reader(source, true)?;
    unsafe {
        reader.SetStreamSelection(ALL_STREAMS, false)?;
        reader.SetStreamSelection(FIRST_VIDEO, true)?;
        let current = request_type(&reader, FIRST_VIDEO, &MFMediaType_Video, &MFVideoFormat_RGB32)?;
        let (w, h) = unpack(current.GetUINT64(&MF_MT_FRAME_SIZE)?);
        seek(&reader, start)?;

        let file = std::fs::File::create(output).map_err(|e| to_error(e.to_string()))?;
        let mut encoder = None;
        let mut previous: Option<Image> = None;
        let mut pending: Option<PendingFrame> = None;
        let interval = q.delay_cs as f64 / 100.0;
        let mut next_time = start;
        loop {
            cancelled(state)?;
            let (mut flags, mut ticks) = (0u32, 0i64);
            let mut sample = None;
            reader.ReadSample(FIRST_VIDEO, 0, None, Some(&mut flags), Some(&mut ticks), Some(&mut sample))?;
            let time = ticks as f64 / TICKS_PER_SECOND;
            if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 || time >= end {
                break;
            }
            let Some(sample) = sample else {
                continue;
            };
            if time < next_time {
                continue;
            }
            next_time += interval;
            report(state, time - start, end - start);
            let img = frame_pixels(&sample, w, h)?.thumbnail(q.max_width, q.max_height);
            let encoder = match &mut encoder {
                Some(e) => e,
                None => {
                    let mut e = gif::Encoder::new(&file, img.w as u16, img.h as u16, &[])
                        .map_err(|e| to_error(e.to_string()))?;
                    e.set_repeat(gif::Repeat::Infinite).map_err(|e| to_error(e.to_string()))?;
                    encoder.insert(e)
                }
            };
            let area = match &previous {
                Some(before) => changed_bounds(before, &img),
                None => Some(img.bounds()),
            };
            match area {
                Some(area) => {
                    if let Some(frame) = pending.take() {
                        write_gif_frame(encoder, frame, q.speed)?;
                    }
                    pending = Some(PendingFrame { rgba: rgba_of(&img.crop(area)), area, delay_cs: q.delay_cs });
                    previous = Some(img);
                }
                // Nothing changed: the frame on the screen simply stays longer.
                None => {
                    if let Some(frame) = &mut pending {
                        frame.delay_cs = frame.delay_cs.saturating_add(q.delay_cs);
                    }
                }
            }
        }
        if let (Some(encoder), Some(frame)) = (&mut encoder, pending) {
            write_gif_frame(encoder, frame, q.speed)?;
        }
    }
    state.percent.store(100, Ordering::Relaxed);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(w: u32, h: u32, v: u8) -> Image {
        Image::new(w, h, [v, v, v, 255].repeat((w * h) as usize))
    }

    fn set_px(img: &mut Image, x: u32, y: u32) {
        let i = ((y * img.w + x) * 4) as usize;
        img.bgra[i] = 7;
    }

    #[test]
    fn identical_frames_have_no_changes() {
        assert_eq!(changed_bounds(&solid(8, 8, 1), &solid(8, 8, 1)), None);
    }

    #[test]
    fn change_is_limited_to_the_changed_pixels() {
        let a = solid(20, 10, 1);
        let mut b = solid(20, 10, 1);
        set_px(&mut b, 3, 2);
        set_px(&mut b, 7, 5);
        assert_eq!(changed_bounds(&a, &b), Some(Rect::new(3, 2, 5, 4)));
    }

    #[test]
    fn single_changed_corner_pixel_is_found() {
        let a = solid(4, 4, 1);
        let mut b = solid(4, 4, 1);
        set_px(&mut b, 0, 0);
        assert_eq!(changed_bounds(&a, &b), Some(Rect::new(0, 0, 1, 1)));
    }

    #[test]
    fn output_names_do_not_overwrite_existing_files() {
        let dir = std::env::temp_dir().join(format!("kadr-names-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let source = dir.join("Запись.mp4");
        let first = free_path(&source, "отрывок", "mp4");
        assert_eq!(first.file_name().unwrap().to_string_lossy(), "Запись (отрывок).mp4");
        std::fs::write(&first, b"x").unwrap();
        let second = free_path(&source, "отрывок", "mp4");
        assert_eq!(second.file_name().unwrap().to_string_lossy(), "Запись (отрывок 2).mp4");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Needs a video in `KADR_TEST_VIDEO`; the results stay next to it for ffprobe.
    /// Run with `cargo test real_video -- --ignored --nocapture --test-threads=1`.
    #[test]
    #[ignore]
    fn real_video_is_trimmed_and_exported() {
        let source = PathBuf::from(std::env::var("KADR_TEST_VIDEO").expect("KADR_TEST_VIDEO"));
        let state = JobState::default();
        let cut = source.with_extension("cut.mp4");
        trim(&source, &cut, 1.0, 3.0, &state).expect("trim");
        for (name, q) in [("high.gif", GIF_HIGH), ("low.gif", GIF_LOW)] {
            let path = source.with_extension(name);
            export_gif(&source, &path, 1.0, 3.0, q, &state).expect("gif");
            eprintln!("{name}: {} KB", std::fs::metadata(&path).unwrap().len() / 1024);
        }
        eprintln!("cut: {} KB", std::fs::metadata(&cut).unwrap().len() / 1024);
    }

    #[test]
    #[ignore]
    fn cancelled_job_leaves_no_file() {
        let source = PathBuf::from(std::env::var("KADR_TEST_VIDEO").expect("KADR_TEST_VIDEO"));
        let state = JobState::default();
        state.cancel.store(true, Ordering::Relaxed);
        let cut = source.with_extension("cancelled.mp4");
        assert!(trim(&source, &cut, 0.0, 2.0, &state).is_err());
        assert!(!cut.exists());
    }
}
