//! Verified sender geometry rules, independent of capture/renderer APIs.
/// Local presentation only. Never feeds capture settings or physical modes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum DisplayAspect {
    #[default]
    Video,
    Display,
    Wide,
    WideTen,
    Standard,
    UltraWide,
}
impl DisplayAspect {
    pub const ALL: [Self; 6] = [
        Self::Video,
        Self::Display,
        Self::Wide,
        Self::WideTen,
        Self::Standard,
        Self::UltraWide,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Self::Video => "保持画面比例",
            Self::Display => "跟随目标显示器比例",
            Self::Wide => "拉伸至 16:9",
            Self::WideTen => "拉伸至 16:10",
            Self::Standard => "拉伸至 4:3",
            Self::UltraWide => "拉伸至 21:9",
        }
    }
    pub fn ratio(self, preferred: Option<(u32, u32)>) -> Option<(u32, u32)> {
        match self {
            Self::Video => None,
            Self::Display => preferred.filter(|(w, h)| *w > 0 && *h > 0),
            Self::Wide => Some((16, 9)),
            Self::WideTen => Some((16, 10)),
            Self::Standard => Some((4, 3)),
            Self::UltraWide => Some((21, 9)),
        }
    }
    #[cfg_attr(
        not(windows),
        allow(
            dead_code,
            reason = "Only the Windows viewer resizes its window to one video pixel per screen pixel."
        )
    )]
    pub fn size(video: (u32, u32), ratio: Option<(u32, u32)>) -> (u32, u32) {
        match ratio {
            Some((w, h)) if w > 0 && h > 0 => (
                ((u64::from(video.1) * u64::from(w) + u64::from(h) / 2) / u64::from(h))
                    .clamp(1, u64::from(u32::MAX)) as u32,
                video.1,
            ),
            _ => video,
        }
    }
}

pub(crate) fn dimensions(quality: i32) -> (u32, u32) {
    match quality {
        1 => (1280, 720),
        3 => (2560, 1440),
        4 | 6 => (3840, 2160),
        _ => (1920, 1080),
    }
}

pub(crate) fn output_size(width: u32, height: u32, quality: i32) -> (u32, u32) {
    fit_size(width, height, dimensions(quality))
}

pub(crate) fn fit_size(width: u32, height: u32, maximum: (u32, u32)) -> (u32, u32) {
    if width <= maximum.0 && height <= maximum.1 {
        return ((width & !1).max(2), (height & !1).max(2));
    }
    // T CCE2C0/CD3A70: single-precision min scale, then 2*round(x/2).
    // The bounds keep their axes on portrait sources; they are not swapped.
    let scale = (maximum.0 as f32 / width as f32)
        .min(maximum.1 as f32 / height as f32)
        .min(1.0);
    (
        ((width as f32 * scale * 0.5).round() as u32 * 2).max(2),
        ((height as f32 * scale * 0.5).round() as u32 * 2).max(2),
    )
}
