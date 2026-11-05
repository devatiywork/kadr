//! Puts images on the clipboard, as a bitmap and as PNG for apps that prefer it, and files
//! the way Explorer does, so that they can be pasted into a folder or a chat.

use crate::image::Image;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::DataExchange::*;
use windows::Win32::System::Memory::*;
use windows::Win32::System::Ole::{CF_DIB, CF_HDROP, CF_UNICODETEXT, DROPEFFECT_COPY};
use windows::Win32::UI::Shell::DROPFILES;
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

/// The format of a file list: a header, then each path as UTF-16 with its terminator, and one
/// more terminator after the last path.
fn file_list_bytes(path: &Path) -> Vec<u8> {
    let header = DROPFILES { pFiles: size_of::<DROPFILES>() as u32, fWide: true.into(), ..Default::default() };
    let header = unsafe { std::slice::from_raw_parts(&header as *const _ as *const u8, size_of::<DROPFILES>()) };
    let mut bytes = header.to_vec();
    for unit in path.as_os_str().encode_wide().chain([0, 0]) {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    bytes
}

pub fn set_file(path: &Path) -> Result<()> {
    let list = global_copy(&[&file_list_bytes(path)])?;
    // Without this some programs treat the paste as a move of the original file.
    let effect = global_copy(&[&DROPEFFECT_COPY.0.to_le_bytes()]).ok();
    unsafe {
        if !open_clipboard() {
            let _ = GlobalFree(Some(list));
            if let Some(e) = effect {
                let _ = GlobalFree(Some(e));
            }
            return Err(Error::from_thread());
        }
        let _ = EmptyClipboard();
        // After SetClipboardData succeeds the system owns the memory.
        let placed = SetClipboardData(CF_HDROP.0 as u32, Some(HANDLE(list.0)));
        if placed.is_err() {
            let _ = GlobalFree(Some(list));
        }
        if let Some(e) = effect {
            if SetClipboardData(RegisterClipboardFormatW(w!("Preferred DropEffect")), Some(HANDLE(e.0))).is_err() {
                let _ = GlobalFree(Some(e));
            }
        }
        let _ = CloseClipboard();
        placed.map(|_| ())
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_list_is_a_wide_path_with_a_double_terminator() {
        let path = r"C:\Videos\Запись экрана.mp4";
        let bytes = file_list_bytes(Path::new(path));
        let header = size_of::<DROPFILES>();
        let number = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
        assert_eq!(number(0), header as u32, "where the paths begin");
        assert_eq!(number(header - 4), 1, "the paths are wide");
        let units: Vec<u16> = bytes[header..].chunks_exact(2).map(|b| u16::from_le_bytes([b[0], b[1]])).collect();
        let expected: Vec<u16> = path.encode_utf16().chain([0, 0]).collect();
        assert_eq!(units, expected);
    }
}
