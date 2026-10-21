//! User-owned file browser roots. Called only by the ordinary-user file worker.
use crate::i18n::t;
use std::path::PathBuf;
use windows::Win32::{System::Com::CoTaskMemFree, UI::Shell::*};

pub(crate) fn known_folders() -> Vec<(PathBuf, &'static str, &'static str)> {
    let names = &t().system;
    [
        (FOLDERID_Documents, names.location_documents, "document"),
        (FOLDERID_Desktop, names.location_desktop, "desktop"),
        (FOLDERID_Pictures, names.location_pictures, "picture"),
        (FOLDERID_Videos, names.location_videos, "video"),
        (FOLDERID_Music, names.location_music, "music"),
        (FOLDERID_Downloads, names.location_downloads, "download"),
    ]
    .into_iter()
    .filter_map(|(id, name, icon)| unsafe {
        // Resolve redirected/OneDrive folders, never synthesize UserProfile suffixes.
        // DONT_VERIFY avoids contacting an unavailable network redirection here;
        // the file backend only exposes existing local directories.
        let raw = SHGetKnownFolderPath(&id, KF_FLAG_DONT_VERIFY, None).ok()?;
        let path = raw.to_string();
        CoTaskMemFree(Some(raw.0.cast()));
        let path = PathBuf::from(path.ok()?);
        let text = path.to_str()?;
        (text.as_bytes().get(1) == Some(&b':') && path.is_dir()).then_some((path, name, icon))
    })
    .collect()
}
