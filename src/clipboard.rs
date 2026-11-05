//! Puts images on the clipboard, as a bitmap and as PNG for apps that prefer it.

use crate::image::Image;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::DataExchange::*;
use windows::Win32::System::Memory::*;
use windows::Win32::System::Ole::{CF_DIB, CF_UNICODETEXT};
use windows::core::*;

fn global_copy(parts: &[&[u8]]) -> Result<HGLOBAL> {
    let len: usize = parts.iter().map(|p| p.len()).sum();
    unsafe {
        let mem = GlobalAlloc(GMEM_MOVEABLE, len)?;
        let dst = GlobalLock(mem) as *mut u8;
        let mut at = 0;
        for p in parts {
            std::ptr::copy_nonoverlapping(p.as_ptr(), dst.add(at), p.len());
            at += p.len();
        }
        let _ = GlobalUnlock(mem);
        Ok(mem)
    }
}

/// Bottom-up 32-bit DIB: the layout every application understands.
fn dib_bytes(img: &Image) -> (BITMAPINFOHEADER, Vec<u8>) {
    let header = BITMAPINFOHEADER {
        biSize: size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: img.w as i32,
        biHeight: img.h as i32,
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB.0,
        biSizeImage: img.w * img.h * 4,
        ..Default::default()
    };
    let stride = img.w as usize * 4;
    let mut pixels = Vec::with_capacity(img.bgra.len());
    for row in img.bgra.chunks_exact(stride).rev() {
        pixels.extend_from_slice(row);
    }
    (header, pixels)
}

/// Another program may hold the clipboard for a moment.
fn open_clipboard() -> bool {
    for _ in 0..20 {
        if unsafe { OpenClipboard(None) }.is_ok() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    false
}

pub fn set_text(text: &str) -> Result<()> {
    let utf16: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let bytes = unsafe { std::slice::from_raw_parts(utf16.as_ptr() as *const u8, utf16.len() * 2) };
    let memory = global_copy(&[bytes])?;
    unsafe {
        if !open_clipboard() {
            let _ = GlobalFree(Some(memory));
            return Err(Error::from_thread());
        }
        let _ = EmptyClipboard();
        // After SetClipboardData succeeds the system owns the memory.
        let _ = SetClipboardData(CF_UNICODETEXT.0 as u32, Some(HANDLE(memory.0)));
        let _ = CloseClipboard();
    }
    Ok(())
}

pub fn set_image(img: &Image) -> Result<()> {
    let (header, pixels) = dib_bytes(img);
    let header_bytes = unsafe {
        std::slice::from_raw_parts(&header as *const _ as *const u8, size_of::<BITMAPINFOHEADER>())
    };
    let dib = global_copy(&[header_bytes, &pixels])?;
    let png = img.to_png().ok().and_then(|bytes| global_copy(&[&bytes]).ok());

    unsafe {
        if !open_clipboard() {
            let _ = GlobalFree(Some(dib));
            if let Some(p) = png {
                let _ = GlobalFree(Some(p));
            }
            return Err(Error::from_thread());
        }
        let _ = EmptyClipboard();
        // After SetClipboardData succeeds the system owns the memory.
        let _ = SetClipboardData(CF_DIB.0 as u32, Some(HANDLE(dib.0)));
        if let Some(p) = png {
            let _ = SetClipboardData(RegisterClipboardFormatW(w!("PNG")), Some(HANDLE(p.0)));
        }
        let _ = CloseClipboard();
    }
    Ok(())
}
