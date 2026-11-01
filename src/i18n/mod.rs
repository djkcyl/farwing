//! Interface text in 简体中文 and English (`docs/i18n.md`).
//!
//! Each area module holds one struct with a field per text and the two
//! translations side by side (`ZH_CN`, `EN`), so the compiler checks that
//! both languages have every text with the same arguments. Read the current
//! language's text with `t().area.field`; texts with arguments are functions:
//! `(t().area.field)(name, count)`. Logs and command-line output stay as
//! they are; only what people see in a window is translated.
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU8, Ordering};

pub(crate) mod app;
pub(crate) mod camera;
pub(crate) mod chat;
pub(crate) mod voice;
pub(crate) mod center;
pub(crate) mod common;
pub(crate) mod controls;
pub(crate) mod errors;
pub(crate) mod features;
pub(crate) mod notices;
pub(crate) mod records;
pub(crate) mod session;
pub(crate) mod settings;
pub(crate) mod system;
pub(crate) mod viewer;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Language {
    #[serde(rename = "zh-CN")]
    ZhCn,
    #[serde(rename = "en")]
    En,
}

impl Language {
    pub(crate) const ALL: [Self; 2] = [Self::ZhCn, Self::En];

    /// The BCP 47 tag, as other processes receive it.
    pub(crate) fn tag(self) -> &'static str {
        match self {
            Self::ZhCn => "zh-CN",
            Self::En => "en",
        }
    }

    pub(crate) fn from_tag(tag: &str) -> Option<Self> {
        let tag = tag.trim().to_ascii_lowercase();
        if tag == "zh" || tag.starts_with("zh-") {
            Some(Self::ZhCn)
        } else if tag == "en" || tag.starts_with("en-") {
            Some(Self::En)
        } else {
            None
        }
    }

    /// The language's own name, the same in every interface language.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::ZhCn => "简体中文",
            Self::En => "English",
        }
    }
}

/// What the user chose in Settings; `System` follows Windows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Choice {
    #[default]
    System,
    #[serde(rename = "zh-CN")]
    ZhCn,
    #[serde(rename = "en")]
    En,
}

impl Choice {
    pub(crate) fn language(self) -> Language {
        match self {
            Self::System => system_language(),
            Self::ZhCn => Language::ZhCn,
            Self::En => Language::En,
        }
    }
}

static CURRENT: AtomicU8 = AtomicU8::new(0);

pub(crate) fn language() -> Language {
    match CURRENT.load(Ordering::Relaxed) {
        0 => Language::ZhCn,
        _ => Language::En,
    }
}

/// Switch this process's interface language. Windows repaint with it;
/// texts already composed (a message on screen) keep their language.
pub(crate) fn set(language: Language) {
    CURRENT.store(
        match language {
            Language::ZhCn => 0,
            Language::En => 1,
        },
        Ordering::Relaxed,
    );
}

/// Windows' display language: Chinese reads 简体中文, anything else English.
pub(crate) fn system_language() -> Language {
    // SAFETY: no arguments; returns the user's UI language ID.
    let id = unsafe { windows::Win32::Globalization::GetUserDefaultUILanguage() };
    const LANG_CHINESE: u16 = 0x04;
    if id & 0x3ff == LANG_CHINESE {
        Language::ZhCn
    } else {
        Language::En
    }
}

#[derive(Default, Serialize, Deserialize)]
struct Saved {
    #[serde(default)]
    choice: Choice,
}

fn saved_path() -> Option<std::path::PathBuf> {
    crate::product::local_data_dir()
        .ok()
        .map(|dir| dir.join("language.json"))
}

/// The language chosen in Settings.
pub(crate) fn choice() -> Choice {
    saved_path()
        .and_then(|path| std::fs::read(path).ok())
        .and_then(|bytes| serde_json::from_slice::<Saved>(&bytes).ok())
        .unwrap_or_default()
        .choice
}

/// Save the choice and switch to it.
pub(crate) fn choose(choice: Choice) -> anyhow::Result<()> {
    let path = saved_path().ok_or_else(|| anyhow::anyhow!("no settings directory"))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, serde_json::to_vec(&Saved { choice })?)?;
    set(choice.language());
    Ok(())
}

/// At process start: the saved choice, or Windows' language.
pub fn init() {
    set(choice().language());
}

pub(crate) struct Messages {
    pub app: app::App,
    pub camera: camera::Camera,
    pub chat: chat::Chat,
    pub voice: voice::Voice,
    pub center: center::Center,
    pub common: common::Common,
    pub controls: controls::Controls,
    pub errors: errors::Errors,
    pub features: features::Features,
    pub records: records::Records,
    pub notices: notices::Notices,
    pub session: session::Session,
    pub settings: settings::Settings,
    pub system: system::System,
    pub viewer: viewer::Viewer,
}

static ZH_CN: Messages = Messages {
    app: app::ZH_CN,
    camera: camera::ZH_CN,
    chat: chat::ZH_CN,
    voice: voice::ZH_CN,
    center: center::ZH_CN,
    common: common::ZH_CN,
    controls: controls::ZH_CN,
    errors: errors::ZH_CN,
    features: features::ZH_CN,
    records: records::ZH_CN,
    notices: notices::ZH_CN,
    session: session::ZH_CN,
    settings: settings::ZH_CN,
    system: system::ZH_CN,
    viewer: viewer::ZH_CN,
};

static EN: Messages = Messages {
    app: app::EN,
    camera: camera::EN,
    chat: chat::EN,
    voice: voice::EN,
    center: center::EN,
    common: common::EN,
    controls: controls::EN,
    errors: errors::EN,
    features: features::EN,
    records: records::EN,
    notices: notices::EN,
    session: session::EN,
    settings: settings::EN,
    system: system::EN,
    viewer: viewer::EN,
};

/// The interface text in the current language.
pub(crate) fn t() -> &'static Messages {
    match language() {
        Language::ZhCn => &ZH_CN,
        Language::En => &EN,
    }
}

/// The interface text in a given language (tests, other processes).
pub(crate) fn messages(language: Language) -> &'static Messages {
    match language {
        Language::ZhCn => &ZH_CN,
        Language::En => &EN,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_round_trip() {
        for language in Language::ALL {
            assert_eq!(Language::from_tag(language.tag()), Some(language));
        }
        assert_eq!(Language::from_tag("zh-TW"), Some(Language::ZhCn));
        assert_eq!(Language::from_tag("en-GB"), Some(Language::En));
        assert_eq!(Language::from_tag("fr"), None);
    }

    #[test]
    fn both_languages_have_arguments_in_place() {
        for language in Language::ALL {
            let text = (messages(language).session.host_enable_failed)("x");
            assert!(text.contains('x'), "{text}");
        }
    }
}
