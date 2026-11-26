//! 借 winit 那条 `wl_display` 的第二个客户端：serial 和拖动落点只有它给得了。
//!
//! winit 的 API 面上缺两样东西，而 Wayland 上拖放两个方向都要：
//!
//! - **`wl_pointer.button` 的 serial**：`wl_data_device.start_drag` 只认这个凭据
//!   （它证明发起方正握着那次 implicit grab），没有别的途径拿得到；
//! - **拖动时的落点**：在 `wl_data_device.motion` 里，而 winit 只报
//!   `HoveredFile`/`DroppedFile`，位置一句不给——拖动期间合成器把指针交给了拖动源，
//!   `CursorMoved` 根本不来，读 X 服务器那份坐标是 XWayland 的陈旧值。
//!
//! 于是在 winit 那条连接上再挂一个 `wayland-client` 后端：同一个 `wl_display` 上挂
//! 多个 event queue 是 libwayland 本来的用法（egui-winit 自己的剪贴板就是这么挂在
//! winit 的 display 上的），事件按对象分发进各自的队列，**读 socket 仍然是 winit 在
//! 做**，这里只在每轮事件循环里 `dispatch_pending`（它不读 fd，不会跟 winit 抢）。
//!
//! 拖动期间的两侧都在这里：
//!
//! - 本机往窗口里拖（对方是别的客户端，我们是落点）：`wl_data_device` 报到哪个窗口、
//!   落在哪一点、拖的是什么文件；
//! - 远端文件往本机拖（我们是源）：拿上面那个 serial 发起 `start_drag`，把文件清单
//!   以 `text/uri-list` 交出去，之后松手、取消、目标要数据都由合成器转达。
//!
//! 本地播放器的每个观看窗口都要 `watch` 一次；拖动本来就只属于一个窗口。
use super::{Event, Files, xdnd};
use anyhow::{Context as _, Result, bail};
use std::{
    collections::HashMap,
    io::{ErrorKind as IoErrorKind, Read, Write},
    os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd},
    path::PathBuf,
    sync::{Arc, Mutex, mpsc},
    time::{Duration, Instant},
};
use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum,
    backend::{Backend, ObjectId},
    protocol::{
        wl_buffer, wl_data_device, wl_data_device_manager, wl_data_offer, wl_data_source,
        wl_pointer, wl_registry, wl_seat, wl_shm, wl_shm_pool, wl_surface,
    },
};
use winit::{
    raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle},
    window::Window,
};

/// `BTN_LEFT`，只认它：拖放是左键的事。
const BTN_LEFT: u32 = 0x110;
/// 认得出的文件清单类型。GTK 的程序在 Wayland 上只挂第一个。
const LIST_MIME: &str = "text/uri-list";
/// 文件清单再大也有限，防止对端一直写。
const LIST_LIMIT: usize = 1 << 20;
/// 等准备线程交出文件的上限：它可能要去挂载、去下载。
const PREPARE_WAIT: Duration = Duration::from_secs(30);
/// 一次悬停超过这么久没动静就丢掉（合成器偶尔漏 `leave`）。
const HOVER_LIMIT: Duration = Duration::from_secs(30);
/// 落下之后等清单写完的上限；源一直不写完就按已经读到的那些算。
const DROP_LIST_WAIT: Duration = Duration::from_secs(5);

/// 一份 `wl_data_offer` 自己报过的东西：对端挂了哪些类型、合成器最后协商出的
/// 动作。`finish` 只在「收的类型对端真有、动作是复制」时才合法，否则是协议错误，
/// 会连同 winit 那条连接一起断掉。
#[derive(Default)]
struct OfferInfo {
    mimes: Vec<String>,
    action: Option<wl_data_device_manager::DndAction>,
}

fn offer_info(offer: &wl_data_offer::WlDataOffer) -> Option<&Mutex<OfferInfo>> {
    offer.data::<Mutex<OfferInfo>>()
}

/// 本机的一次拖动，此刻悬停在这个观看窗口上的样子。
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Hovering {
    /// 窗口内的表面坐标：`wl_data_device` 报的**逻辑**坐标，本机缩放 1.35 时
    /// 和物理像素差着一整个比例。要用它当 winit 的 `PhysicalPosition` 得先乘
    /// 缩放（见 `linux_drag`），直接混用会让整个落点偏出去。
    pub position: (f64, f64),
    pub paths: Vec<PathBuf>,
    /// 这次读到的是不是「已经落下」——落点和悬停走同一套上报。
    pub dropped: bool,
}

/// 从别的线程交给 UI 线程去发起的一次拖动。
struct Request {
    id: u64,
    files: mpsc::Receiver<Result<Files>>,
    allowed: Arc<dyn Fn() -> bool + Send + Sync>,
    notify: Arc<dyn Fn(Event) + Send + Sync>,
    since: Instant,
}

/// 正在进行的本地拖动（我们当源），`files` 一直拿着以保住挂载。
struct Sending {
    id: u64,
    source: wl_data_source::WlDataSource,
    files: Files,
    notify: Arc<dyn Fn(Event) + Send + Sync>,
    allowed: Arc<dyn Fn() -> bool + Send + Sync>,
    dropped: bool,
}

/// 一次读取：管道那头可能还没写完，所以是非阻塞的。
struct Listing {
    file: std::fs::File,
    text: Vec<u8>,
}
impl Listing {
    fn new(fd: OwnedFd) -> Self {
        // 拖动期间的主线程一次也不许停在这里等对端。
        unsafe {
            libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK);
        }
        Self {
            file: std::fs::File::from(fd),
            text: Vec::new(),
        }
    }
    /// 读干现在能读到的；返回是不是读完了。
    fn drain(&mut self) -> bool {
        let mut chunk = [0u8; 4096];
        loop {
            match self.file.read(&mut chunk) {
                Ok(0) => return true,
                Ok(read) => {
                    self.text.extend_from_slice(&chunk[..read]);
                    if self.text.len() > LIST_LIMIT {
                        return true;
                    }
                }
                Err(error) if error.kind() == IoErrorKind::WouldBlock => return false,
                Err(_) => return true,
            }
        }
    }
    fn paths(&self) -> Vec<PathBuf> {
        xdnd::parse_uri_list(&self.text)
    }
}

/// 悬停中的一次本机拖动。
struct Hover {
    owner: u64,
    position: (f64, f64),
    /// `enter` 的 serial：改口（收 / 不收）时 `accept` 要带上它。
    serial: u32,
    /// 现在是不是告诉了合成器「这里收」。观看窗口此刻不能收（没开拖放、指针在
    /// 画面外）时要改口，否则源以为落下会被收下。
    accepting: bool,
    offer: Option<wl_data_offer::WlDataOffer>,
    listing: Option<Listing>,
    paths: Vec<PathBuf>,
    /// 落点已经收到 drop：清单照读，但状态要分开报。
    dropped: bool,
    since: Instant,
}
impl Drop for Hover {
    /// 这次拖动用完了：落下过的告诉源「收完了」，然后把 offer 还回去。
    /// 不还的话每拖进一次就多一个永远不销毁的对象；落下后不 `finish`，有的
    /// 合成器会把这次拖动当成取消，源（Nautilus 之类）还会一直等。
    fn drop(&mut self) {
        let Some(offer) = self.offer.take() else {
            return;
        };
        let finishable = self.dropped
            && self.accepting
            && offer.version() >= 3
            && offer_info(&offer).is_some_and(|info| {
                let info = lock(info);
                info.mimes.iter().any(|mime| mime == LIST_MIME)
                    && info.action == Some(wl_data_device_manager::DndAction::Copy)
            });
        if finishable {
            offer.finish();
        }
        offer.destroy();
    }
}

#[derive(Default)]
struct State {
    handle: Option<QueueHandle<State>>,
    shm: Option<wl_shm::WlShm>,
    seat: Option<wl_seat::WlSeat>,
    manager: Option<wl_data_device_manager::WlDataDeviceManager>,
    registry: Option<wl_registry::WlRegistry>,
    pointer: Option<wl_pointer::WlPointer>,
    device: Option<wl_data_device::WlDataDevice>,
    /// 观看窗口的 surface（协议 id）→ 窗口标识。
    windows: HashMap<u32, u64>,
    /// 指针此刻所在的我们自己的 surface。
    focus: Option<wl_surface::WlSurface>,
    /// 最近一次左键按下的 serial 和按在哪块 surface 上；松手就没了，它只在按着
    /// 的时候管用。`start_drag` 的 origin 必须是这块 surface：GNOME、wlroots 一系
    /// 都会核对「origin 就是握着这次 implicit grab 的 surface」，对不上就不发起。
    press: Option<(u32, wl_surface::WlSurface)>,
    hover: Option<Hover>,
    request: Option<Request>,
    sending: Option<Sending>,
    /// 只给测试用：贴上去的空 buffer 要活到 surface 销毁。
    #[cfg_attr(not(test), allow(dead_code, reason = "只有测试会贴空 buffer"))]
    presented: Vec<(
        wayland_client::protocol::wl_buffer::WlBuffer,
        wayland_client::protocol::wl_shm_pool::WlShmPool,
        OwnedFd,
    )>,
    next_id: u64,
}

struct Inner {
    connection: Connection,
    queue: EventQueue<State>,
    state: State,
}

/// 挂在 winit 的 display 上的那一个客户端。进程里只有一个。
pub(crate) struct Linked {
    inner: Mutex<Inner>,
}

static SHARED: Mutex<Option<Arc<Linked>>> = Mutex::new(None);

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 一次性的管道；`receive` 只认 fd。
///
/// `O_CLOEXEC` 不能省：这个进程会 spawn 一堆子进程（被控端的 agent、门户、
/// 编码器），写端被它们一起继承的话，对端写完关闭之后我们这头也等不到 EOF——
/// 清单永远读不完，拖动就一直停在「正在准备文件」。测试并行跑时踩到过一次。
fn pipe() -> Option<(OwnedFd, OwnedFd)> {
    let mut fds = [0i32; 2];
    // SAFETY: 两个 fd 都是这里创建的，立刻被 OwnedFd 接管。
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return None;
    }
    // SAFETY: `pipe` 成功时两个 fd 都是新的、有效的。
    Some(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

fn wayland_display(window: &Window) -> Result<*mut std::ffi::c_void> {
    let handle = window.display_handle().context("读取窗口的显示连接失败")?;
    match handle.as_raw() {
        RawDisplayHandle::Wayland(handle) => Ok(handle.display.as_ptr()),
        _ => bail!("不是 Wayland 会话"),
    }
}

fn surface_id(surface: *mut std::ffi::c_void) -> Result<ObjectId> {
    // SAFETY: winit 给的指针就是它自己那个 wl_surface，类型对得上（同一个
    // wayland-backend 建的）；`from_ptr` 只读它的类别和 user data。
    unsafe { ObjectId::from_ptr(wl_surface::WlSurface::interface(), surface.cast()) }
        .map_err(|_| anyhow::anyhow!("无法识别窗口的 wl_surface"))
}

impl Linked {
    /// 挂到 winit 的连接上。进程里只真的挂一次，第二个窗口拿到的是同一个。
    pub fn attach(window: &Window) -> Result<Arc<Self>> {
        let mut shared = lock(&SHARED);
        if let Some(linked) = shared.as_ref() {
            return Ok(linked.clone());
        }
        let linked = Arc::new(Self::create(window)?);
        *shared = Some(linked.clone());
        Ok(linked)
    }

    /// 已经挂上的那一个；没挂上（X11 会话）就是 None。
    pub fn shared() -> Option<Arc<Self>> {
        lock(&SHARED).clone()
    }

    fn create(window: &Window) -> Result<Self> {
        let display = wayland_display(window)?;
        // SAFETY: 指针来自 winit，它那条连接就是这个进程里的 libwayland
        // display；借用而不是接管，`from_foreign_display` 正为此存在。
        let backend = unsafe { Backend::from_foreign_display(display.cast()) };
        let connection = Connection::from_backend(backend);
        let mut queue = connection.new_event_queue::<State>();
        let handle = queue.handle();
        connection.display().get_registry(&handle, ());
        let mut state = State {
            handle: Some(handle.clone()),
            ..State::default()
        };
        // 这里会阻塞读 socket，但只在这条事件队列上：libwayland 的
        // prepare_read/read_events 让 winit 那条队列照常收到它的事件，等这一轮
        // 回调返回、winit 下次进 poll 之前自己会把它们分发掉。
        queue
            .roundtrip(&mut state)
            .context("读取 Wayland 全局对象失败")?;
        let manager = state
            .manager
            .clone()
            .context("合成器没有 wl_data_device_manager")?;
        let seat = state.seat.clone().context("合成器没有 wl_seat")?;
        state.pointer = Some(seat.get_pointer(&handle, ()));
        state.device = Some(manager.get_data_device(&seat, &handle, ()));
        connection.flush().context("发送 Wayland 请求失败")?;
        tracing::info!("viewer attached a second wayland client for drag and drop");
        Ok(Self {
            inner: Mutex::new(Inner {
                connection,
                queue,
                state,
            }),
        })
    }

    /// 认一个观看窗口。返回的凭据交给 [`Self::unwatch`]。
    pub fn watch(&self, window: &Window) -> Result<u64> {
        let surface = match window.window_handle().context("读取窗口句柄失败")?.as_raw() {
            RawWindowHandle::Wayland(handle) => handle.surface.as_ptr(),
            _ => bail!("不是 Wayland 窗口"),
        };
        let id = surface_id(surface)?;
        let mut inner = lock(&self.inner);
        let owner = u64::from(window.id());
        inner.state.windows.insert(id.protocol_id(), owner);
        Ok(owner)
    }

    pub fn unwatch(&self, owner: u64) {
        let mut inner = lock(&self.inner);
        inner.state.windows.retain(|_, watched| *watched != owner);
        if inner
            .state
            .hover
            .as_ref()
            .is_some_and(|hover| hover.owner == owner)
        {
            inner.state.hover = None;
        }
    }

    /// 每轮事件循环调用一次：把已经读进来的事件分发给这个队列，再推进拖动。
    pub fn pump(&self) {
        let mut inner = lock(&self.inner);
        let Inner {
            connection,
            queue,
            state,
        } = &mut *inner;
        let _ = connection.flush();
        if queue.dispatch_pending(state).is_err() {
            return;
        }
        Self::drain_hover(state);
        Self::advance(state);
    }

    /// 本机拖动此刻是不是悬停在这个窗口上；`dropped` 的那一次读完就消失。
    pub fn hovering(&self, owner: u64) -> Option<Hovering> {
        let inner = lock(&self.inner);
        let hover = inner
            .state
            .hover
            .as_ref()
            .filter(|hover| hover.owner == owner)?;
        Some(Hovering {
            position: hover.position,
            paths: hover.paths.clone(),
            // 清单读完之前不报落下：drop 常常比清单先到，这时交出去的是空的。
            dropped: hover.dropped && hover.listing.is_none(),
        })
    }

    /// 本机拖动是不是已经落下并交完了（落点那边等它收尾）。
    pub fn drop_taken(&self, owner: u64) -> bool {
        let mut inner = lock(&self.inner);
        let taken =
            inner.state.hover.as_ref().is_some_and(|hover| {
                hover.owner == owner && hover.dropped && hover.listing.is_none()
            });
        if taken {
            inner.state.hover = None;
        }
        taken
    }

    /// 观看窗口此刻能不能收下这次拖动（拖放开着、指针落在画面上）。合成器据此
    /// 决定松手时是投递还是取消，源也据此显示能不能放。
    pub fn set_accepting(&self, owner: u64, accepting: bool) {
        let mut inner = lock(&self.inner);
        let Inner {
            connection, state, ..
        } = &mut *inner;
        let Some(hover) = state.hover.as_mut().filter(|hover| hover.owner == owner) else {
            return;
        };
        if hover.dropped || hover.accepting == accepting {
            return;
        }
        let Some(offer) = hover.offer.as_ref() else {
            return;
        };
        hover.accepting = accepting;
        Self::offer_actions(offer, hover.serial, accepting);
        let _ = connection.flush();
    }

    /// 对一份 offer 说收还是不收：收的话只认文件清单、只做复制。
    fn offer_actions(offer: &wl_data_offer::WlDataOffer, serial: u32, accepting: bool) {
        let has_list = offer_info(offer)
            .is_some_and(|info| lock(info).mimes.iter().any(|mime| mime == LIST_MIME));
        if accepting && has_list {
            offer.accept(serial, Some(LIST_MIME.to_owned()));
            if offer.version() >= 3 {
                offer.set_actions(
                    wl_data_device_manager::DndAction::Copy,
                    wl_data_device_manager::DndAction::Copy,
                );
            }
        } else {
            offer.accept(serial, None);
            if offer.version() >= 3 {
                offer.set_actions(
                    wl_data_device_manager::DndAction::empty(),
                    wl_data_device_manager::DndAction::empty(),
                );
            }
        }
    }

    /// 从任意线程登记一次本地拖动：文件备好之后由 UI 线程发出去。
    ///
    /// `serial` 取的是最近一次左键按下，而那次按下就发生在观看窗口里——用户正是
    /// 按着它把远端的文件拖出画面的。按已经松开就没得发了，`notify` 会收到失败。
    pub fn request_drag(
        &self,
        files: mpsc::Receiver<Result<Files>>,
        allowed: Arc<dyn Fn() -> bool + Send + Sync>,
        notify: Arc<dyn Fn(Event) + Send + Sync>,
    ) -> u64 {
        let mut inner = lock(&self.inner);
        let id = inner.state.next_id.wrapping_add(1).max(1);
        inner.state.next_id = id;
        if let Some(previous) = inner.state.request.take() {
            (previous.notify)(Event::Finished {
                accepted: false,
                effect: 0,
                error: Some("被新的拖动顶掉了".into()),
            });
        }
        inner.state.request = Some(Request {
            id,
            files,
            allowed,
            notify,
            since: Instant::now(),
        });
        id
    }

    /// 取消一次还没发出去或者正在进行的拖动。
    pub fn abort_drag(&self, id: u64) {
        let mut inner = lock(&self.inner);
        if inner.state.request.as_ref().is_some_and(|it| it.id == id) {
            inner.state.request = None;
        }
        if inner.state.sending.as_ref().is_some_and(|it| it.id == id) {
            State::stop_sending(&mut inner.state, false, None);
        }
    }

    /// 把悬停读干：清单是非阻塞读的，读完才知道拖的是哪些文件。
    fn drain_hover(state: &mut State) {
        let Some(hover) = state.hover.as_mut() else {
            return;
        };
        if let Some(listing) = hover.listing.as_mut() {
            let done = listing.drain();
            if done || (hover.dropped && hover.since.elapsed() > DROP_LIST_WAIT) {
                hover.paths = listing.paths();
                hover.listing = None;
            }
        }
        if hover.since.elapsed() > HOVER_LIMIT {
            state.hover = None;
        }
    }

    /// 推进拖动：发出待发的那一次，收掉已经作废的那一次。
    fn advance(state: &mut State) {
        if let Some(request) = state.request.as_mut() {
            if (request.allowed)() {
                match request.files.try_recv() {
                    Ok(files) => {
                        // 用户可能已经松手了；没有按着就没有 implicit grab，
                        // 这个 serial 发出去只会被合成器丢掉。
                        match (files, state.press.clone()) {
                            (Ok(files), Some((serial, origin))) => {
                                let request = state.request.take().unwrap();
                                if let Err(error) =
                                    Self::start(state, request, files, serial, &origin)
                                {
                                    tracing::warn!(%error, "无法发起本地文件拖动");
                                }
                            }
                            (Ok(_), None) => {
                                let request = state.request.take().unwrap();
                                (request.notify)(Event::Finished {
                                    accepted: false,
                                    effect: 0,
                                    error: Some("按下已经松开，无法再发起拖动".into()),
                                });
                            }
                            (Err(error), _) => {
                                let request = state.request.take().unwrap();
                                (request.notify)(Event::Finished {
                                    accepted: false,
                                    effect: 0,
                                    error: Some(error.to_string()),
                                });
                            }
                        }
                    }
                    Err(mpsc::TryRecvError::Disconnected) => {
                        let request = state.request.take().unwrap();
                        (request.notify)(Event::Finished {
                            accepted: false,
                            effect: 0,
                            error: Some("拖动准备线程已停止".into()),
                        });
                    }
                    Err(mpsc::TryRecvError::Empty) if request.since.elapsed() > PREPARE_WAIT => {
                        let request = state.request.take().unwrap();
                        (request.notify)(Event::Finished {
                            accepted: false,
                            effect: 0,
                            error: Some("准备拖动文件超时".into()),
                        });
                    }
                    Err(mpsc::TryRecvError::Empty) => {}
                }
            } else {
                let request = state.request.take().unwrap();
                (request.notify)(Event::Finished {
                    accepted: false,
                    effect: 0,
                    error: None,
                });
            }
        }
        let revoked = state
            .sending
            .as_ref()
            .is_some_and(|sending| !(sending.allowed)());
        if revoked {
            State::stop_sending(state, false, None);
        }
    }

    /// 真正发起一次 Wayland 拖动。origin 是用户按着的那块观看窗口的 surface：
    /// 合成器核对的正是「这次 implicit grab 属于它」。
    fn start(
        state: &mut State,
        request: Request,
        files: Files,
        serial: u32,
        origin: &wl_surface::WlSurface,
    ) -> Result<()> {
        let (manager, device, handle) = (
            state.manager.clone(),
            state.device.clone(),
            state.handle.clone(),
        );
        let (Some(manager), Some(device), Some(handle)) = (manager, device, handle) else {
            bail!("拖放连接尚未就绪");
        };
        if files.paths.is_empty() {
            (request.notify)(Event::Finished {
                accepted: false,
                effect: 0,
                error: Some("没有可拖出的文件".into()),
            });
            return Ok(());
        }
        // 上一次还没收尾就再来一次：先把旧的收掉，别把它的通知漏了。
        State::stop_sending(state, false, Some("被新的拖动顶掉了".into()));
        let source = manager.create_data_source(&handle, ());
        source.offer(LIST_MIME.to_owned());
        source.offer("text/plain;charset=utf-8".to_owned());
        source.set_actions(wl_data_device_manager::DndAction::Copy);
        device.start_drag(Some(&source), origin, None, serial);
        tracing::info!(
            drag = request.id,
            selections = files.paths.len(),
            "本地文件拖动已发起"
        );
        state.sending = Some(Sending {
            id: request.id,
            source,
            files,
            notify: request.notify,
            allowed: request.allowed,
            dropped: false,
        });
        Ok(())
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        handle: &QueueHandle<Self>,
    ) {
        let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        else {
            return;
        };
        state.registry = Some(registry.clone());
        match interface.as_str() {
            "wl_seat" => {
                state.seat = Some(registry.bind(name, version.min(8), handle, ()));
            }
            "wl_data_device_manager" => {
                state.manager = Some(registry.bind(name, version.min(3), handle, ()));
            }
            "wl_shm" => {
                state.shm = Some(registry.bind(name, version.min(1), handle, ()));
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        _: &mut Self,
        _: &wl_seat::WlSeat,
        _: wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_surface::WlSurface, ()> for State {
    fn event(
        _: &mut Self,
        _: &wl_surface::WlSurface,
        _: wl_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_shm::WlShm, ()> for State {
    fn event(
        _: &mut Self,
        _: &wl_shm::WlShm,
        _: wl_shm::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_shm_pool::WlShmPool, ()> for State {
    fn event(
        _: &mut Self,
        _: &wl_shm_pool::WlShmPool,
        _: wl_shm_pool::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_buffer::WlBuffer, ()> for State {
    fn event(
        _: &mut Self,
        _: &wl_buffer::WlBuffer,
        _: wl_buffer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_data_device_manager::WlDataDeviceManager, ()> for State {
    fn event(
        _: &mut Self,
        _: &wl_data_device_manager::WlDataDeviceManager,
        _: wl_data_device_manager::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // 只关心左键那一次按住：`start_drag` 要的正是它的 serial 和按住的那块
        // surface。事件只会落在这个进程自己的窗口上。
        match event {
            wl_pointer::Event::Enter { surface, .. } => state.focus = Some(surface),
            wl_pointer::Event::Leave { .. } => state.focus = None,
            wl_pointer::Event::Button {
                serial,
                button: BTN_LEFT,
                state: which,
                ..
            } => match which {
                WEnum::Value(wl_pointer::ButtonState::Pressed) => {
                    state.press = state.focus.clone().map(|surface| (serial, surface));
                }
                WEnum::Value(wl_pointer::ButtonState::Released) => state.press = None,
                _ => {}
            },
            _ => {}
        }
    }
}

impl Dispatch<wl_data_device::WlDataDevice, ()> for State {
    fn event_created_child(
        opcode: u16,
        handle: &QueueHandle<Self>,
    ) -> Arc<dyn wayland_client::backend::ObjectData> {
        assert_eq!(opcode, wl_data_device::EVT_DATA_OFFER_OPCODE);
        handle.make_data::<wl_data_offer::WlDataOffer, Mutex<OfferInfo>>(Mutex::default())
    }

    fn event(
        state: &mut Self,
        _: &wl_data_device::WlDataDevice,
        event: wl_data_device::Event,
        _: &(),
        connection: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if !matches!(event, wl_data_device::Event::Motion { .. }) {
            tracing::debug!(?event, "本机拖放事件");
        }
        match event {
            wl_data_device::Event::Enter {
                serial,
                surface,
                x,
                y,
                id,
            } => {
                let key = surface.id().protocol_id();
                // 上一次没收尾的（漏了 leave）在这里还掉。
                state.hover = None;
                let Some(owner) = state.windows.get(&key).copied() else {
                    // 不是观看窗口，别去接别人的拖动；offer 也还回去。
                    tracing::debug!(surface = key, "拖动落在没登记的 surface 上");
                    if let Some(offer) = id {
                        offer.destroy();
                    }
                    return;
                };
                tracing::debug!(surface = key, owner, x, y, "本机拖动进了观看窗口");
                let mut hover = Hover {
                    owner,
                    position: (f64::from(x), f64::from(y)),
                    serial,
                    accepting: true,
                    offer: id,
                    listing: None,
                    paths: Vec::new(),
                    dropped: false,
                    since: Instant::now(),
                };
                if let Some(offer) = hover.offer.as_ref() {
                    // 先按「收」接下：不 accept 的话合成器会把落点当成不收，松手时
                    // 源等到的就是 cancelled。此刻收不了的话观看窗口下一轮会改口
                    // （`set_accepting`）。没挂文件清单的拖动（纯文字之类）不收。
                    Linked::offer_actions(offer, serial, true);
                    let has_list = offer_info(offer)
                        .is_some_and(|info| lock(info).mimes.iter().any(|mime| mime == LIST_MIME));
                    hover.accepting = has_list;
                    // 悬停期间就问一次清单：落点要据此决定收不收，预览也靠它。
                    if has_list {
                        hover.listing = Self::ask(offer, LIST_MIME);
                    }
                    let _ = connection.flush();
                }
                state.hover = Some(hover);
            }
            wl_data_device::Event::Motion { x, y, .. } => {
                if let Some(hover) = state.hover.as_mut()
                    && !hover.dropped
                {
                    hover.position = (f64::from(x), f64::from(y));
                    hover.since = Instant::now();
                }
            }
            wl_data_device::Event::Leave => {
                // 落下之后合成器紧接着就会发 leave，而落点还要报给上层——
                // 在这里清掉的话，上层永远等不到那一次「落下」。
                if !state.hover.as_ref().is_some_and(|hover| hover.dropped) {
                    state.hover = None;
                }
            }
            wl_data_device::Event::Drop => {
                let Some(hover) = state.hover.as_mut() else {
                    return;
                };
                hover.dropped = true;
                hover.since = Instant::now();
                // 悬停那份可能还没写完（drop 常常先到），落点这里再要一次。
                if let Some(offer) = hover.offer.as_ref()
                    && hover.accepting
                    && hover.paths.is_empty()
                    && hover.listing.is_none()
                {
                    hover.listing = Self::ask(offer, LIST_MIME);
                    let _ = connection.flush();
                }
            }
            // 剪贴板的 offer 也会到这个 data device 上；我们不读，立刻还回去，
            // 否则每复制一次就多一个永远不销毁的对象。
            wl_data_device::Event::Selection { id: Some(offer) } => offer.destroy(),
            _ => {}
        }
    }
}

impl State {
    /// 收尾一次拖动：`accepted` 是「目标真的收下了」。
    fn stop_sending(state: &mut State, accepted: bool, error: Option<String>) {
        let Some(sending) = state.sending.take() else {
            return;
        };
        // 销毁源就是告诉合成器这次拖动结束了。
        sending.source.destroy();
        (sending.notify)(Event::Finished {
            accepted,
            effect: if accepted { super::COPY } else { 0 },
            error,
        });
    }

    /// 向对端要一次文件清单，拿回读那一头。
    fn ask(offer: &wl_data_offer::WlDataOffer, mime: &str) -> Option<Listing> {
        let (read, write) = pipe()?;
        offer.receive(mime.to_owned(), write.as_fd());
        Some(Listing::new(read))
    }
}

impl Dispatch<wl_data_offer::WlDataOffer, Mutex<OfferInfo>> for State {
    fn event(
        _: &mut Self,
        _: &wl_data_offer::WlDataOffer,
        event: wl_data_offer::Event,
        info: &Mutex<OfferInfo>,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        tracing::trace!(?event, "本机拖放 offer 事件");
        match event {
            wl_data_offer::Event::Offer { mime_type } => lock(info).mimes.push(mime_type),
            wl_data_offer::Event::Action {
                dnd_action: WEnum::Value(action),
            } => lock(info).action = Some(action),
            _ => {}
        }
    }
}

impl Dispatch<wl_data_source::WlDataSource, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_data_source::WlDataSource,
        event: wl_data_source::Event,
        _: &(),
        connection: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(sending) = state.sending.as_mut() else {
            return;
        };
        match event {
            wl_data_source::Event::Target { .. } => {}
            wl_data_source::Event::Send { mime_type, fd } => {
                if mime_type != LIST_MIME && mime_type != "text/plain;charset=utf-8" {
                    return;
                }
                let text = if mime_type == LIST_MIME {
                    xdnd::uri_list(&sending.files.paths)
                } else {
                    sending
                        .files
                        .paths
                        .iter()
                        .map(|path| path.to_string_lossy().into_owned())
                        .collect::<Vec<_>>()
                        .join("\n")
                };
                // 对端读得慢（或者干脆不读）时，写满管道会卡住 UI 线程，所以交给
                // 一个短命的线程去写。
                let mut pipe = std::fs::File::from(fd);
                let spawned =
                    std::thread::Builder::new()
                        .name("drag list".into())
                        .spawn(move || {
                            if let Err(error) = pipe.write_all(text.as_bytes()) {
                                tracing::debug!(%error, "写文件清单失败");
                            }
                        });
                if let Err(error) = spawned {
                    tracing::debug!(%error, "无法启动写文件清单的线程");
                }
            }
            wl_data_source::Event::Cancelled => Self::stop_sending(state, false, None),
            wl_data_source::Event::DndDropPerformed => {
                sending.dropped = true;
                (sending.notify)(Event::Released);
            }
            wl_data_source::Event::DndFinished => {
                let accepted = sending.dropped;
                Self::stop_sending(state, accepted, None);
            }
            wl_data_source::Event::Action { dnd_action } => {
                let effect = match dnd_action {
                    WEnum::Value(action)
                        if action.contains(wl_data_device_manager::DndAction::Copy) =>
                    {
                        super::COPY
                    }
                    _ => 0,
                };
                (sending.notify)(Event::Feedback(effect));
            }
            _ => {}
        }
        let _ = connection.flush();
    }
}

/// 一块全透明的共享内存缓冲，连同撑住它的池子和 fd 要活到 surface 销毁。
#[cfg(test)]
fn empty_buffer(
    shm: &wl_shm::WlShm,
    handle: &QueueHandle<State>,
    width: u32,
    height: u32,
) -> Result<(wl_buffer::WlBuffer, wl_shm_pool::WlShmPool, OwnedFd)> {
    let stride = width * 4;
    let length = (stride as usize * height as usize).max(4);
    let name = std::ffi::CString::new("openuuyc-dnd-test")?;
    // SAFETY: 名字是合法 C 字符串，flags 是一组普通位。
    let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        bail!("创建共享内存失败：{}", std::io::Error::last_os_error());
    }
    // SAFETY: 这个 fd 是刚创建的，没有别的主人。
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    // SAFETY: fd 是我们的且可写。
    if unsafe { libc::ftruncate(fd.as_raw_fd(), length as libc::off_t) } < 0 {
        bail!("调整共享内存大小失败：{}", std::io::Error::last_os_error());
    }
    // 涂一层不透明色：这块窗口是给人看的，全透明的话找都找不到。
    // SAFETY: fd 是我们刚 ftruncate 过的，长度对得上；映射完立刻解掉。
    let mapped = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            length,
            libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd.as_raw_fd(),
            0,
        )
    };
    if mapped != libc::MAP_FAILED {
        // SAFETY: 映射成功、长度是 4 的倍数，按 u32 写。
        unsafe {
            std::slice::from_raw_parts_mut(mapped as *mut u32, length / 4).fill(0xff20_60a0);
            libc::munmap(mapped, length);
        }
    }
    let pool = shm.create_pool(fd.as_fd(), length as i32, handle, ());
    let buffer = pool.create_buffer(
        0,
        width as i32,
        height as i32,
        stride as i32,
        wl_shm::Format::Argb8888,
        handle,
        (),
    );
    Ok((buffer, pool, fd))
}

#[cfg(test)]
impl Linked {
    /// 测试用：贴一块空 buffer 让窗口真的上屏。
    ///
    /// 真实的观看窗口一直在画（egui 那套），本来就不缺这一步；测试里没有渲染器，
    /// 不贴 buffer 合成器就不把这个 surface 当窗口，指针落不到它上面，拖放也就
    /// 无从谈起。surface 是 winit 建的那个，只是由我们这条连接来贴——同一个
    /// display、同一个客户端，本来就该如此。
    pub(crate) fn present_empty(&self, window: &Window) -> Result<()> {
        let surface = match window.window_handle()?.as_raw() {
            RawWindowHandle::Wayland(handle) => handle.surface.as_ptr(),
            _ => bail!("不是 Wayland 窗口"),
        };
        let size = window.inner_size();
        let (width, height) = (size.width.max(1), size.height.max(1));
        let surface =
            wl_surface::WlSurface::from_id(&lock(&self.inner).connection, surface_id(surface)?)
                .map_err(|_| anyhow::anyhow!("无法接管窗口的 wl_surface"))?;
        let mut inner = lock(&self.inner);
        let Inner {
            connection,
            queue,
            state,
        } = &mut *inner;
        let shm = state.shm.clone().context("合成器没有 wl_shm")?;
        let handle = state.handle.clone().context("拖放连接尚未就绪")?;
        let (buffer, pool, fd) = empty_buffer(&shm, &handle, width, height)?;
        surface.attach(Some(&buffer), 0, 0);
        surface.damage(0, 0, width as i32, height as i32);
        surface.commit();
        let _ = connection.flush();
        if queue.dispatch_pending(state).is_err() {
            bail!("分发 Wayland 事件失败");
        }
        state.presented.push((buffer, pool, fd));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use winit::{
        application::ApplicationHandler,
        event::WindowEvent,
        event_loop::{ActiveEventLoop, ControlFlow, EventLoop},
        platform::wayland::EventLoopBuilderExtWayland as _,
        window::{WindowAttributes, WindowId},
    };

    /// 清单是对端异步写过来的：没写完不算完，写完了要一次读到，写端关了才算收工。
    /// 拖动期间主线程一次也不能停在这儿等，所以这三步都必须是「读到现在能读的」。
    #[test]
    fn listing_reads_what_arrived_without_waiting() {
        let (read, write) = pipe().unwrap();
        let mut listing = Listing::new(read);
        assert!(!listing.drain(), "对端一个字都还没写，不该算读完");
        let mut write = std::fs::File::from(write);
        write.write_all(b"file:///tmp/a.txt\r\n").unwrap();
        assert!(!listing.drain(), "写端还开着，还不算完");
        assert_eq!(listing.paths(), vec![PathBuf::from("/tmp/a.txt")]);
        write
            .write_all(b"# a comment is not a path\nfile:///tmp/b c.txt\n")
            .unwrap();
        drop(write);
        // 写端可能在别处被短暂继承一份：这个进程里的测试会 fork 子进程（被控端
        // agent、门户那一类），从 fork 到 exec 之间的那一小段里，子进程也拿着
        // 这份写端，EOF 于是会迟一点才到。`O_CLOEXEC` 已经把 exec 之后的窗口
        // 关掉了，剩下的这一小段只能等——别把别人的窗口期当成我们的 bug。
        let mut done = false;
        for _ in 0..50 {
            if listing.drain() {
                done = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(done, "写端关了才算读完");
        assert_eq!(
            listing.paths(),
            vec![PathBuf::from("/tmp/a.txt"), PathBuf::from("/tmp/b c.txt")]
        );
    }

    /// 这些测试跑在真机上，出问题时要看得见库里那几条 debug 日志。
    fn trace() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter("debug,openuuyc::platform::linux::drag_drop=trace")
            .with_test_writer()
            .try_init();
    }

    fn wayland_session() -> bool {
        trace();
        if std::env::var("XDG_SESSION_TYPE").as_deref() != Ok("wayland") {
            eprintln!("跳过：不是在 Wayland 会话里");
            return false;
        }
        true
    }

    /// 起一个真的上了屏的窗口，把 `Linked` 挂上去并登记它。
    ///
    /// 窗口必须上屏：合成器只把指针交给指针底下的那个 surface，没贴 buffer 的
    /// 窗口在它眼里就不存在，拖动落不到上面。
    struct Host {
        linked: Option<Arc<Linked>>,
        /// 窗口得活着，不然它连同 surface 一起没了。
        #[allow(dead_code, reason = "持有着让窗口别被销毁")]
        window: Option<Arc<Window>>,
        owner: u64,
        deadline: Instant,
        title: &'static str,
    }

    impl Host {
        fn new(title: &'static str) -> Self {
            Self {
                linked: None,
                window: None,
                owner: 0,
                deadline: Instant::now() + Duration::from_secs(300),
                title,
            }
        }

        fn ready(&mut self, event_loop: &ActiveEventLoop) -> bool {
            if self.linked.is_some() {
                return true;
            }
            let window = match event_loop
                .create_window(WindowAttributes::default().with_title(self.title))
            {
                Ok(window) => Arc::new(window),
                Err(error) => {
                    eprintln!("建窗口失败：{error}");
                    event_loop.exit();
                    return false;
                }
            };
            let linked = match Linked::attach(&window) {
                Ok(linked) => linked,
                Err(error) => {
                    eprintln!("挂拖放通道失败：{error:#}");
                    event_loop.exit();
                    return false;
                }
            };
            let prepared = linked.watch(&window).and_then(|owner| {
                linked.present_empty(&window)?;
                Ok(owner)
            });
            match prepared {
                Ok(owner) => {
                    eprintln!("窗口 {:?} 就绪，owner={owner}", self.title);
                    self.owner = owner;
                }
                Err(error) => {
                    eprintln!("准备窗口失败：{error:#}");
                    event_loop.exit();
                    return false;
                }
            }
            self.linked = Some(linked);
            self.window = Some(window);
            true
        }

        /// pump 之后把状态交给回调读。
        ///
        /// 回调是在 [`Linked`] 的锁里跑的：**不要在它里面调任何加锁的方法**
        /// （`request_drag` 之类），那会死锁——真实代码里那些都是从别的线程调的。
        fn poll(
            &mut self,
            event_loop: &ActiveEventLoop,
            ctx: &mut impl FnMut(&Arc<Linked>),
        ) -> bool {
            if let Some(linked) = self.linked.clone() {
                linked.pump();
                ctx(&linked);
            }
            if Instant::now() > self.deadline {
                eprintln!("等超时了");
                event_loop.exit();
                return false;
            }
            event_loop.set_control_flow(ControlFlow::WaitUntil(
                Instant::now() + Duration::from_millis(15),
            ));
            true
        }
    }

    fn app<A: ApplicationHandler + 'static>(app: &mut A) {
        let event_loop = EventLoop::builder()
            .with_any_thread(true)
            .build()
            .expect("创建事件循环失败");
        event_loop.run_app(app).expect("事件循环失败");
    }

    /// 落点那一半：另一个客户端把文件拖到这个窗口上，位置和清单都要报出来。
    ///
    /// 要外部配合（`/tmp/dnd-probe source` 那种），所以是 `#[ignore]`：
    /// ```text
    /// cargo test --lib -- --ignored --nocapture receives_a_local_drag
    /// ```
    #[test]
    #[ignore = "需要真实 Wayland 会话，还要另一个客户端把文件拖过来"]
    fn receives_a_local_drag() {
        if !wayland_session() {
            return;
        }

        struct Waiter {
            host: Host,
            got: Option<Hovering>,
        }
        impl ApplicationHandler for Waiter {
            fn resumed(&mut self, event_loop: &ActiveEventLoop) {
                if self.host.ready(event_loop) {
                    eprintln!("等另一个客户端把文件拖到窗口上");
                }
            }

            fn window_event(
                &mut self,
                event_loop: &ActiveEventLoop,
                _: WindowId,
                event: WindowEvent,
            ) {
                match event {
                    WindowEvent::CloseRequested => event_loop.exit(),
                    // 合成器定下尺寸之前贴的那块是最小的，configure 之后要补一块。
                    WindowEvent::Resized(_) => {
                        if let (Some(linked), Some(window)) =
                            (self.host.linked.clone(), self.host.window.clone())
                            && let Err(error) = linked.present_empty(&window)
                        {
                            eprintln!("重新贴缓冲失败：{error:#}");
                        }
                    }
                    _ => {}
                }
            }

            fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
                let mut done = None;
                let owner = self.host.owner;
                self.host.poll(event_loop, &mut |linked| {
                    if let Some(hover) = linked.hovering(owner) {
                        eprintln!(
                            "拖动：位置 {:?} 清单 {:?} 落下 {}",
                            hover.position, hover.paths, hover.dropped
                        );
                        if hover.dropped {
                            linked.drop_taken(owner);
                            done = Some(hover);
                        }
                    }
                });
                if let Some(hover) = done {
                    self.got = Some(hover);
                    event_loop.exit();
                }
            }
        }

        let mut waiter = Waiter {
            host: Host::new("openuuyc-dnd-target"),
            got: None,
        };
        app(&mut waiter);
        let got = waiter.got.expect("没有等到拖动");
        assert!(got.dropped, "只看到悬停，没有落下");
        assert!(!got.paths.is_empty(), "落下时清单是空的");
    }

    /// 源那一半：拿一次真实按下的 serial 发起 Wayland 拖动，把清单交出去。
    ///
    /// 按下必须是真的（`start_drag` 认的正是那个 serial），所以请人按一下：
    /// 在这个窗口上按住左键，拖到本机任意一个文件管理器窗口上松开。
    /// ```text
    /// cargo test --lib -- --ignored --nocapture starts_a_local_drag
    /// ```
    #[test]
    #[ignore = "需要真实 Wayland 会话，还要人在窗口上按住左键拖一次"]
    fn starts_a_local_drag() {
        if !wayland_session() {
            return;
        }
        let source = std::env::temp_dir().join("openuuyc-dnd-source.txt");
        std::fs::write(&source, b"dragged by openuuyc\n").unwrap();

        struct Dragger {
            host: Host,
            path: PathBuf,
            requested: bool,
            result: Arc<Mutex<Option<(bool, Option<String>)>>>,
        }
        impl ApplicationHandler for Dragger {
            fn resumed(&mut self, event_loop: &ActiveEventLoop) {
                if self.host.ready(event_loop) {
                    eprintln!("请在这个窗口上按住左键，拖到别处松开");
                }
            }

            fn window_event(
                &mut self,
                event_loop: &ActiveEventLoop,
                _: WindowId,
                event: WindowEvent,
            ) {
                match event {
                    WindowEvent::CloseRequested => event_loop.exit(),
                    WindowEvent::Resized(_) => {
                        if let (Some(linked), Some(window)) =
                            (self.host.linked.clone(), self.host.window.clone())
                            && let Err(error) = linked.present_empty(&window)
                        {
                            eprintln!("重新贴缓冲失败：{error:#}");
                        }
                    }
                    _ => {}
                }
            }

            fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
                self.host.poll(event_loop, &mut |_| {});
                let Some(linked) = self.host.linked.clone() else {
                    return;
                };
                if !self.requested {
                    // 锁已经放开了才登记：`request_drag` 自己要拿那把锁。
                    let serial = lock(&linked.inner)
                        .state
                        .press
                        .as_ref()
                        .map(|press| press.0);
                    if let Some(serial) = serial {
                        eprintln!("拿到按下的 serial={serial}，发起拖动");
                        self.requested = true;
                        let (send, receive) = mpsc::channel();
                        let _ = send.send(Ok(Files {
                            paths: vec![self.path.clone()],
                            _hold: Arc::new(()),
                        }));
                        let slot = self.result.clone();
                        let id = linked.request_drag(
                            receive,
                            Arc::new(|| true),
                            Arc::new(move |event| {
                                eprintln!("拖动事件：{event:?}");
                                if let Event::Finished {
                                    accepted, error, ..
                                } = event
                                {
                                    *lock(&slot) = Some((accepted, error));
                                }
                            }),
                        );
                        eprintln!("拖动已登记（id={id}）");
                    }
                }
                if lock(&self.result).is_some() {
                    event_loop.exit();
                }
            }
        }

        let mut dragger = Dragger {
            host: Host::new("openuuyc-dnd-source"),
            path: source,
            requested: false,
            result: Arc::new(Mutex::new(None)),
        };
        app(&mut dragger);
        let result = lock(&dragger.result).clone().expect("拖动没有收尾");
        eprintln!("收尾：{result:?}");
    }
}
