//! The Windows "open file" dialog (several files), for chat attachments.
use std::path::PathBuf;

/// Ask for files on a thread of its own; the chosen paths (none when
/// cancelled) arrive on the returned channel.
pub(crate) fn pick_files(title: String) -> std::sync::mpsc::Receiver<Vec<PathBuf>> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(open(&title).unwrap_or_default());
    });
    rx
}

fn open(title: &str) -> windows::core::Result<Vec<PathBuf>> {
    use windows::Win32::System::Com::{
        CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
        CoUninitialize,
    };
    use windows::Win32::UI::Shell::{
        FOS_ALLOWMULTISELECT, FOS_FILEMUSTEXIST, FileOpenDialog, IFileOpenDialog, SIGDN_FILESYSPATH,
    };
    use windows::core::HSTRING;
    // SAFETY: COM is initialized for this thread for the dialog's lifetime;
    // every interface comes from the system and is released by its wrapper.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let result = (|| {
            let dialog: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER)?;
            dialog.SetOptions(dialog.GetOptions()? | FOS_ALLOWMULTISELECT | FOS_FILEMUSTEXIST)?;
            dialog.SetTitle(&HSTRING::from(title))?;
            if dialog.Show(None).is_err() {
                return Ok(Vec::new());
            }
            let items = dialog.GetResults()?;
            let mut out = Vec::new();
            for i in 0..items.GetCount()? {
                let item = items.GetItemAt(i)?;
                let name = item.GetDisplayName(SIGDN_FILESYSPATH)?;
                if let Ok(path) = name.to_string() {
                    out.push(PathBuf::from(path));
                }
                CoTaskMemFree(Some(name.0 as *const _));
            }
            Ok(out)
        })();
        CoUninitialize();
        result
    }
}

/// An image on the clipboard as PNG (copied pictures, screenshots).
pub(crate) fn clipboard_png() -> Option<Vec<u8>> {
    use crate::features::clipboard::system;
    // Browsers and many programs put a PNG; otherwise a DIB.
    if let Ok(png) = system::data(system::register("PNG"))
        && png.starts_with(b"\x89PNG")
    {
        return Some(png);
    }
    let dib = system::data(8).ok()?;
    if dib.len() < 40 {
        return None;
    }
    // A BMP file is a 14-byte header in front of the DIB.
    let header_size = u32::from_le_bytes([dib[0], dib[1], dib[2], dib[3]]);
    let bit_count = u16::from_le_bytes([dib[14], dib[15]]);
    let colors = u32::from_le_bytes([dib[32], dib[33], dib[34], dib[35]]);
    let compression = u32::from_le_bytes([dib[16], dib[17], dib[18], dib[19]]);
    let palette = if bit_count <= 8 { (if colors == 0 { 1 << bit_count } else { colors }) * 4 } else { 0 };
    let masks = if compression == 3 && header_size == 40 { 12 } else { 0 };
    let offset = 14 + header_size + palette + masks;
    let mut bmp = Vec::with_capacity(dib.len() + 14);
    bmp.extend_from_slice(b"BM");
    bmp.extend_from_slice(&((dib.len() + 14) as u32).to_le_bytes());
    bmp.extend_from_slice(&[0, 0, 0, 0]);
    bmp.extend_from_slice(&offset.to_le_bytes());
    bmp.extend_from_slice(&dib);
    let image = image::load_from_memory_with_format(&bmp, image::ImageFormat::Bmp).ok()?;
    let mut png = Vec::new();
    image.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).ok()?;
    Some(png)
}
