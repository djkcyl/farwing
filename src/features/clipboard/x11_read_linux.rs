//! 读 X11 那一侧的剪贴板。
//!
//! Wayland 与 X11 的剪贴板是两座互不相通的孤岛（见 `publish_files`），而进程里那个
//! arboard 实例只会落在 Wayland 上——它跟着 `WAYLAND_DISPLAY` 走。于是 XWayland 里的
//! 程序（文件管理器就是）复制的东西我们一概看不见，往远端自然也发不出去。这里直接
//! 连 X 服务器把那一份读回来。
//!
//! 只读，不接管选择区：我们自己发布文件那一侧归 `x11_offer`。读的每一步都有时限——
//! 这个函数跑在剪贴板工作线程上，卡住会把整条本地通道一起拖死。

use super::native::LocalSnapshot;
use anyhow::{Context as _, Result};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ConnectionExt as _, CreateWindowAux, EventMask, Property, WindowClass,
};

/// 等 X 服务器回话的上限。
const WAIT: Duration = Duration::from_millis(300);
/// 等事件时的轮询间隔。
const POLL: Duration = Duration::from_millis(2);
/// 一次最多搬多少字节。
const MAX: usize = 32 * 1024 * 1024;

/// 这个会话里要不要单独去读 X11 那一侧。
///
/// 三件事都得成立：XWayland 在（`DISPLAY`）、Wayland 会话在（`WAYLAND_DISPLAY`），
/// 而且 arboard 真的落在 Wayland 那一侧（`wayland_clipboard`）。最后一条最要紧：
/// 合成器不给 data-control 时（GNOME/Mutter 就是这样）arboard 自己会退回 X11，
/// 那时 `read_wayland` 读的本来就是 X11 那一份，再读一遍等于把同一个选择区每轮读
/// 两次——图片要解码两次，还每次都新建一条 X 连接。只有 `WAYLAND_DISPLAY` 时没有
/// XWayland，连不上 X 服务器。
///
/// 调用方也用它决定还能不能阻塞等 Wayland 事件，见 `run`。
pub(super) fn wanted(wayland_clipboard: bool) -> bool {
    wayland_clipboard
        && std::env::var_os("DISPLAY").is_some()
        && std::env::var_os("WAYLAND_DISPLAY").is_some()
}

/// 读一次。读不到（没人持有选择区、对端不支持我们要的类型、超时）就是空的。
pub(super) fn read(wayland_clipboard: bool) -> LocalSnapshot {
    if !wanted(wayland_clipboard) {
        return LocalSnapshot::default();
    }
    match collect() {
        Ok(snapshot) => snapshot,
        Err(error) => {
            tracing::debug!(error = format!("{error:#}"), "读取 X11 剪贴板失败");
            LocalSnapshot::default()
        }
    }
}

fn collect() -> Result<LocalSnapshot> {
    let (connection, screen) = x11rb::connect(None).context("连接 X11 失败")?;
    let root = connection.setup().roots[screen].root;
    let atoms = Atoms::intern(&connection)?;
    // 没人持有就没什么可读的：只有 Wayland 那侧有东西时就是这种情况。
    if connection
        .get_selection_owner(atoms.clipboard)?
        .reply()?
        .owner
        == x11rb::NONE
    {
        return Ok(LocalSnapshot::default());
    }
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
        .context("创建剪贴板读取窗口失败")?;

    let offered = offered(&connection, window, &atoms)?;
    // 文件优先，和 Wayland 那侧一致：复制文件的程序通常也把文件名当文本提供，
    // 粘出来一串名字不是要的东西。
    if offered.contains(&atoms.file) {
        let Some(data) = fetch(&connection, window, &atoms, atoms.file)? else {
            return Ok(LocalSnapshot::default());
        };
        let sources: Vec<PathBuf> = crate::platform::linux::drag_drop::parse_uri_list(&data)
            .into_iter()
            .filter(|path| path.is_absolute())
            .collect();
        return Ok(LocalSnapshot {
            sources,
            ..Default::default()
        });
    }
    if offered.contains(&atoms.text) || offered.contains(&atoms.text_plain) {
        let target = if offered.contains(&atoms.text) {
            atoms.text
        } else {
            atoms.text_plain
        };
        if let Some(data) = fetch(&connection, window, &atoms, target)? {
            let text = String::from_utf8_lossy(&data).into_owned();
            return Ok(LocalSnapshot {
                text: (!text.is_empty()).then_some(text),
                ..Default::default()
            });
        }
    }
    if offered.contains(&atoms.png)
        && let Some(data) = fetch(&connection, window, &atoms, atoms.png)?
    {
        let image = image::load_from_memory_with_format(&data, image::ImageFormat::Png)
            .context("解析 PNG 失败")?
            .to_rgba8();
        return Ok(LocalSnapshot {
            image: Some(super::native::rgba_to_dib(
                image.width(),
                image.height(),
                &image,
            )?),
            ..Default::default()
        });
    }
    Ok(LocalSnapshot::default())
}

/// 选择区现在提供哪些类型。
fn offered(connection: &impl Connection, window: u32, atoms: &Atoms) -> Result<Vec<Atom>> {
    let Some(reply) = convert(connection, window, atoms, atoms.targets)? else {
        return Ok(Vec::new());
    };
    let reply = connection
        .get_property(false, window, reply, AtomEnum::ATOM, 0, 512)?
        .reply()
        .context("读取 TARGETS 失败")?;
    Ok(reply
        .value32()
        .map(|values| values.collect())
        .unwrap_or_default())
}

/// 要一次数据，并把结果读回来（大块走 INCR）。
fn fetch(
    connection: &impl Connection,
    window: u32,
    atoms: &Atoms,
    target: Atom,
) -> Result<Option<Vec<u8>>> {
    let Some(property) = convert(connection, window, atoms, target)? else {
        return Ok(None);
    };
    // 先只看类型。
    let reply = connection
        .get_property(false, window, property, AtomEnum::ANY, 0, 0)?
        .reply()
        .context("读取剪贴板属性失败")?;
    if reply.type_ != atoms.incr {
        let reply = connection
            .get_property(true, window, property, AtomEnum::ANY, 0, MAX as u32 / 4)?
            .reply()
            .context("读取剪贴板属性失败")?;
        return Ok(Some(reply.value));
    }
    // INCR：对端每写好一段就删掉属性再设一次，我们每见到一次新值读一段，
    // 读到空的那次为止。
    //
    // `WAIT` 是「两段之间最多等多久」，每收到一段就从那一刻重新算起——不是整次
    // 传输的总时限。一整块几十 MB 传上几秒是正常的，按整次计时会在中途超时，
    // 而这里的超时是直接放弃：上层拿到的是一份空快照，用户看到的是「复制了但
    // 什么都没过来」，连截断都不是。总量另有 `MAX` 管着。
    //
    // 传输从我们删掉 `INCR` 那个记号开始，对端看到删除才写第一段。删除要单独发：
    // `GetProperty` 只在一次读完整个属性时才顺手删掉它，上面那次只读了零个字节。
    connection
        .delete_property(window, property)?
        .check()
        .context("开始分段读取剪贴板失败")?;
    let mut data = Vec::new();
    let mut deadline = Instant::now() + WAIT;
    loop {
        let Some(event) = next(connection, deadline)? else {
            anyhow::bail!("对端没有把数据送完");
        };
        if let Event::PropertyNotify(notify) = event
            && notify.window == window
            && notify.atom == property
            && notify.state == Property::NEW_VALUE
        {
            deadline = Instant::now() + WAIT;
            let reply = connection
                .get_property(true, window, property, AtomEnum::ANY, 0, MAX as u32 / 4)?
                .reply()
                .context("读取剪贴板属性失败")?;
            if reply.value.is_empty() {
                return Ok(Some(data));
            }
            data.extend_from_slice(&reply.value);
            if data.len() > MAX {
                anyhow::bail!("剪贴板内容过大");
            }
        }
    }
}

/// 发一次 `ConvertSelection`，等回话，拿回对端用来放数据的属性名（`None` 是不支持）。
fn convert(
    connection: &impl Connection,
    window: u32,
    atoms: &Atoms,
    target: Atom,
) -> Result<Option<Atom>> {
    connection
        .convert_selection(
            window,
            atoms.clipboard,
            target,
            atoms.transfer,
            x11rb::CURRENT_TIME,
        )?
        .check()
        .context("请求剪贴板数据失败")?;
    connection.flush().context("发送 X11 请求失败")?;
    let deadline = Instant::now() + WAIT;
    while let Some(event) = next(connection, deadline)? {
        match event {
            // 对端不支持这个类型时属性是 None，这就是它说「没有」的方式。
            Event::SelectionNotify(notify) if notify.requestor == window => {
                return Ok((notify.property != x11rb::NONE).then_some(notify.property));
            }
            _ => continue,
        }
    }
    Ok(None)
}

/// 等一个事件，最多等到 `deadline`。到点了返回 `None`。
fn next(connection: &impl Connection, deadline: Instant) -> Result<Option<Event>> {
    loop {
        if let Some(event) = connection.poll_for_event().context("读取 X11 事件失败")? {
            return Ok(Some(event));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        std::thread::sleep(POLL);
    }
}

struct Atoms {
    clipboard: Atom,
    targets: Atom,
    /// 大数据分块传输时对端给的信号。
    incr: Atom,
    /// 我们让对端把数据写进的那个属性，自己起个名字就行。
    transfer: Atom,
    file: Atom,
    text: Atom,
    text_plain: Atom,
    png: Atom,
}

impl Atoms {
    fn intern(connection: &impl Connection) -> Result<Self> {
        let atom = |name: &str| -> Result<Atom> {
            Ok(connection
                .intern_atom(false, name.as_bytes())?
                .reply()
                .with_context(|| format!("intern X11 atom {name}"))?
                .atom)
        };
        Ok(Self {
            clipboard: atom("CLIPBOARD")?,
            targets: atom("TARGETS")?,
            incr: atom("INCR")?,
            transfer: atom("OPENUUYC_CLIP")?,
            file: atom("text/uri-list")?,
            text: atom("UTF8_STRING")?,
            text_plain: atom("text/plain;charset=utf-8")?,
            png: atom("image/png")?,
        })
    }
}
