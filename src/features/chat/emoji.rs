//! Emoji in chat text (`docs/protocol/chat-design.md` §4.1, §12.3): text
//! cut into graphemes, the emoji among them named by their images on the
//! server (`/emoji/<code points>.png`, `fe0f` left out), and the picker's
//! index searched in either language.
use unicode_segmentation::UnicodeSegmentation;

/// Whether `c` starts an emoji (Extended_Pictographic, regional indicators).
fn pictographic(c: char) -> bool {
    matches!(c as u32,
        0x00A9 | 0x00AE | 0x203C | 0x2049 | 0x2122 | 0x2139
        | 0x2194..=0x2199 | 0x21A9..=0x21AA | 0x231A..=0x231B | 0x2328 | 0x23CF
        | 0x23E9..=0x23F3 | 0x23F8..=0x23FA | 0x24C2 | 0x25AA..=0x25AB | 0x25B6 | 0x25C0
        | 0x25FB..=0x25FE | 0x2600..=0x27BF | 0x2934..=0x2935 | 0x2B05..=0x2B07
        | 0x2B1B..=0x2B1C | 0x2B50 | 0x2B55 | 0x3030 | 0x303D | 0x3297 | 0x3299
        | 0x1F000..=0x1FAFF)
}

pub(crate) fn is_emoji(grapheme: &str) -> bool {
    let mut chars = grapheme.chars();
    match chars.next() {
        Some(c @ ('#' | '*' | '0'..='9')) => {
            let rest: Vec<char> = chars.collect();
            let _ = c;
            rest == ['\u{FE0F}', '\u{20E3}'] || rest == ['\u{20E3}']
        }
        Some(c) => pictographic(c),
        None => false,
    }
}

/// The image name of an emoji grapheme: code points in hex, `-` joined.
pub(crate) fn key(grapheme: &str) -> String {
    grapheme
        .chars()
        .filter(|c| *c != '\u{FE0F}')
        .map(|c| format!("{:x}", c as u32))
        .collect::<Vec<_>>()
        .join("-")
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Piece<'a> {
    Text(&'a str),
    Emoji { text: &'a str, key: String },
}

/// Text cut into runs of plain text and single emoji.
pub(crate) fn pieces(text: &str) -> Vec<Piece<'_>> {
    let mut out = Vec::new();
    let mut start = None;
    for (at, g) in text.grapheme_indices(true) {
        if is_emoji(g) {
            if let Some(s) = start.take() {
                out.push(Piece::Text(&text[s..at]));
            }
            out.push(Piece::Emoji { text: g, key: key(g) });
        } else if start.is_none() {
            start = Some(at);
        }
    }
    if let Some(s) = start {
        out.push(Piece::Text(&text[s..]));
    }
    out
}

/// Only emoji (whitespace aside), at most 27: shown large.
pub(crate) fn only_emoji(text: &str) -> bool {
    let mut count = 0;
    for g in text.graphemes(true) {
        if g.trim().is_empty() {
            continue;
        }
        if !is_emoji(g) {
            return false;
        }
        count += 1;
        if count > 27 {
            return false;
        }
    }
    count > 0
}

/// One entry of the picker's index (`/emoji/index.json`).
#[derive(Clone, Debug, Default, serde::Deserialize)]
#[serde(default)]
pub(crate) struct Entry {
    /// The emoji itself.
    pub e: String,
    /// Its image name.
    pub c: String,
    /// Unicode's group.
    pub g: String,
    pub n: String,
    pub k: Vec<String>,
    pub zn: String,
    pub zk: Vec<String>,
    /// Has skin tones.
    pub t: bool,
}

/// Entries whose names or keywords contain `query`, best first.
pub(crate) fn search<'a>(entries: &'a [Entry], query: &str, limit: usize) -> Vec<&'a Entry> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return entries.iter().take(limit).collect();
    }
    let mut scored: Vec<(u8, &Entry)> = entries
        .iter()
        .filter_map(|e| {
            let names = [e.n.to_lowercase(), e.zn.to_lowercase()];
            let words: Vec<String> = e.k.iter().chain(&e.zk).map(|w| w.to_lowercase()).collect();
            let score = if names.iter().any(|n| *n == q) {
                0
            } else if names.iter().any(|n| n.starts_with(&q)) {
                1
            } else if words.iter().any(|w| w.starts_with(&q)) {
                2
            } else if names.iter().any(|n| n.contains(&q)) || words.iter().any(|w| w.contains(&q)) {
                3
            } else {
                return None;
            };
            Some((score, e))
        })
        .collect();
    scored.sort_by_key(|(s, _)| *s);
    scored.into_iter().take(limit).map(|(_, e)| e).collect()
}

/// Skin tone modifiers, lightest first (1–5; 0 is none).
pub(crate) const TONES: [&str; 6] = ["", "\u{1F3FB}", "\u{1F3FC}", "\u{1F3FD}", "\u{1F3FE}", "\u{1F3FF}"];

pub(crate) fn with_tone(e: &Entry, tone: usize) -> String {
    if !e.t || tone == 0 || tone >= TONES.len() {
        return e.e.clone();
    }
    let chars: Vec<char> = e.e.chars().filter(|c| *c != '\u{FE0F}').collect();
    let mut out = String::new();
    out.push(chars[0]);
    out.push_str(TONES[tone]);
    out.extend(&chars[1..]);
    out
}

/// Unicode's groups, in the picker's order, with an emoji for each tab.
pub(crate) const GROUPS: [(&str, &str); 9] = [
    ("Smileys & Emotion", "😀"),
    ("People & Body", "👋"),
    ("Animals & Nature", "🐱"),
    ("Food & Drink", "🍎"),
    ("Travel & Places", "🚗"),
    ("Activities", "⚽"),
    ("Objects", "💡"),
    ("Symbols", "❤\u{FE0F}"),
    ("Flags", "🏁"),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_is_cut_into_emoji_and_the_rest() {
        let family = "👨\u{200d}👩\u{200d}👧";
        let text = format!("hi 👍🏽 {family}!🇨🇳");
        let keys: Vec<String> = pieces(&text)
            .into_iter()
            .map(|p| match p {
                Piece::Text(t) => t.to_owned(),
                Piece::Emoji { key, .. } => key,
            })
            .collect();
        assert_eq!(
            keys,
            ["hi ", "1f44d-1f3fd", " ", "1f468-200d-1f469-200d-1f467", "!", "1f1e8-1f1f3"]
        );
        assert_eq!(key("❤\u{FE0F}"), "2764");
        assert_eq!(key("#\u{FE0F}\u{20E3}"), "23-20e3");
        assert!(is_emoji("#\u{FE0F}\u{20E3}"));
        assert!(!is_emoji("#"));
    }

    #[test]
    fn messages_of_only_emoji() {
        assert!(only_emoji("😀 😀"));
        assert!(!only_emoji("😀 a"));
        assert!(!only_emoji("   "));
        assert!(!only_emoji(&"😀".repeat(28)));
    }

    #[test]
    fn search_and_skin_tones() {
        let entries = vec![
            Entry { e: "😀".into(), c: "1f600".into(), n: "grinning face".into(), k: vec!["grin".into()], zn: "嘿嘿".into(), zk: vec!["笑脸".into()], ..Default::default() },
            Entry { e: "👍".into(), c: "1f44d".into(), n: "thumbs up".into(), k: vec!["+1".into()], zn: "拇指向上".into(), zk: vec!["赞".into()], t: true, ..Default::default() },
        ];
        assert_eq!(search(&entries, "thumb", 5)[0].c, "1f44d");
        assert_eq!(search(&entries, "笑", 5)[0].c, "1f600");
        assert_eq!(with_tone(&entries[1], 3), "👍🏽");
        assert_eq!(with_tone(&entries[0], 3), "😀");
    }
}
