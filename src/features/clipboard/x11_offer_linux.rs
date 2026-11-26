//! Owning the X11 clipboard for what this adapter publishes.
//!
//! 这个会话里 X11 与 Wayland 的选择区是两座互不相通的孤岛（niri 这类合成器不桥接，
//! 实测 `xclip` 写进去的东西 `wl-paste` 看不见，反过来也一样），而 arboard 只落在
//! Wayland 那一侧，所以想让 XWayland 的程序粘得到，就得自己去占 X11 的选择区。
//!
//! 一份复制往往不是一个载荷而是同一件事的好几种说法，读哪一种看程序：GTK 的文件管理
//! 器认 `x-special/gnome-copied-files`（它还带「是复制还是剪切」），别的多半读
//! `text/uri-list`，再老一点的直接读路径文本；文本那边现代程序问 `UTF8_STRING`，只认
//! `STRING` 的老程序也还有。只有选择区的持有者能替一种以上的类型作答，而 arboard 一次
//! 只挂一个名字，所以这里把选择区整个接过来。
//!
//! Ownership passes back the moment anything else claims the clipboard, including
//! this process publishing something else: the owner thread is told, and stops.

use anyhow::{Context as _, Result, bail};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use x11rb::connection::{Connection, RequestConnection as _};
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ChangeWindowAttributesAux, ConnectionExt as _, CreateWindowAux, EventMask,
    PropMode, Property, SELECTION_NOTIFY_EVENT, SelectionNotifyEvent, SelectionRequestEvent,
    Window, WindowClass,
};
use x11rb::wrapper::ConnectionExt as _;

/// How long the owner thread waits for an event before looking at whether it
/// has been asked to stop.
const POLL: Duration = Duration::from_millis(50);

/// 对方多久不来取下一段就放弃这次分段传输。每送出一段重新算起。
const INCR_WAIT: Duration = Duration::from_secs(5);

/// `ChangeProperty` 请求除数据以外的字节：24 字节的请求头，再加上 BIG-REQUESTS
/// 把长度字段挪出来的 4 字节。
const PROPERTY_HEADER: usize = 28;

/// The targets a file copy offers, most specific first, which is the order
/// `TARGETS` should list them in.
const FILE_TARGETS: [&str; 6] = [
    "x-special/gnome-copied-files",
    "text/uri-list",
    "UTF8_STRING",
    "text/plain;charset=utf-8",
    "STRING",
    "TEXT",
];

/// 文本那几种。GTK 与 Qt 问的是 `UTF8_STRING`（或者带字符集的 `text/plain`），
/// XTerm 和老的 Java 程序还只认 `STRING` / `TEXT`，那两个按 ICCCM 是 ISO-8859-1。
const TEXT_TARGETS: [&str; 5] = [
    "UTF8_STRING",
    "text/plain;charset=utf-8",
    "text/plain",
    "STRING",
    "TEXT",
];

/// 我们放在选择区上的东西。
enum Content {
    Files(Vec<std::path::PathBuf>),
    Text(String),
}

impl Content {
    /// 对外提供的类型，最具体的排前面。
    fn targets(&self) -> &'static [&'static str] {
        match self {
            Self::Files(_) => &FILE_TARGETS,
            Self::Text(_) => &TEXT_TARGETS,
        }
    }

    /// 回应 `target` 时属性的类型。
    ///
    /// 按 ICCCM，`TEXT` 是「由持有者挑一种编码」，回应里写的是挑中的那种：文本给的是
    /// Latin-1（见 `payload`），也就是 `STRING`；文件那份的路径文本是 UTF-8。其余类型
    /// 问什么就是什么。
    fn kind(&self, target: &'static str) -> &'static str {
        match (self, target) {
            (Self::Text(_), "TEXT") => "STRING",
            (Self::Files(_), "TEXT") => "UTF8_STRING",
            _ => target,
        }
    }

    /// 某个类型上要给的字节。
    fn payload(&self, target: &str) -> Vec<u8> {
        match self {
            Self::Files(paths) => files_payload(target, paths),
            // `STRING` / `TEXT` 是 ISO-8859-1，其余按 UTF-8。落在 Latin-1 之外的
            // 字符换个 `?`：只认这两个类型的程序本来也显示不出它们，而拿 UTF-8 去
            // 冒充 Latin-1 会把整段文字变成乱码。
            Self::Text(text) => match target {
                "STRING" | "TEXT" => text
                    .chars()
                    .map(|c| if (c as u32) <= 0xff { c as u8 } else { b'?' })
                    .collect(),
                _ => text.as_bytes().to_vec(),
            },
        }
    }
}

/// Percent-encode a path for a `file:` URI.
///
/// Everything outside the unreserved set is escaped, which is stricter than
/// what file managers write -- they leave brackets and parentheses alone -- but
/// anything reading a URI has to decode the escapes anyway.
fn encode(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt as _;
    let mut out = String::from("file://");
    for byte in path.as_os_str().as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                out.push(*byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// The bytes each target carries for `paths`.
fn files_payload(target: &str, paths: &[std::path::PathBuf]) -> Vec<u8> {
    let uris: Vec<String> = paths.iter().map(|path| encode(path)).collect();
    match target {
        // The first line is the operation; a paste that reads this one is
        // deciding between copying and moving.
        "x-special/gnome-copied-files" => format!("copy\n{}", uris.join("\n")).into_bytes(),
        // RFC 2483 delimits this with CRLF, including after the last entry.
        "text/uri-list" => uris
            .iter()
            .map(|uri| format!("{uri}\r\n"))
            .collect::<String>()
            .into_bytes(),
        // Plain text gets the paths themselves: a URI pasted into a terminal
        // or a text box is not what anyone wanted.
        _ => paths
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("\n")
            .into_bytes(),
    }
}

struct Atoms {
    clipboard: Atom,
    targets: Atom,
    timestamp: Atom,
    incr: Atom,
    /// One per entry in the content's target list, in the same order.
    data: Vec<Atom>,
}

fn intern(connection: &impl Connection, name: &str) -> Result<Atom> {
    Ok(connection
        .intern_atom(false, name.as_bytes())?
        .reply()
        .with_context(|| format!("intern X11 atom {name}"))?
        .atom)
}

/// A copy published on the clipboard, owned until this is dropped or something
/// else takes the selection.
pub(super) struct Offer {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Offer {
    /// Take the clipboard and answer for `paths` until told otherwise.
    pub(super) fn publish_files(paths: Vec<std::path::PathBuf>) -> Result<Self> {
        if paths.is_empty() {
            bail!("没有要发布的文件");
        }
        Self::publish(Content::Files(paths))
    }

    /// 同上，内容是这段文本。
    pub(super) fn publish_text(text: String) -> Result<Self> {
        Self::publish(Content::Text(text))
    }

    fn publish(content: Content) -> Result<Self> {
        let (connection, screen) = x11rb::connect(None).context("连接 X11 显示失败")?;
        let root = connection.setup().roots[screen].root;
        let window = connection.generate_id().context("分配 X11 窗口失败")?;
        connection
            .create_window(
                x11rb::COPY_DEPTH_FROM_PARENT,
                window,
                root,
                0,
                0,
                1,
                1,
                0,
                WindowClass::INPUT_ONLY,
                x11rb::COPY_FROM_PARENT,
                &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
            )?
            .check()
            .context("创建 X11 剪贴板窗口失败")?;

        let atoms = Atoms {
            clipboard: intern(&connection, "CLIPBOARD")?,
            targets: intern(&connection, "TARGETS")?,
            timestamp: intern(&connection, "TIMESTAMP")?,
            incr: intern(&connection, "INCR")?,
            data: content
                .targets()
                .iter()
                .map(|name| intern(&connection, name))
                .collect::<Result<_>>()?,
        };

        // ICCCM asks for a real timestamp rather than CurrentTime, and the way
        // to get one is to provoke an event that carries it.
        let marker = intern(&connection, "OPENUUYC_TIMESTAMP")?;
        connection.change_property8(PropMode::APPEND, window, marker, AtomEnum::STRING, &[])?;
        connection.flush()?;
        let mut time = x11rb::CURRENT_TIME;
        for _ in 0..100 {
            match connection.poll_for_event()? {
                Some(Event::PropertyNotify(event)) if event.window == window => {
                    time = event.time;
                    break;
                }
                Some(_) => {}
                None => std::thread::sleep(Duration::from_millis(2)),
            }
        }

        connection
            .set_selection_owner(window, atoms.clipboard, time)?
            .check()
            .context("取得剪贴板所有权失败")?;
        connection.flush()?;
        if connection
            .get_selection_owner(atoms.clipboard)?
            .reply()?
            .owner
            != window
        {
            bail!("剪贴板所有权被其他程序抢占");
        }

        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let thread = std::thread::Builder::new()
            .name("UU clipboard owner".into())
            .spawn(move || serve(&connection, window, &atoms, &content, &worker_stop))
            .context("启动剪贴板所有权线程失败")?;
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for Offer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// 一次分段传输（ICCCM 2.7.2 的 INCR）。
///
/// 数据大过 X 服务器单个请求的上限时，一次 `ChangeProperty` 写不下：先在对方的属性
/// 上放一个 `INCR` 类型、值为总长度的记号，对方每删一次属性，就往里写下一段，最后
/// 写一段空的表示完了。
struct Transfer {
    requestor: Window,
    property: Atom,
    /// 每一段属性的类型，也就是这份数据实际的类型。
    kind: Atom,
    data: Vec<u8>,
    /// 已经写出去的字节数。
    sent: usize,
    /// 到这一刻对方还没来取下一段，就放弃。
    deadline: Instant,
}

/// Answer selection requests until ownership is lost or the offer is retired.
fn serve(
    connection: &x11rb::rust_connection::RustConnection,
    window: x11rb::protocol::xproto::Window,
    atoms: &Atoms,
    content: &Content,
    stop: &AtomicBool,
) {
    // 一个请求能带多少字节由 X 服务器说了算（有 BIG-REQUESTS 时 Xorg 是 16 MB，没有
    // 时只有 256 KB），远端发来的文本最大 32 MB，超过的只能分段交出去。
    let chunk = connection
        .maximum_request_bytes()
        .saturating_sub(PROPERTY_HEADER);
    let mut transfers: Vec<Transfer> = Vec::new();
    // 选择区被别人拿走以后不再接新的请求，但已经开始的分段传输要送完：对方正等着
    // 下一段，半路停下它就只能干等到自己超时（`xclip` 根本没有超时）。offer 被撤下
    // （`Drop`）时不在此列：剪贴板线程正等着这里退出。
    let mut owned = true;
    while !stop.load(Ordering::Acquire) {
        let now = Instant::now();
        let expired: Vec<Window> = transfers
            .iter()
            .filter(|transfer| transfer.deadline <= now)
            .map(|transfer| transfer.requestor)
            .collect();
        if !expired.is_empty() {
            tracing::debug!(
                count = expired.len(),
                "对方迟迟不取下一段，放弃剪贴板分段传输"
            );
            transfers.retain(|transfer| transfer.deadline > now);
            for requestor in expired {
                unwatch(connection, &transfers, requestor);
            }
        }
        if !owned && transfers.is_empty() {
            return;
        }
        let event = match connection.poll_for_event() {
            Ok(Some(event)) => event,
            Ok(None) => {
                wait(connection);
                continue;
            }
            Err(error) => {
                tracing::debug!(%error, "剪贴板所有权连接中断");
                return;
            }
        };
        match event {
            // Something else took the clipboard, which retires this offer.
            Event::SelectionClear(event) if event.owner == window => owned = false,
            Event::SelectionRequest(request) if owned => {
                answer(connection, atoms, content, &request, chunk, &mut transfers);
            }
            // 选择区已经不是我们的了，还在路上的请求一律拒绝。
            Event::SelectionRequest(request) => notify(connection, &request, x11rb::NONE),
            // 对方的窗口没了（程序退出、粘贴被取消），它那几次传输不用再等。
            Event::DestroyNotify(event) => {
                transfers.retain(|transfer| transfer.requestor != event.window);
            }
            // 对方删掉了属性，也就是取走了上一段，接着写下一段。
            Event::PropertyNotify(event) if event.state == Property::DELETE => {
                let Some(index) = transfers.iter().position(|transfer| {
                    transfer.requestor == event.window && transfer.property == event.atom
                }) else {
                    continue;
                };
                match next_chunk(connection, &mut transfers[index], chunk) {
                    Ok(true) => {}
                    Ok(false) => {
                        transfers.swap_remove(index);
                        unwatch(connection, &transfers, event.window);
                    }
                    Err(error) => {
                        tracing::debug!(error = format!("{error:#}"), "剪贴板分段传输失败");
                        transfers.swap_remove(index);
                        unwatch(connection, &transfers, event.window);
                    }
                }
            }
            _ => {}
        }
    }
    // 收尾时把手里的选择区交还出去——但只在它真的还是我们的时候。这个 offer 被
    // 换掉（用户又复制了一次，新的 offer 已经抢走了所有权）或者本地用户在别的
    // 程序里复制了东西之后，选择区早就易主；这时再置空就是替别人把剪贴板清掉，
    // 刚上任的那一份也跟着没了。
    let ours = connection
        .get_selection_owner(atoms.clipboard)
        .ok()
        .and_then(|cookie| cookie.reply().ok())
        .is_some_and(|reply| reply.owner == window);
    if ours {
        let _ = connection.set_selection_owner(x11rb::NONE, atoms.clipboard, x11rb::CURRENT_TIME);
        let _ = connection.flush();
    }
}

/// 等 X 服务器发来点什么，最多等 `POLL`。
///
/// 不能干睡：请求在队列里多躺几十毫秒，分段传输的每一段就多等几十毫秒；而请求方
/// 退出后 X 会把它的窗口号分给下一个程序，晚到的旧请求就会答到新程序的窗口上。
fn wait(connection: &x11rb::rust_connection::RustConnection) {
    use std::os::fd::AsRawFd as _;
    let mut poll = libc::pollfd {
        fd: connection.stream().as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // Safe because `poll` only writes into the one descriptor it was given.
    // 出错（包括被信号打断）和到点一样处理：回去看一眼停没停、再取事件。
    let _ = unsafe { libc::poll(&mut poll, 1, POLL.as_millis() as i32) };
}

/// 回应一次请求。写属性出了错也照样回话（`property = None`，也就是拒绝）：不回话的
/// 话，对方会一直等下去。
fn answer(
    connection: &impl Connection,
    atoms: &Atoms,
    content: &Content,
    request: &SelectionRequestEvent,
    chunk: usize,
    transfers: &mut Vec<Transfer>,
) {
    // A requestor that sent no property is using the obsolete convention; the
    // target doubles as the property in that case.
    let property = if request.property == x11rb::NONE {
        request.target
    } else {
        request.property
    };
    let accepted = match reply(
        connection, atoms, content, request, property, chunk, transfers,
    ) {
        Ok(accepted) => accepted,
        Err(error) => {
            tracing::debug!(error = format!("{error:#}"), "写剪贴板属性失败，按拒绝回应");
            false
        }
    };
    notify(
        connection,
        request,
        if accepted { property } else { x11rb::NONE },
    );
}

/// 把被问的那一份写到对方的属性上，返回「给了没有」。不支持的类型返回 `false`。
///
/// 每一笔都等 X 服务器确认（`check`）：属性没写成却回话说写好了，对方读到的是空的。
fn reply(
    connection: &impl Connection,
    atoms: &Atoms,
    content: &Content,
    request: &SelectionRequestEvent,
    property: Atom,
    chunk: usize,
    transfers: &mut Vec<Transfer>,
) -> Result<bool> {
    let requestor = request.requestor;
    // 同一个窗口、同一个属性又来要了：上一次分段传输对方已经不要了。X 也会把退出的
    // 程序用过的窗口号分给新程序，留着它会把旧数据写进新请求的属性里。
    transfers.retain(|transfer| transfer.requestor != requestor || transfer.property != property);
    if request.target == atoms.targets {
        let mut list = vec![atoms.targets, atoms.timestamp];
        list.extend_from_slice(&atoms.data);
        connection
            .change_property32(
                PropMode::REPLACE,
                requestor,
                property,
                AtomEnum::ATOM,
                &list,
            )?
            .check()?;
    } else if request.target == atoms.timestamp {
        connection
            .change_property32(
                PropMode::REPLACE,
                requestor,
                property,
                AtomEnum::INTEGER,
                &[request.time],
            )?
            .check()?;
    } else if let Some(index) = atoms.data.iter().position(|atom| *atom == request.target) {
        let target = content.targets()[index];
        let data = content.payload(target);
        // 两份清单里都有 `STRING` 和 `UTF8_STRING`，找不到只会是清单改坏了，那就照旧
        // 写被问的类型。
        let kind = content
            .targets()
            .iter()
            .position(|name| *name == content.kind(target))
            .map_or(request.target, |kind| atoms.data[kind]);
        if data.len() <= chunk {
            connection
                .change_property8(PropMode::REPLACE, requestor, property, kind, &data)?
                .check()?;
        } else {
            // 一次写不下，分段送。先盯住对方窗口上的属性变化，再放 `INCR` 记号、回话：
            // 顺序反过来的话，对方删记号那一下可能在我们开始盯之前就发生了。窗口销毁
            // 也一起盯，对方中途退出时好及早放手。
            connection
                .change_window_attributes(
                    requestor,
                    &ChangeWindowAttributesAux::new()
                        .event_mask(EventMask::PROPERTY_CHANGE | EventMask::STRUCTURE_NOTIFY),
                )?
                .check()?;
            // 值是总长度的下限；超过 32 位就写最大值，反正 `MAX_CLIP` 远小于它。
            let size = u32::try_from(data.len()).unwrap_or(u32::MAX);
            connection
                .change_property32(PropMode::REPLACE, requestor, property, atoms.incr, &[size])?
                .check()?;
            tracing::debug!(
                size = data.len(),
                chunk,
                "剪贴板内容超过单个请求上限，分段传输"
            );
            transfers.push(Transfer {
                requestor,
                property,
                kind,
                data,
                sent: 0,
                deadline: Instant::now() + INCR_WAIT,
            });
        }
    } else {
        // Refusing is reported by naming no property, which is what tells the
        // requestor to try a different target.
        return Ok(false);
    }
    Ok(true)
}

/// 对方取走了上一段，写下一段。返回「还有没有下文」：数据写完之后还要再写一段空的，
/// 那一段写出去这次传输才算完。
fn next_chunk(connection: &impl Connection, transfer: &mut Transfer, chunk: usize) -> Result<bool> {
    let end = (transfer.sent + chunk).min(transfer.data.len());
    let more = transfer.sent < transfer.data.len();
    connection.change_property8(
        PropMode::REPLACE,
        transfer.requestor,
        transfer.property,
        transfer.kind,
        &transfer.data[transfer.sent..end],
    )?;
    connection.flush()?;
    transfer.sent = end;
    transfer.deadline = Instant::now() + INCR_WAIT;
    Ok(more)
}

/// 这个窗口上没有在途的传输了，就不再盯它的属性变化。窗口可能已经没了，出错不管。
fn unwatch(connection: &impl Connection, transfers: &[Transfer], requestor: Window) {
    if transfers
        .iter()
        .any(|transfer| transfer.requestor == requestor)
    {
        return;
    }
    let _ = connection.change_window_attributes(
        requestor,
        &ChangeWindowAttributesAux::new().event_mask(EventMask::NO_EVENT),
    );
    let _ = connection.flush();
}

/// 回话：`property` 是 `None` 就是拒绝。
fn notify(connection: &impl Connection, request: &SelectionRequestEvent, property: Atom) {
    let event = SelectionNotifyEvent {
        response_type: SELECTION_NOTIFY_EVENT,
        sequence: 0,
        time: request.time,
        requestor: request.requestor,
        selection: request.selection,
        target: request.target,
        property,
    };
    let sent = connection
        .send_event(false, request.requestor, EventMask::NO_EVENT, event)
        .map(|_| ())
        .and_then(|()| connection.flush());
    if let Err(error) = sent {
        tracing::debug!(%error, "回应剪贴板请求失败");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// 文本那几种：现代程序问的 `UTF8_STRING` 给 UTF-8，ICCCM 里是 Latin-1 的
    /// `STRING` / `TEXT` 得转过来，落不下的字符换个问号——拿 UTF-8 去冒充 Latin-1
    /// 会让整段文字变乱码。
    #[test]
    fn a_text_copy_answers_in_the_encoding_each_target_asks_for() {
        let content = Content::Text("caf\u{e9} 中文".into());
        assert_eq!(
            content.payload("UTF8_STRING"),
            "caf\u{e9} 中文".as_bytes(),
            "现代程序读的是 UTF-8"
        );
        assert_eq!(
            content.payload("text/plain;charset=utf-8"),
            "caf\u{e9} 中文".as_bytes()
        );
        // `\u{e9}` 在 Latin-1 里是 0xE9，汉字不在，换问号。
        assert_eq!(
            content.payload("STRING"),
            [0x63, 0x61, 0x66, 0xe9, 0x20, b'?', b'?']
        );
        assert_eq!(content.payload("TEXT"), content.payload("STRING"));
        // `TEXT` 的回应要写实际的编码，Latin-1 就是 `STRING`。
        assert_eq!(content.kind("TEXT"), "STRING");
        assert_eq!(content.kind("UTF8_STRING"), "UTF8_STRING");
        assert_eq!(content.kind("STRING"), "STRING");
    }

    /// 文件那份还是老样子：`text/uri-list` 是 file URI，其余类型给路径文本。
    #[test]
    fn a_file_copy_keeps_its_own_targets() {
        let content = Content::Files(vec![PathBuf::from("/tmp/a b.txt")]);
        assert_eq!(content.targets(), FILE_TARGETS);
        assert_eq!(
            String::from_utf8(content.payload("text/uri-list")).unwrap(),
            "file:///tmp/a%20b.txt\r\n"
        );
        assert_eq!(
            String::from_utf8(content.payload("UTF8_STRING")).unwrap(),
            "/tmp/a b.txt"
        );
        // 路径文本是 UTF-8，`TEXT` 就照实说。
        assert_eq!(content.payload("TEXT"), content.payload("UTF8_STRING"));
        assert_eq!(content.kind("TEXT"), "UTF8_STRING");
    }
}
