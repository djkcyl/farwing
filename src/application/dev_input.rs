//! `dev input`: scripted keyboard and mouse input over a real controller
//! session, for verifying host-side injection (`docs/protocol/v2-design.md`
//! §7) without a viewer window.
//!
//! Keys are named by their physical position on a US layout and sent as the
//! HID usage of that position (the same scan-code table viewers use), so the
//! host's own layout and IME decide what they produce.
use crate::features::remote_input::{MouseMode, RemoteInput};
use crate::platform::windows::input::keyboard;
use crate::protocol::input::SystemCommand;
use anyhow::{Context as _, Result, anyhow, bail, ensure};
use std::time::Duration;

/// Input owner of the script (a viewer uses its window handle).
const OWNER: u64 = 0x6465_7669;
const LEFT_SHIFT: u16 = 0x2a;

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Key {
    pub name: String,
    pub usage: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Button {
    Left,
    Right,
    Middle,
    X1,
    X2,
}

impl Button {
    fn parse(name: &str) -> Result<Self> {
        Ok(match name {
            "left" => Self::Left,
            "right" => Self::Right,
            "middle" => Self::Middle,
            "x1" => Self::X1,
            "x2" => Self::X2,
            other => bail!("未知鼠标按钮 {other}（left/right/middle/x1/x2）"),
        })
    }
    /// Button mask of `MouseButton` (§7).
    fn mask(self) -> u32 {
        match self {
            Self::Left => 1,
            Self::Right => 2,
            Self::Middle => 16,
            Self::X1 => 32,
            Self::X2 => 64,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Step {
    /// US-layout text; `\n` is Enter and `\t` Tab.
    Text(String),
    /// Keys pressed in order and released in reverse.
    Chord(Vec<Key>),
    Down(Key),
    Up(Key),
    Repeat { key: Key, count: u32, interval: Duration },
    Lock([bool; 3]),
    Move(f64, f64),
    Rel(i32, i32),
    Click(Button, u32),
    Press(Button),
    Release(Button),
    Wheel { dy: i32, dx: i32 },
    System(SystemCommand),
    Sleep(Duration),
    Pull,
}

/// Set-1 scan code of a US-layout character and whether it needs Shift.
fn char_scan(c: char) -> Option<(u16, bool)> {
    const LETTERS: [u16; 26] = [
        0x1e, 0x30, 0x2e, 0x20, 0x12, 0x21, 0x22, 0x23, 0x17, 0x24, 0x25, 0x26, 0x32, 0x31, 0x18,
        0x19, 0x10, 0x13, 0x1f, 0x14, 0x16, 0x2f, 0x11, 0x2d, 0x15, 0x2c,
    ];
    const DIGITS: [u16; 10] = [0x0b, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a];
    Some(match c {
        'a'..='z' => (LETTERS[(c as u8 - b'a') as usize], false),
        'A'..='Z' => (LETTERS[(c as u8 - b'A') as usize], true),
        '0'..='9' => (DIGITS[(c as u8 - b'0') as usize], false),
        ')' => (0x0b, true),
        '!' => (0x02, true),
        '@' => (0x03, true),
        '#' => (0x04, true),
        '$' => (0x05, true),
        '%' => (0x06, true),
        '^' => (0x07, true),
        '&' => (0x08, true),
        '*' => (0x09, true),
        '(' => (0x0a, true),
        '-' => (0x0c, false),
        '_' => (0x0c, true),
        '=' => (0x0d, false),
        '+' => (0x0d, true),
        '[' => (0x1a, false),
        '{' => (0x1a, true),
        ']' => (0x1b, false),
        '}' => (0x1b, true),
        '\\' => (0x2b, false),
        '|' => (0x2b, true),
        ';' => (0x27, false),
        ':' => (0x27, true),
        '\'' => (0x28, false),
        '"' => (0x28, true),
        '`' => (0x29, false),
        '~' => (0x29, true),
        ',' => (0x33, false),
        '<' => (0x33, true),
        '.' => (0x34, false),
        '>' => (0x34, true),
        '/' => (0x35, false),
        '?' => (0x35, true),
        ' ' => (0x39, false),
        '\n' => (0x1c, false),
        '\t' => (0x0f, false),
        _ => return None,
    })
}

/// Set-1 scan code of a named key (`0xE0xx` for extended keys).
fn named_scan(name: &str) -> Option<u16> {
    Some(match name {
        "enter" | "return" => 0x1c,
        "esc" | "escape" => 0x01,
        "tab" => 0x0f,
        "backspace" => 0x0e,
        "space" => 0x39,
        "equal" => 0x0d,
        "capslock" => 0x3a,
        "numlock" => 0xe045,
        "scrolllock" => 0x46,
        "pause" => 0x45,
        "printscreen" => 0xe037,
        "insert" => 0xe052,
        "delete" | "del" => 0xe053,
        "home" => 0xe047,
        "end" => 0xe04f,
        "pageup" => 0xe049,
        "pagedown" => 0xe051,
        "up" => 0xe048,
        "down" => 0xe050,
        "left" => 0xe04b,
        "right" => 0xe04d,
        "apps" | "menu" => 0xe05d,
        "ctrl" | "lctrl" => 0x1d,
        "rctrl" => 0xe01d,
        "shift" | "lshift" => 0x2a,
        "rshift" => 0x36,
        "alt" | "lalt" => 0x38,
        "ralt" | "altgr" => 0xe038,
        "win" | "lwin" => 0xe05b,
        "rwin" => 0xe05c,
        "volumeup" => 0xe030,
        "volumedown" => 0xe02e,
        "mute" | "volumemute" => 0xe020,
        "mediaplaypause" | "playpause" => 0xe022,
        "mediastop" => 0xe024,
        "medianext" | "nexttrack" => 0xe019,
        "mediaprev" | "prevtrack" => 0xe010,
        "kpenter" => 0xe01c,
        "kpplus" => 0x4e,
        "kpminus" => 0x4a,
        "kpmultiply" => 0x37,
        "kpdivide" => 0xe035,
        "kpdecimal" => 0x53,
        "kp0" => 0x52,
        "kp1" => 0x4f,
        "kp2" => 0x50,
        "kp3" => 0x51,
        "kp4" => 0x4b,
        "kp5" => 0x4c,
        "kp6" => 0x4d,
        "kp7" => 0x47,
        "kp8" => 0x48,
        "kp9" => 0x49,
        _ => {
            let number = name.strip_prefix('f')?.parse::<u16>().ok()?;
            match number {
                1..=10 => 0x3a + number,
                11 => 0x57,
                12 => 0x58,
                _ => return None,
            }
        }
    })
}

fn usage_of(scan: u16) -> Result<u32> {
    keyboard::usage(scan).with_context(|| format!("扫描码 {scan:#06x} 没有对应的 HID usage"))
}

/// A key by name, or a single character key (Shift not included: `key a`
/// presses the A key; use `text A` or `key shift+a` for a capital).
fn parse_key(name: &str) -> Result<Key> {
    let lower = name.to_ascii_lowercase();
    let scan = named_scan(&lower)
        .or_else(|| {
            let mut chars = name.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) => char_scan(c.to_ascii_lowercase()).map(|(scan, _)| scan),
                _ => None,
            }
        })
        .with_context(|| format!("未知按键 {name}"))?;
    Ok(Key {
        name: lower,
        usage: usage_of(scan)?,
    })
}

/// The keys of one character of `text`: optional Shift, then the key.
pub(crate) fn char_keys(c: char) -> Result<(Option<u32>, u32)> {
    let (scan, shift) = char_scan(c).with_context(|| format!("无法按美式键位输入字符 {c:?}"))?;
    Ok((
        shift.then(|| usage_of(LEFT_SHIFT)).transpose()?,
        usage_of(scan)?,
    ))
}

fn unescape(text: &str) -> Result<String> {
    let mut out = String::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('\\') => out.push('\\'),
            other => bail!("无效转义 \\{}", other.map(String::from).unwrap_or_default()),
        }
    }
    Ok(out)
}

fn on_off(value: &str) -> Result<bool> {
    match value {
        "on" | "1" | "true" => Ok(true),
        "off" | "0" | "false" => Ok(false),
        other => bail!("锁定状态应为 on/off：{other}"),
    }
}

fn number<T: std::str::FromStr>(word: Option<&str>, what: &str) -> Result<T> {
    word.with_context(|| format!("缺少{what}"))?
        .parse()
        .map_err(|_| anyhow!("{what}无效"))
}

fn parse_line(line: &str, allow_disruptive: bool) -> Result<Option<Step>> {
    let line = line.trim_end_matches(['\r', '\n']);
    let trimmed = line.trim_start();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return Ok(None);
    }
    // `text` keeps its argument verbatim, including spaces.
    if let Some(text) = trimmed.strip_prefix("text ") {
        return Ok(Some(Step::Text(unescape(text)?)));
    }
    let mut words = trimmed.split_whitespace();
    let command = words.next().unwrap_or_default();
    let step = match command {
        "key" => {
            let chord = words.next().context("缺少按键")?;
            let keys = chord.split('+').map(parse_key).collect::<Result<Vec<_>>>()?;
            ensure!(!keys.is_empty(), "缺少按键");
            Step::Chord(keys)
        }
        "down" => Step::Down(parse_key(words.next().context("缺少按键")?)?),
        "up" => Step::Up(parse_key(words.next().context("缺少按键")?)?),
        "repeat" => Step::Repeat {
            key: parse_key(words.next().context("缺少按键")?)?,
            count: number(words.next(), "次数")?,
            interval: Duration::from_millis(number(words.next(), "间隔")?),
        },
        "lock" => {
            let mut state = [false; 3];
            for word in words.by_ref() {
                let (name, value) = word.split_once('=').context("锁定状态应写成 caps=on")?;
                let index = match name {
                    "caps" => 0,
                    "num" => 1,
                    "scroll" => 2,
                    other => bail!("未知锁定键 {other}"),
                };
                state[index] = on_off(value)?;
            }
            Step::Lock(state)
        }
        "move" => {
            let x: f64 = number(words.next(), "横坐标")?;
            let y: f64 = number(words.next(), "纵坐标")?;
            ensure!(
                (0.0..=1.0).contains(&x) && (0.0..=1.0).contains(&y),
                "坐标应在 0..1 之间"
            );
            Step::Move(x, y)
        }
        "rel" => Step::Rel(number(words.next(), "横向位移")?, number(words.next(), "纵向位移")?),
        "click" => {
            let button = Button::parse(words.next().unwrap_or("left"))?;
            let count = words.next().map(str::parse).transpose().map_err(|_| anyhow!("次数无效"))?;
            Step::Click(button, count.unwrap_or(1))
        }
        "press" => Step::Press(Button::parse(words.next().unwrap_or("left"))?),
        "release" => Step::Release(Button::parse(words.next().unwrap_or("left"))?),
        "wheel" => Step::Wheel {
            dy: number(words.next(), "滚动量")?,
            dx: words.next().map(str::parse).transpose().map_err(|_| anyhow!("横向滚动量无效"))?.unwrap_or(0),
        },
        "system" => {
            let command = match words.next().context("缺少系统命令")? {
                "taskmgr" => SystemCommand::TaskManager,
                "desktop" => SystemCommand::ShowDesktop,
                "taskview" => SystemCommand::TaskView,
                "sas" => SystemCommand::Sas,
                "lock" => SystemCommand::Lock,
                other => bail!("未知系统命令 {other}"),
            };
            ensure!(
                allow_disruptive || !matches!(command, SystemCommand::Sas | SystemCommand::Lock),
                "sas / lock 会锁定本机桌面，需要 --allow-disruptive"
            );
            Step::System(command)
        }
        "sleep" => Step::Sleep(Duration::from_millis(number(words.next(), "毫秒数")?)),
        "pull" => Step::Pull,
        other => bail!("未知命令 {other}"),
    };
    ensure!(words.next().is_none(), "多余的参数");
    Ok(Some(step))
}

/// Parse a script: one step per line, `#` comments; errors name the line.
pub(crate) fn parse_script(text: &str, allow_disruptive: bool) -> Result<Vec<Step>> {
    let mut steps = Vec::new();
    for (index, line) in text.lines().enumerate() {
        if let Some(step) =
            parse_line(line, allow_disruptive).with_context(|| format!("第 {} 行：{line}", index + 1))?
        {
            steps.push(step);
        }
    }
    Ok(steps)
}

impl std::fmt::Display for Step {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names = |keys: &[Key]| keys.iter().map(|k| k.name.as_str()).collect::<Vec<_>>().join("+");
        match self {
            Self::Text(text) => write!(f, "text {text:?}"),
            Self::Chord(keys) => write!(f, "key {}", names(keys)),
            Self::Down(key) => write!(f, "down {} ({:#x})", key.name, key.usage),
            Self::Up(key) => write!(f, "up {} ({:#x})", key.name, key.usage),
            Self::Repeat { key, count, interval } => {
                write!(f, "repeat {} ×{count} every {} ms", key.name, interval.as_millis())
            }
            Self::Lock([caps, num, scroll]) => write!(f, "lock caps={caps} num={num} scroll={scroll}"),
            Self::Move(x, y) => write!(f, "move {x:.4} {y:.4}"),
            Self::Rel(dx, dy) => write!(f, "rel {dx} {dy}"),
            Self::Click(button, count) => write!(f, "click {button:?} ×{count}"),
            Self::Press(button) => write!(f, "press {button:?}"),
            Self::Release(button) => write!(f, "release {button:?}"),
            Self::Wheel { dy, dx } => write!(f, "wheel dy={dy} dx={dx}"),
            Self::System(command) => write!(f, "system {command:?}"),
            Self::Sleep(duration) => write!(f, "sleep {} ms", duration.as_millis()),
            Self::Pull => write!(f, "pull"),
        }
    }
}

pub(crate) struct Options {
    pub screen: Option<i32>,
    pub hold: Duration,
}

/// Runs a parsed script on an established session.
pub(crate) struct Runner {
    input: RemoteInput,
    exchange: crate::features::clipboard::Clipboard,
    screen: i32,
    hold: Duration,
}

impl Runner {
    /// Wait for the input channel and the host's screens, enable control and
    /// check that the host granted it.
    pub(crate) async fn start(
        handle: crate::features::stream_control::StreamControlHandle,
        exchange: crate::features::clipboard::Clipboard,
        options: &Options,
    ) -> Result<Self> {
        let input = handle.mouse().clone();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        let mut last = None;
        loop {
            let snapshot = handle.snapshot();
            if snapshot.pb_connected && !snapshot.screens.is_empty() {
                // Absolute mouse, host cursor left alone (§7).
                match handle.set_mouse_mode(MouseMode::Local) {
                    Ok(()) => break,
                    Err(error) => last = Some(error),
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(last.unwrap_or_else(|| anyhow!("被控端没有提供显示器列表")))
                    .context("键鼠控制未能开启");
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        let grants = loop {
            if let Some(grants) = handle.granted() {
                break grants;
            }
            ensure!(
                tokio::time::Instant::now() < deadline,
                "被控端没有报告本会话的能力"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        println!("granted: {}", grants.join(","));
        ensure!(
            grants.iter().any(|cap| cap == farwing_protocol::caps::CONTROL),
            "被控端没有授予键鼠控制（control）"
        );
        // One participant controls at a time: wait for the control token.
        loop {
            if handle.participants().holds_control() {
                break;
            }
            if input.mode() == MouseMode::View {
                bail!(
                    "被控端没有交出控制权：{}",
                    handle
                        .participants()
                        .controlled_by_other()
                        .map(|m| crate::features::stream_control::control_turn::held_by(&m.nickname))
                        .unwrap_or_else(|| "请求被拒绝".into())
                );
            }
            ensure!(tokio::time::Instant::now() < deadline, "等待控制权超时");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        println!("control token held");
        // Nothing is held on this side: the activation may send at once.
        input.confirm_neutral(input.activation_generation());
        ensure!(!input.waiting_for_neutral(), "键鼠控制未能就绪");
        let snapshot = handle.snapshot();
        let screen = match options.screen {
            Some(id) => snapshot
                .screens
                .iter()
                .find(|s| s.id == id)
                .with_context(|| format!("被控端没有屏幕 {id}"))?,
            None => snapshot
                .screens
                .iter()
                .find(|s| s.primary)
                .or(snapshot.screens.first())
                .context("被控端没有屏幕")?,
        };
        println!(
            "screen: {} ({}×{}{})",
            screen.id,
            screen.width,
            screen.height,
            if screen.primary { ", primary" } else { "" }
        );
        Ok(Self {
            screen: screen.id,
            input,
            exchange,
            hold: options.hold,
        })
    }

    async fn flush(&self) -> Result<()> {
        ensure!(
            self.input.flushed(Duration::from_secs(3)).await,
            "输入未能在 3 秒内送出"
        );
        if let Some(error) = self.input.error() {
            bail!(error);
        }
        Ok(())
    }

    async fn key(&self, usage: u32, down: bool) -> Result<()> {
        ensure!(
            self.input.key(OWNER, usage, down),
            "按键 {usage:#x} {} 未被接受",
            if down { "按下" } else { "抬起" }
        );
        self.flush().await
    }

    async fn tap(&self, shift: Option<u32>, usage: u32) -> Result<()> {
        if let Some(shift) = shift {
            self.key(shift, true).await?;
        }
        self.key(usage, true).await?;
        tokio::time::sleep(self.hold).await;
        self.key(usage, false).await?;
        if let Some(shift) = shift {
            self.key(shift, false).await?;
        }
        tokio::time::sleep(self.hold).await;
        Ok(())
    }

    async fn button(&self, button: Button, down: bool) -> Result<()> {
        self.input.button(OWNER, button.mask(), down);
        self.flush().await
    }

    pub(crate) async fn run(&self, step: &Step) -> Result<()> {
        match step {
            Step::Text(text) => {
                for c in text.chars() {
                    let (shift, usage) = char_keys(c)?;
                    self.tap(shift, usage).await?;
                }
            }
            Step::Chord(keys) => {
                for key in keys {
                    self.key(key.usage, true).await?;
                }
                tokio::time::sleep(self.hold).await;
                for key in keys.iter().rev() {
                    self.key(key.usage, false).await?;
                }
                tokio::time::sleep(self.hold).await;
            }
            Step::Down(key) => self.key(key.usage, true).await?,
            Step::Up(key) => self.key(key.usage, false).await?,
            Step::Repeat { key, count, interval } => {
                self.key(key.usage, true).await?;
                for _ in 0..*count {
                    tokio::time::sleep(*interval).await;
                    // Down while already down is sent as an auto-repeat.
                    self.key(key.usage, true).await?;
                }
                self.key(key.usage, false).await?;
            }
            Step::Lock(state) => {
                self.input.sync_locks(OWNER, *state);
                self.flush().await?;
            }
            Step::Move(x, y) => {
                self.input.set_relative_mode(false);
                // Absolute positions are 0 <= v < 1 of the screen.
                let clamp = |v: f64| v.min(1.0 - 1e-6);
                self.input.absolute(OWNER, self.screen, clamp(*x), clamp(*y));
                self.flush().await?;
            }
            Step::Rel(dx, dy) => {
                self.input.set_relative_mode(true);
                self.input.relative(OWNER, *dx, *dy);
                let flushed = self.flush().await;
                self.input.set_relative_mode(false);
                flushed?;
            }
            Step::Click(button, count) => {
                for _ in 0..*count {
                    self.button(*button, true).await?;
                    tokio::time::sleep(self.hold).await;
                    self.button(*button, false).await?;
                    tokio::time::sleep(self.hold).await;
                }
            }
            Step::Press(button) => self.button(*button, true).await?,
            Step::Release(button) => self.button(*button, false).await?,
            Step::Wheel { dy, dx } => {
                self.input.wheel(OWNER, *dx, *dy);
                self.flush().await?;
            }
            Step::System(command) => {
                self.input.system_command(OWNER, *command)?;
                self.flush().await?;
            }
            Step::Sleep(duration) => tokio::time::sleep(*duration).await,
            Step::Pull => {
                let exchange = self.exchange.clone();
                let text = tokio::task::spawn_blocking(move || pull_text(&exchange))
                    .await
                    .context("获取剪贴板的线程异常结束")??;
                println!("clipboard: {text:?}");
            }
        }
        Ok(())
    }

    /// Release everything still held on the host and stop input.
    pub(crate) async fn finish(&self) {
        self.input.close().await;
    }
}

/// The host clipboard as text, through `Pull` (§8.2).
fn pull_text(exchange: &crate::features::clipboard::Clipboard) -> Result<String> {
    let (before, _) = exchange
        .memory_wait_offer(0, Duration::from_millis(1))
        .unwrap_or((0, Vec::new()));
    exchange.pull()?;
    let (offer, formats) = exchange.memory_wait_offer(before, Duration::from_secs(5))?;
    let format = formats
        .iter()
        .find(|format| format.id == 13)
        .cloned()
        .context("被控剪贴板没有文本")?;
    let bytes = exchange.memory_fetch(offer, &format)?;
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .take_while(|unit| *unit != 0)
        .collect();
    Ok(String::from_utf16_lossy(&units))
}

#[cfg(test)]
mod tests {
    use super::*;
    const K: u32 = keyboard::KEYBOARD;
    const C: u32 = keyboard::CONSUMER;

    #[test]
    fn text_maps_to_us_positions() {
        assert_eq!(char_keys('a').unwrap(), (None, K | 0x04));
        assert_eq!(char_keys('A').unwrap(), (Some(K | 0xe1), K | 0x04));
        assert_eq!(char_keys('z').unwrap(), (None, K | 0x1d));
        assert_eq!(char_keys('1').unwrap(), (None, K | 0x1e));
        assert_eq!(char_keys('0').unwrap(), (None, K | 0x27));
        assert_eq!(char_keys('!').unwrap(), (Some(K | 0xe1), K | 0x1e));
        assert_eq!(char_keys(')').unwrap(), (Some(K | 0xe1), K | 0x27));
        assert_eq!(char_keys('\n').unwrap(), (None, K | 0x28));
        assert_eq!(char_keys('\t').unwrap(), (None, K | 0x2b));
        assert_eq!(char_keys(' ').unwrap(), (None, K | 0x2c));
        assert_eq!(char_keys(';').unwrap(), (None, K | 0x33));
        assert_eq!(char_keys(':').unwrap(), (Some(K | 0xe1), K | 0x33));
        assert_eq!(char_keys('?').unwrap(), (Some(K | 0xe1), K | 0x38));
        assert_eq!(char_keys('"').unwrap(), (Some(K | 0xe1), K | 0x34));
        assert_eq!(char_keys('~').unwrap(), (Some(K | 0xe1), K | 0x35));
        assert_eq!(char_keys('|').unwrap(), (Some(K | 0xe1), K | 0x31));
        assert!(char_keys('é').is_err());
        // Every printable ASCII character has a key.
        for c in (b' '..=b'~').map(char::from) {
            assert!(char_keys(c).is_ok(), "{c:?}");
        }
    }

    #[test]
    fn script_parses_every_step() {
        let script = "\
# Notepad check
text Hello, World! 123\\n
key ctrl+a
key ctrl+shift+esc
down shift
up shift
repeat a 5 30
lock caps=on num=off
move 0.5 0.25
rel -10 5
click left 2
click right
press middle
release middle
wheel -120
wheel 0 240
system taskmgr
key volumeup
key f12
key kp7
sleep 100
pull
";
        let steps = parse_script(script, false).unwrap();
        assert_eq!(steps.len(), 21);
        assert_eq!(steps[0], Step::Text("Hello, World! 123\n".into()));
        let Step::Chord(keys) = &steps[2] else { panic!() };
        assert_eq!(
            keys.iter().map(|k| k.usage).collect::<Vec<_>>(),
            vec![K | 0xe0, K | 0xe1, K | 0x29]
        );
        assert!(matches!(&steps[5], Step::Repeat { key, count: 5, interval } if key.usage == K | 0x04 && *interval == Duration::from_millis(30)));
        assert_eq!(steps[6], Step::Lock([true, false, false]));
        assert_eq!(steps[7], Step::Move(0.5, 0.25));
        assert_eq!(steps[8], Step::Rel(-10, 5));
        assert_eq!(steps[9], Step::Click(Button::Left, 2));
        assert_eq!(steps[10], Step::Click(Button::Right, 1));
        assert_eq!(steps[13], Step::Wheel { dy: -120, dx: 0 });
        assert_eq!(steps[14], Step::Wheel { dy: 0, dx: 240 });
        assert_eq!(steps[15], Step::System(SystemCommand::TaskManager));
        let Step::Chord(keys) = &steps[16] else { panic!() };
        assert_eq!(keys[0].usage, C | 0xe9);
        let Step::Chord(keys) = &steps[17] else { panic!() };
        assert_eq!(keys[0].usage, K | 0x45);
        let Step::Chord(keys) = &steps[18] else { panic!() };
        assert_eq!(keys[0].usage, K | 0x5f);
        assert_eq!(steps[19], Step::Sleep(Duration::from_millis(100)));
        assert_eq!(steps[20], Step::Pull);
    }

    #[test]
    fn text_keeps_spaces_and_escapes() {
        let steps = parse_script("text   two  spaces\\tand\\\\slash", false).unwrap();
        assert_eq!(steps, vec![Step::Text("  two  spaces\tand\\slash".into())]);
        assert!(parse_script("text bad\\q", false).is_err());
    }

    #[test]
    fn errors_name_the_line_and_disruptive_commands_need_consent() {
        let error = parse_script("key ctrl+a\nfrobnicate\n", false).unwrap_err();
        assert!(format!("{error:#}").contains("第 2 行"));
        assert!(parse_script("system sas", false).is_err());
        assert!(parse_script("system lock", false).is_err());
        assert!(parse_script("system lock", true).is_ok());
        assert!(parse_script("key nosuchkey", false).is_err());
        assert!(parse_script("move 1.5 0", false).is_err());
        assert!(parse_script("lock caps=maybe", false).is_err());
        assert!(parse_script("click left 2 extra", false).is_err());
    }
}
