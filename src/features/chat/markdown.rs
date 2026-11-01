//! The Markdown subset of chat messages (`docs/protocol/chat-design.md`
//! §4.1), parsed the same way as the web console
//! (`server/web/src/lib/chat/markdown.ts`): **bold**, *italic* / _italic_,
//! ~~strike~~, `code`, fenced blocks, `> ` quotes, ||spoilers||, links
//! (written and bare, http and https only), backslash escapes, mentions.

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Inline {
    Text(String),
    Code(String),
    Bold(Vec<Inline>),
    Italic(Vec<Inline>),
    Strike(Vec<Inline>),
    Spoiler(Vec<Inline>),
    Link { href: String, children: Vec<Inline> },
    User(String),
    Role(String),
    Channel(String),
    Everyone,
    Here,
    Break,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Block {
    Paragraph(Vec<Inline>),
    Quote(Vec<Inline>),
    Code { lang: String, text: String },
}

const ESCAPABLE: &str = "\\*_~|`[]()<>#";

fn is_word(c: Option<char>) -> bool {
    c.is_some_and(char::is_alphanumeric)
}

/// Index (in chars) of the next unescaped `delim` at or after `from`.
fn closing(src: &[char], delim: &[char], from: usize) -> Option<usize> {
    let mut i = from;
    while i < src.len() {
        if src[i] == '\\' {
            i += 2;
            continue;
        }
        if delim != ['`'] && src[i] == '`'
            && let Some(end) = (i + 1..src.len()).find(|&j| src[j] == '`')
        {
            i = end + 1;
            continue;
        }
        if src[i..].starts_with(delim) {
            return Some(i);
        }
        i += 1;
    }
    None
}

pub(crate) fn safe_href(href: &str) -> Option<String> {
    let h = href.trim();
    let lower = h.to_ascii_lowercase();
    ((lower.starts_with("https://") || lower.starts_with("http://"))
        && h.len() > 8
        && !h.chars().any(char::is_whitespace))
    .then(|| h.to_owned())
}

fn bare_link_end(src: &[char], start: usize) -> usize {
    let mut end = start;
    while end < src.len() && !src[end].is_whitespace() && src[end] != '<' {
        end += 1;
    }
    while end > start {
        let c = src[end - 1];
        if ".,;:!?'\"".contains(c) {
            end -= 1;
            continue;
        }
        if c == ')' {
            let text = &src[start..end];
            let open = text.iter().filter(|c| **c == '(').count();
            let close = text.iter().filter(|c| **c == ')').count();
            if close > open {
                end -= 1;
                continue;
            }
        }
        break;
    }
    end
}

fn is_uuid(s: &str) -> bool {
    farwing_protocol::ids::is_uuid(s)
}

pub(crate) fn parse_inline(text: &str) -> Vec<Inline> {
    let src: Vec<char> = text.chars().collect();
    inline(&src)
}

fn inline(src: &[char]) -> Vec<Inline> {
    let mut out = Vec::new();
    let mut text = String::new();
    let flush = |text: &mut String, out: &mut Vec<Inline>| {
        if !text.is_empty() {
            out.push(Inline::Text(std::mem::take(text)));
        }
    };
    let wrap = |delim: &[char], i: usize| -> Option<(usize, Vec<Inline>)> {
        let end = closing(src, delim, i + delim.len())?;
        let inner = &src[i + delim.len()..end];
        if inner.iter().all(|c| c.is_whitespace()) {
            return None;
        }
        Some((end + delim.len(), inline(inner)))
    };
    let mut i = 0;
    while i < src.len() {
        let c = src[i];
        if c == '\\' && i + 1 < src.len() && ESCAPABLE.contains(src[i + 1]) {
            text.push(src[i + 1]);
            i += 2;
            continue;
        }
        if c == '\n' {
            flush(&mut text, &mut out);
            out.push(Inline::Break);
            i += 1;
            continue;
        }
        if c == '`'
            && let Some(end) = (i + 1..src.len()).find(|&j| src[j] == '`')
            && end > i + 1
        {
            flush(&mut text, &mut out);
            out.push(Inline::Code(src[i + 1..end].iter().collect()));
            i = end + 1;
            continue;
        }
        let pairs: [(&[char], fn(Vec<Inline>) -> Inline); 3] = [
            (&['*', '*'], Inline::Bold),
            (&['~', '~'], Inline::Strike),
            (&['|', '|'], Inline::Spoiler),
        ];
        let mut matched = false;
        for (delim, make) in pairs {
            if src[i..].starts_with(delim)
                && let Some((next, children)) = wrap(delim, i)
            {
                flush(&mut text, &mut out);
                out.push(make(children));
                i = next;
                matched = true;
                break;
            }
        }
        if matched {
            continue;
        }
        if (c == '*' || (c == '_' && !is_word(i.checked_sub(1).map(|j| src[j]))))
            && src.get(i + 1).is_some_and(|n| *n != c && *n != ' ')
            && let Some(end) = closing(src, &[c], i + 1)
            && end > i + 1
            && src[end - 1] != ' '
            && (c != '_' || !is_word(src.get(end + 1).copied()))
        {
            flush(&mut text, &mut out);
            out.push(Inline::Italic(inline(&src[i + 1..end])));
            i = end + 1;
            continue;
        }
        if c == '['
            && let Some(close) = closing(src, &[']'], i + 1)
            && close > i + 1
            && src.get(close + 1) == Some(&'(')
            && let Some(end) = (close + 2..src.len()).find(|&j| src[j] == ')')
            && let Some(href) = safe_href(&src[close + 2..end].iter().collect::<String>())
        {
            flush(&mut text, &mut out);
            out.push(Inline::Link {
                href,
                children: inline(&src[i + 1..close]),
            });
            i = end + 1;
            continue;
        }
        if c == '<'
            && let Some(end) = (i + 1..src.len()).find(|&j| src[j] == '>')
        {
            let inner: String = src[i + 1..end].iter().collect();
            let node = if inner == "@everyone" {
                Some(Inline::Everyone)
            } else if inner == "@here" {
                Some(Inline::Here)
            } else if let Some(role) = inner.strip_prefix("@&").filter(|r| !r.is_empty() && r.bytes().all(|b| b.is_ascii_digit())) {
                Some(Inline::Role(role.to_owned()))
            } else if let Some(ch) = inner.strip_prefix('#').filter(|r| !r.is_empty() && r.bytes().all(|b| b.is_ascii_digit())) {
                Some(Inline::Channel(ch.to_owned()))
            } else {
                inner.strip_prefix('@').filter(|u| is_uuid(u)).map(|u| Inline::User(u.to_owned()))
            };
            if let Some(node) = node {
                flush(&mut text, &mut out);
                out.push(node);
                i = end + 1;
                continue;
            }
        }
        let rest: String = src[i..src.len().min(i + 8)].iter().collect();
        if (rest.starts_with("https://") || rest.starts_with("http://"))
            && !is_word(i.checked_sub(1).map(|j| src[j]))
        {
            let end = bare_link_end(src, i);
            if let Some(href) = safe_href(&src[i..end].iter().collect::<String>()) {
                flush(&mut text, &mut out);
                out.push(Inline::Link {
                    children: vec![Inline::Text(href.clone())],
                    href,
                });
                i = end;
                continue;
            }
        }
        text.push(c);
        i += 1;
    }
    flush(&mut text, &mut out);
    out
}

/// Blocks: fenced code, quotes (`> ` lines) and the rest.
pub(crate) fn parse(src: &str) -> Vec<Block> {
    let lines: Vec<&str> = src.split('\n').collect();
    let mut blocks = Vec::new();
    let mut para: Vec<&str> = Vec::new();
    let mut quote: Vec<&str> = Vec::new();
    let flush_para = |para: &mut Vec<&str>, blocks: &mut Vec<Block>| {
        if !para.is_empty() {
            blocks.push(Block::Paragraph(parse_inline(&para.join("\n"))));
            para.clear();
        }
    };
    let flush_quote = |quote: &mut Vec<&str>, blocks: &mut Vec<Block>| {
        if !quote.is_empty() {
            blocks.push(Block::Quote(parse_inline(&quote.join("\n"))));
            quote.clear();
        }
    };
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if let Some(rest) = line.strip_prefix("```") {
            let body = rest.trim_end();
            if rest.chars().count() > 3 && body.ends_with("```") {
                flush_para(&mut para, &mut blocks);
                flush_quote(&mut quote, &mut blocks);
                blocks.push(Block::Code {
                    lang: String::new(),
                    text: body[..body.len() - 3].to_owned(),
                });
                i += 1;
                continue;
            }
            if let Some(end) = (i + 1..lines.len()).find(|&j| lines[j].trim_end().ends_with("```")) {
                flush_para(&mut para, &mut blocks);
                flush_quote(&mut quote, &mut blocks);
                let trimmed = rest.trim();
                let lang = if !trimmed.is_empty()
                    && trimmed.chars().count() <= 20
                    && trimmed.chars().all(|c| c.is_alphanumeric() || "_+#.-".contains(c))
                {
                    trimmed.to_owned()
                } else {
                    String::new()
                };
                let mut text: Vec<&str> = Vec::new();
                if lang.is_empty() && !trimmed.is_empty() {
                    text.push(rest);
                }
                text.extend(&lines[i + 1..end]);
                let closing_line = lines[end].trim_end();
                if closing_line != "```" {
                    text.push(&closing_line[..closing_line.len() - 3]);
                }
                blocks.push(Block::Code {
                    lang,
                    text: text.join("\n"),
                });
                i = end + 1;
                continue;
            }
        }
        if let Some(q) = line.strip_prefix("> ").or_else(|| (line == ">").then_some("")) {
            flush_para(&mut para, &mut blocks);
            quote.push(q);
            i += 1;
            continue;
        }
        flush_quote(&mut quote, &mut blocks);
        para.push(line);
        i += 1;
    }
    flush_para(&mut para, &mut blocks);
    flush_quote(&mut quote, &mut blocks);
    blocks.retain(|b| !matches!(b, Block::Paragraph(c) if c.is_empty()));
    blocks
}

/// Names for mentions in plain text.
pub(crate) trait Names {
    fn user(&self, id: &str) -> String;
    fn role(&self, id: &str) -> String;
    fn channel(&self, id: &str) -> String;
}

/// Plain text of inline nodes (previews, notifications).
pub(crate) fn plain(nodes: &[Inline], names: &dyn Names) -> String {
    let mut out = String::new();
    for n in nodes {
        match n {
            Inline::Text(t) | Inline::Code(t) => out.push_str(t),
            Inline::Break => out.push(' '),
            Inline::User(id) => out.push_str(&format!("@{}", names.user(id))),
            Inline::Role(id) => out.push_str(&format!("@{}", names.role(id))),
            Inline::Channel(id) => out.push_str(&format!("#{}", names.channel(id))),
            Inline::Everyone => out.push_str("@everyone"),
            Inline::Here => out.push_str("@here"),
            Inline::Spoiler(_) => out.push_str("▒▒▒"),
            Inline::Bold(c) | Inline::Italic(c) | Inline::Strike(c) => out.push_str(&plain(c, names)),
            Inline::Link { children, .. } => out.push_str(&plain(children, names)),
        }
    }
    out
}

/// A whole message as plain text.
pub(crate) fn plain_message(content: &str, names: &dyn Names) -> String {
    parse(content)
        .iter()
        .map(|b| match b {
            Block::Code { text, .. } => text.clone(),
            Block::Paragraph(c) | Block::Quote(c) => plain(c, names),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use Inline::*;

    fn t(s: &str) -> Inline {
        Text(s.to_owned())
    }

    #[test]
    fn inline_subset() {
        assert_eq!(
            parse_inline("**bold** and *it* _it_ ~~gone~~ `x*y` ||secret||"),
            vec![
                Bold(vec![t("bold")]),
                t(" and "),
                Italic(vec![t("it")]),
                t(" "),
                Italic(vec![t("it")]),
                t(" "),
                Strike(vec![t("gone")]),
                t(" "),
                Code("x*y".into()),
                t(" "),
                Spoiler(vec![t("secret")]),
            ]
        );
    }

    #[test]
    fn nesting_and_escapes() {
        assert_eq!(
            parse_inline("**a *b* c**"),
            vec![Bold(vec![t("a "), Italic(vec![t("b")]), t(" c")])]
        );
        assert_eq!(
            parse_inline("\\*not\\* 2*3 snake_case_name"),
            vec![t("*not* 2*3 snake_case_name")]
        );
        assert_eq!(parse_inline("** **"), vec![t("** **")]);
    }

    #[test]
    fn links_are_http_only() {
        assert_eq!(
            parse_inline("[site](https://example.com/a) [bad](javascript:alert(1))"),
            vec![
                Link {
                    href: "https://example.com/a".into(),
                    children: vec![t("site")]
                },
                t(" [bad](javascript:alert(1))"),
            ]
        );
        assert_eq!(
            parse_inline("see https://example.com/x_(y). ok"),
            vec![
                t("see "),
                Link {
                    href: "https://example.com/x_(y)".into(),
                    children: vec![t("https://example.com/x_(y)")]
                },
                t(". ok"),
            ]
        );
    }

    #[test]
    fn mentions() {
        let u = "0f8fad5b-d9cb-469f-a165-70867728950e";
        assert_eq!(
            parse_inline(&format!("<@{u}> <@&42> <@everyone> <@here> <#7> <@nobody>")),
            vec![
                User(u.into()),
                t(" "),
                Role("42".into()),
                t(" "),
                Everyone,
                t(" "),
                Here,
                t(" "),
                Channel("7".into()),
                t(" <@nobody>"),
            ]
        );
    }

    #[test]
    fn blocks() {
        assert_eq!(
            parse("hi\n```rust\nfn main() {}\n**x**\n```\n> quoted *a*\n> more\nafter"),
            vec![
                Block::Paragraph(vec![t("hi")]),
                Block::Code {
                    lang: "rust".into(),
                    text: "fn main() {}\n**x**".into()
                },
                Block::Quote(vec![t("quoted "), Italic(vec![t("a")]), Break, t("more")]),
                Block::Paragraph(vec![t("after")]),
            ]
        );
        assert_eq!(
            parse("```one line```"),
            vec![Block::Code {
                lang: String::new(),
                text: "one line".into()
            }]
        );
        assert_eq!(
            parse("```not closed\ntext"),
            vec![Block::Paragraph(vec![t("```not closed"), Break, t("text")])]
        );
    }
}
