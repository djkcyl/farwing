use super::*;
use windows::{
    Win32::System::DataExchange::{GetClipboardFormatNameW, RegisterClipboardFormatW},
    core::PCWSTR,
};
#[derive(Clone)]
pub(super) struct Format {
    pub wire: wire::Format,
    pub local: u32,
}
pub(super) fn register(name: &str) -> u32 {
    let s: Vec<u16> = name.encode_utf16().chain([0]).collect();
    unsafe { RegisterClipboardFormatW(PCWSTR(s.as_ptr())) }
}
pub(super) fn name(id: u32) -> String {
    let mut s = [0u16; 1024];
    let n = unsafe { GetClipboardFormatNameW(id, &mut s) };
    String::from_utf16_lossy(&s[..n.max(0) as usize])
}
pub(super) fn file_format(id: u32, name: &str) -> bool {
    id == 15
        || [
            "FileGroupDescriptorW",
            "FileGroupDescriptor",
            "FileContents",
            "FileName",
            "FileNameW",
            "Shell IDList Array",
            "Preferred DropEffect",
            "DropDescription",
            "Shell Object Offsets",
            "DataObjectAttributes",
            "DataObjectAttributesRequiringElevation",
            "public.file-url",
        ]
        .iter()
        .any(|s| s.eq_ignore_ascii_case(name))
}
pub(super) fn supported(id: u32, name: &str, files: bool) -> bool {
    if name.eq_ignore_ascii_case("Farwing.Clipboard.Remote")
        || name.eq_ignore_ascii_case("DataObject")
        || name.eq_ignore_ascii_case("Ole Private Data")
    {
        return false;
    }
    if file_format(id, name) {
        return files
            && matches!(
                name,
                "FileGroupDescriptorW" | "FileContents" | "public.file-url"
            );
    }
    matches!(
        id,
        1 | 4 | 5 | 6 | 7 | 8 | 10 | 11 | 12 | 13 | 14 | 16 | 17 | 0x81 | 0x8e
    ) || (id >= 0xc000 && !name.is_empty())
}
pub(super) fn incoming(f: wire::Format, files: bool) -> Option<Format> {
    let local = if f.name.is_empty() { f.id } else { register(&f.name) };
    if local == 0 || !supported(local, &name(local), files) {
        return None;
    }
    Some(Format { wire: f, local })
}
pub(super) fn outgoing(ids: &[(u32, String)], files: bool) -> Vec<Format> {
    ids.iter()
        .filter(|(id, n)| supported(*id, n, files))
        .map(|(id, n)| Format {
            wire: wire::Format {
                id: *id,
                name: n.clone(),
            },
            local: *id,
        })
        .collect()
}
