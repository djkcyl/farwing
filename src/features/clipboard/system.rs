//! `dev clipboard`: read and write the Windows clipboard of this process
//! directly (no clipboard listener, no sync), so an in-memory controller
//! exchange can be checked against a real host end to end: what the host
//! renders from our offers, and what it offers from the system clipboard.
use super::*;
use std::path::PathBuf;
use windows::{
    Win32::{
        Foundation::*,
        System::{Com::*, DataExchange::*, Memory::*, Ole::*},
        UI::WindowsAndMessaging::*,
    },
    core::w,
};

/// Run `work` on a fresh OLE thread.
fn on_ole_thread<T: Send + 'static>(
    work: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    std::thread::spawn(move || {
        unsafe { OleInitialize(None) }?;
        let result = work();
        unsafe { OleUninitialize() };
        result
    })
    .join()
    .map_err(|_| anyhow!("剪贴板测试线程异常结束"))?
}

pub(crate) fn register(name: &str) -> u32 {
    super::formats::register(name)
}

fn format_etc(id: u32, index: i32, tymed: TYMED) -> FORMATETC {
    FORMATETC {
        cfFormat: id as u16,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: index,
        tymed: tymed.0 as u32,
    }
}

fn global(handle: HGLOBAL) -> Result<Vec<u8>> {
    unsafe {
        let size = GlobalSize(handle);
        let pointer = GlobalLock(handle);
        ensure!(!pointer.is_null() || size == 0, "无法锁定剪贴板内容");
        let bytes = std::slice::from_raw_parts(pointer.cast::<u8>(), size).to_vec();
        let _ = GlobalUnlock(handle);
        Ok(bytes)
    }
}

/// The current system clipboard's data of format `id` (HGLOBAL). The size
/// may be rounded up by the allocator.
pub(crate) fn data(id: u32) -> Result<Vec<u8>> {
    on_ole_thread(move || unsafe {
        let object = OleGetClipboard()?;
        let mut medium = object.GetData(&format_etc(id, -1, TYMED_HGLOBAL))?;
        let bytes = global(medium.u.hGlobal);
        ReleaseStgMedium(&mut medium);
        bytes
    })
}

/// The file list (`FileGroupDescriptorW`) and file contents (`FileContents`
/// streams) of the current system clipboard.
pub(crate) fn files() -> Result<Vec<(FileItem, Vec<u8>)>> {
    on_ole_thread(move || unsafe {
        let object = OleGetClipboard()?;
        let mut medium = object.GetData(&format_etc(
            register("FileGroupDescriptorW"),
            -1,
            TYMED_HGLOBAL,
        ))?;
        let descriptor = global(medium.u.hGlobal);
        ReleaseStgMedium(&mut medium);
        let items = super::native::parse_descriptors(&descriptor?)?;
        let mut out = Vec::new();
        for (index, item) in items.into_iter().enumerate() {
            if item.attributes & 0x10 != 0 {
                out.push((item, Vec::new()));
                continue;
            }
            let mut medium = object.GetData(&format_etc(
                register("FileContents"),
                index as i32,
                TYMED(TYMED_ISTREAM.0 | TYMED_HGLOBAL.0),
            ))?;
            let contents = if medium.tymed == TYMED_ISTREAM.0 as u32 {
                let stream = (*medium.u.pstm).clone().ok_or_else(|| anyhow!("没有文件流"))?;
                let mut contents = Vec::with_capacity(item.size as usize);
                let mut buffer = vec![0u8; 4 * 1024 * 1024];
                loop {
                    let mut read = 0;
                    stream
                        .Read(buffer.as_mut_ptr().cast(), buffer.len() as u32, Some(&mut read))
                        .ok()?;
                    if read == 0 {
                        break;
                    }
                    contents.extend_from_slice(&buffer[..read as usize]);
                }
                contents
            } else {
                global(medium.u.hGlobal)?
            };
            ReleaseStgMedium(&mut medium);
            out.push((item, contents));
        }
        Ok(out)
    })
}

fn allocate(bytes: &[u8]) -> Result<HGLOBAL> {
    unsafe {
        let handle = GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1))?;
        let pointer = GlobalLock(handle);
        ensure!(!pointer.is_null(), "无法分配剪贴板内存");
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), pointer.cast(), bytes.len());
        let _ = GlobalUnlock(handle);
        Ok(handle)
    }
}

/// Replace the system clipboard with `contents` (format ID, bytes) and, if
/// any, a file list (`CF_HDROP` of `files`).
pub(crate) fn set(contents: Vec<(u32, Vec<u8>)>, files: Vec<PathBuf>) -> Result<()> {
    on_ole_thread(move || unsafe {
        let window = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("STATIC"),
            w!(""),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            None,
            None,
        )?;
        let mut opened = false;
        for _ in 0..50 {
            if OpenClipboard(Some(window)).is_ok() {
                opened = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        ensure!(opened, "剪贴板正在被其他程序占用");
        let result = (|| -> Result<()> {
            EmptyClipboard()?;
            for (id, bytes) in &contents {
                let handle = allocate(bytes)?;
                if SetClipboardData(*id, Some(HANDLE(handle.0))).is_err() {
                    let _ = GlobalFree(Some(handle));
                    bail!("写入剪贴板格式 {id} 失败");
                }
            }
            if !files.is_empty() {
                // DROPFILES { pFiles, pt, fNC, fWide } + a double-NUL list.
                let mut bytes = Vec::new();
                bytes.extend_from_slice(&20u32.to_le_bytes());
                bytes.extend_from_slice(&[0; 12]);
                bytes.extend_from_slice(&1u32.to_le_bytes());
                for path in &files {
                    for unit in path.as_os_str().to_string_lossy().encode_utf16() {
                        bytes.extend_from_slice(&unit.to_le_bytes());
                    }
                    bytes.extend_from_slice(&[0, 0]);
                }
                bytes.extend_from_slice(&[0, 0]);
                let handle = allocate(&bytes)?;
                if SetClipboardData(15, Some(HANDLE(handle.0))).is_err() {
                    let _ = GlobalFree(Some(handle));
                    bail!("写入文件列表失败");
                }
            }
            Ok(())
        })();
        let _ = CloseClipboard();
        let _ = DestroyWindow(window);
        result
    })
}
