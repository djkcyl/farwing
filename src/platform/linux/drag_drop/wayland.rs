//! Wayland 那边的 drop target：接住远端用户从这块屏上拖走的那些文件，问出路径。
//!
//! X11 那条路是 `portal.rs`（一个临时 XDND 交接窗口）。Wayland 上不需要「合成
//! 一个 drop」——那也做不到，drop 只有合成器能投递。要的东西其实更简单：**我们
//! 自己当那个合法的 drop target**。拖放本来就是合成器按指针位置投递给指针底下那
//! 个客户端的，我们只要在那时候是那个客户端就行。
//!
//! 被控这边的指针本来就停在拖动点上（那个点主控随 `Probe` 一起给了），所以只要
//! 把这块透明的 layer surface 摆到拖动点上去，指针就在它里面。之后要等合成器重新
//! 判定一次指针底下是谁，才会把这次拖放交给它：sway 在 surface 上屏时自己就会重
//! 判；niri 只在指针动过之后才重判，所以等不到时请键鼠注入的引擎把指针在原地重
//! 报一次（见 `platform::linux::input::realign_pointer`），不另建注入设备。
//!
//! 和 X11 那条并存但只走一条（会话类型说了算，见 `features/drag_drop/mod.rs`）：
//! 两个世界的拖放互相看不见（XWayland 不会把 XDND 拖拽桥接给 Wayland 客户端，
//! 反过来也一样）。
use super::{Position, portal::Event, xdnd};
use anyhow::{Context as _, Result, bail};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_data_device, wl_data_device_manager, wl_data_offer, wl_output,
    wl_registry, wl_seat, wl_shm, wl_shm_pool, wl_surface,
};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, WEnum};
use wayland_protocols::xdg::xdg_output::zv1::client::{zxdg_output_manager_v1, zxdg_output_v1};
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};

/// 接取用的方形 surface 边长，以指针为中心。用户拖着东西在动，指针每动一次合成器
/// 就重新判定一次指针底下是谁，所以只要他还在这块里就一定会交给我们；开这么大是
/// 给快速甩动留余量（每一次 motion 事件之间指针可能跳过几十像素）。
const SIDE: i32 = 512;
/// 一直没等到拖放就安静收工。整体超时由 `tick` 管（30 秒），这里不必等那么久。
const LIMIT: Duration = Duration::from_secs(20);
/// 摆好之后等合成器把拖放交过来。上屏时自己重判的合成器一个 roundtrip 就到，靠
/// 原地重报的要到 `REALIGN_AT` 之后；过了这么久还没到，就当这台合成器上这条路走
/// 不通，和 X11 那条一样趁早报 `Unavailable`（对齐 `portal.rs` 的 `CAPTURE_WAIT`）。
const ENTER_WAIT: Duration = Duration::from_millis(600);
/// 摆好之后按这几个时刻各请指针在原地动一下（见
/// `platform::linux::input::realign_pointer`）。先等一小会儿是给 sway 这类合成器
/// 留的：它们自己会在面上屏时重判，那就不必注入任何东西了。
const REALIGN_AT: [Duration; 2] = [Duration::from_millis(50), Duration::from_millis(300)];
/// 接住之后还要等原来那次拖动收场，等不到也最多等这么久：主控那头等的是这一句
/// （见 `run` 末尾），一直不吭声它就一直卡在「正在准备文件」。
const RELEASE: Duration = Duration::from_secs(2);
/// 轮询间隔。合成器有事会立刻唤醒，这个只是兜底。
const POLL: Duration = Duration::from_millis(2);
/// 等合成器给尺寸、等对端写文件列表，各自的上限。
const SETUP: Duration = Duration::from_secs(2);
const READ: Duration = Duration::from_millis(800);
/// 文件列表不可能有多大，防止对端一直写。
const MAX_LIST: usize = 1 << 20;
/// 认得出的文件列表类型。GTK 的程序在 Wayland 上只用得上第一个，但两边的来源都
/// 认一遍不花什么代价。
const MIMES: [&str; 2] = ["text/uri-list", "x-special/gnome-copied-files"];

/// 摆在那儿的接取者。丢掉或者 `cancel` 之后就收工。
pub(crate) struct Catcher {
    stop: Arc<AtomicBool>,
}
impl Catcher {
    /// 摆到 `point` 上：这块屏内的像素坐标，和采集那边（`screen_position`）同一套。
    /// `size` 是这块屏在同一套坐标里的尺寸，用来把点换算成合成器的逻辑坐标。
    pub fn start(
        device: &str,
        size: (u32, u32),
        point: Position,
        allowed: Arc<dyn Fn() -> bool + Send + Sync>,
        notify: Arc<dyn Fn(Event) + Send + Sync>,
    ) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let ending = stop.clone();
        let device = device.to_owned();
        std::thread::Builder::new()
            .name("Wayland drag catch".into())
            .spawn(move || {
                // 这是 Wayland 会话里唯一的一条路，接取不了必须让主控知道原因：一声
                // 不吭地收工，主控只会等 3 秒看门狗然后显示一句「取消了」。`notify`
                // 走的是 `Command::Portal`，对已经结束的条目会被忽略。
                if let Err(error) = run(&device, size, point, &ending, &allowed, &notify) {
                    notify(Event::Failed(format!("{error:#}")));
                }
            })?;
        Ok(Self { stop })
    }
    pub fn cancel(&self) {
        self.stop.store(true, Ordering::Release);
    }
}

/// 合成器的一块输出：`wl_output` 本身、它报上来的名字（v4 才有，之前恒为空串），
/// 以及当前模式的物理尺寸（名字对不上时拿它认屏）。
struct Output {
    proxy: wl_output::WlOutput,
    name: String,
    physical: (i32, i32),
}

#[derive(Default)]
struct State {
    compositor: Option<wl_compositor::WlCompositor>,
    layer: Option<zwlr_layer_shell_v1::ZwlrLayerShellV1>,
    shm: Option<wl_shm::WlShm>,
    seat: Option<wl_seat::WlSeat>,
    manager: Option<wl_data_device_manager::WlDataDeviceManager>,
    /// 拖放事件的来源。要一直拿着，丢了就没有事件了。
    device: Option<wl_data_device::WlDataDevice>,
    xdg: Option<zxdg_output_manager_v1::ZxdgOutputManagerV1>,
    /// 每块屏的 xdg-output，顺序和 `outputs` 一致。要一直拿着，丢了就收不到尺寸。
    described: Vec<zxdg_output_v1::ZxdgOutputV1>,
    /// 每块屏的逻辑位置与尺寸，顺序同上。`layer_shell` 的边距按逻辑算，采集给的
    /// 拖动点按物理算，两边差着一个缩放。
    logical: Vec<Option<((i32, i32), (i32, i32))>>,
    /// 合成器的输出（名字与模式要等绑定之后再走一轮才到）。
    outputs: Vec<Output>,
    /// 这次拖放带的 offer，以及它报出来的类型。
    offer: Option<wl_data_offer::WlDataOffer>,
    mimes: Vec<String>,
    /// 指针已经进到我们这块 surface 上。
    entered: bool,
    /// 文件列表已经问到（`paths` 刚拿到，还没被取走）。
    captured: bool,
    /// 接住的是个没有文件列表的东西（拖的是文字、网页里的图片之类），已经可以
    /// 断定拿不到路径，不必再等。
    refused: bool,
    /// 原来那次拖动收场了：`drop`（松开在我们这块上），或者 `leave`（拖着拖着
    /// 离开了我们这块）。这两条是 Wayland 拖放里唯一能看到的「结束」。
    ended: bool,
    /// 已经发出 `receive`，就等读。
    requested: Option<(String, OwnedFd, OwnedFd)>,
    paths: Option<Vec<PathBuf>>,
    configure: Option<(u32, u32, u32)>,
    closed: bool,
}

/// 拖动点（这块屏内的像素坐标）换算成合成器那边的逻辑坐标：`size` 是采集报的屏幕
/// 尺寸，`logical` 是 `xdg-output` 报的逻辑尺寸，两者差着一个缩放。
///
/// 转过 90 度的屏不用特殊处理：这两个尺寸是同一块屏的两种度量，都还是「用户看着的
/// 那个方向」，轴对轴各按自己的比例缩放就够了（DP-3 那种 2560x1600、1.6 倍、转 90
/// 度的屏上，1600x2560 对 1000x1600，两个轴的比例都是 0.625）。
fn logical_point(point: Position, size: (u32, u32), logical: (i32, i32)) -> (i32, i32) {
    let axis = |at: i32, physical: u32, logical: i32| {
        (f64::from(at) * f64::from(logical) / f64::from(physical.max(1))).round() as i32
    };
    (
        axis(point.x, size.0, logical.0),
        axis(point.y, size.1, logical.1),
    )
}

/// 第 `index` 块屏的逻辑尺寸，合成器还没报时为 `(0, 0)`。
fn logical_size(state: &State, index: usize) -> (i32, i32) {
    state
        .logical
        .get(index)
        .copied()
        .flatten()
        .map_or((0, 0), |(_, size)| size)
}

/// 挑出 `device` 指的是哪块屏。
///
/// 名字对不上是常态：XWayland 那边给的可能压根不是合成器的输出名（老版本直接叫
/// `XWAYLAND0`），而 `wl_output` 在 v4 之前根本不发名字（绑上去拿到的是空串）。
/// 所以照采集那边的三段式来：按名字 → 只有一块屏就用它 → 都不行就按尺寸认（尺寸
/// 按无序对比较，转过 90 度的屏宽高是换过来的）→ 还不行就报错，并把合成器有的
/// 输出列出来。
///
/// 按尺寸认时先比逻辑尺寸、再比物理尺寸：`size` 来自 XWayland 的 RandR，有缩放时
/// 报的多半是逻辑尺寸，而 `wl_output.mode` 是物理像素——只比后者的话，这条退路恰
/// 好死在最需要它的多屏缩放场景里。逻辑尺寸有好几块屏对得上就报错，不再去比物理
/// 尺寸：那样只会挑中凑巧对上的另一块屏。
fn pick(state: &State, device: &str, size: (u32, u32)) -> Result<usize> {
    if let Some(index) = state
        .outputs
        .iter()
        .position(|output| !output.name.is_empty() && output.name == device)
    {
        return Ok(index);
    }
    if state.outputs.len() == 1 {
        tracing::debug!(
            device,
            name = %state.outputs[0].name,
            "合成器只有一块屏，就按它接取"
        );
        return Ok(0);
    }
    let side = |(width, height): (i32, i32)| (width.max(height), width.min(height));
    let wanted = side((size.0 as i32, size.1 as i32));
    let matching = |size_of: &dyn Fn(usize) -> (i32, i32)| -> Vec<usize> {
        (0..state.outputs.len())
            .filter(|&index| {
                let size = size_of(index);
                size != (0, 0) && side(size) == wanted
            })
            .collect()
    };
    let mut matched = matching(&|index| logical_size(state, index));
    if matched.is_empty() {
        matched = matching(&|index| state.outputs[index].physical);
    }
    let describe = |index: usize| {
        let output = &state.outputs[index];
        if output.name.is_empty() {
            format!("无名 {}x{}", output.physical.0, output.physical.1)
        } else {
            output.name.clone()
        }
    };
    match matched[..] {
        [index] => {
            tracing::debug!(device, "输出名对不上，按尺寸认屏");
            Ok(index)
        }
        [] => bail!(
            "合成器里没有名为 {device} 的输出，也没有 {}x{} 的屏（它有：{}）",
            size.0,
            size.1,
            (0..state.outputs.len())
                .map(describe)
                .collect::<Vec<_>>()
                .join("、")
        ),
        _ => bail!(
            "合成器里没有名为 {device} 的输出，按尺寸 {}x{} 认又有好几块屏对得上（{}），\
             分不出是哪一块",
            size.0,
            size.1,
            matched
                .iter()
                .map(|&index| describe(index))
                .collect::<Vec<_>>()
                .join("、")
        ),
    }
}

fn run(
    device: &str,
    size: (u32, u32),
    point: Position,
    stop: &AtomicBool,
    allowed: &Arc<dyn Fn() -> bool + Send + Sync>,
    notify: &Arc<dyn Fn(Event) + Send + Sync>,
) -> Result<()> {
    let connection = Connection::connect_to_env().context("没有可用的 Wayland 会话")?;
    let mut queue = connection.new_event_queue::<State>();
    let handle = queue.handle();
    connection.display().get_registry(&handle, ());
    let mut state = State::default();
    queue
        .roundtrip(&mut state)
        .context("读取 Wayland 全局对象失败")?;
    // 输出绑定之后才会报上自己的名字，所以要再来一轮。
    queue
        .roundtrip(&mut state)
        .context("读取 Wayland 输出描述失败")?;

    // 先确认必需的全局对象，再认屏：两边顺序反过来的话，GNOME 这类缺
    // wlr-layer-shell 的合成器上报出来的会是「没有名为 XWAYLAND0 的输出」，把
    // 真正的原因盖掉，而主控要看到的正是「这个合成器不提供 wlr-layer-shell」。
    let compositor = state
        .compositor
        .clone()
        .context("合成器没有 wl_compositor")?;
    // 缺 layer-shell 的合成器上这条路走不通，得让主控看到原因，不能一声不吭。
    let layer = state
        .layer
        .clone()
        .context("这个合成器不提供 wlr-layer-shell（GNOME 就是这样），没法在指针底下接取拖放")?;
    let shm = state.shm.clone().context("合成器没有 wl_shm")?;
    let seat = state.seat.clone().context("合成器没有 wl_seat")?;
    let manager = state
        .manager
        .clone()
        .context("合成器没有 wl_data_device_manager")?;

    // 逻辑坐标只能问 xdg-output：`wl_output` 给的是物理尺寸，两者之间差着一个缩放
    // （本机 DP-1 是 1.35 倍、DP-3 是 1.6 倍），而 layer surface 的边距是按逻辑算
    // 的。采集那边报的屏幕尺寸和拖动点都在物理这边，两个一除就是缩放。
    let xdg = state
        .xdg
        .clone()
        .context("合成器没有 zxdg_output_manager_v1，读不到屏幕的逻辑尺寸")?;
    state.described = state
        .outputs
        .iter()
        .map(|output| xdg.get_xdg_output(&output.proxy, &handle, ()))
        .collect();
    state.logical = vec![None; state.outputs.len()];
    let known = |state: &State| {
        state
            .logical
            .iter()
            .all(|entry| entry.is_some_and(|(_, (width, height))| width > 0 && height > 0))
    };

    // 认屏。名字对得上就直接用它，不必先把所有屏的逻辑尺寸等齐——合成器里只要有
    // 一块从不报逻辑尺寸的输出（笔记本上拔掉的外接屏就是），等它到齐要白等满
    // `SETUP`。
    let by_name = state
        .outputs
        .iter()
        .position(|output| !output.name.is_empty() && output.name == device);
    let index = match by_name {
        Some(index) => index,
        None => {
            // 名字对不上是常态（XWayland 给的往往不是合成器的输出名，`wl_output`
            // 在 v4 之前也不发名字），这时只能按尺寸认；而尺寸要在物理与逻辑两套
            // 单位里都比一遍（见 `pick`），所以先把逻辑尺寸等齐。正常一轮 roundtrip
            // 就到，`SETUP` 只是兜底。
            let deadline = Instant::now() + SETUP;
            while !known(&state) && Instant::now() < deadline {
                dispatch(&mut queue, &mut state, &connection)?;
                wait(&mut queue, &mut state, &connection, POLL)?;
            }
            pick(&state, device, size)?
        }
    };
    // 认出来的这块屏，它的逻辑尺寸仍要等一轮（`get_xdg_output` 之后事件才到）：
    // layer surface 的边距按逻辑算，采集给的屏幕尺寸与拖动点按物理算，两者差着
    // 一个缩放，见 `logical_point`。只等这一块，别的屏报不报都无所谓。
    let ready = |state: &State| {
        state
            .logical
            .get(index)
            .is_some_and(|entry| entry.is_some_and(|(_, (width, height))| width > 0 && height > 0))
    };
    let deadline = Instant::now() + SETUP;
    while !ready(&state) && Instant::now() < deadline {
        dispatch(&mut queue, &mut state, &connection)?;
        wait(&mut queue, &mut state, &connection, POLL)?;
    }
    let target = state.outputs[index].proxy.clone();
    let (_, (logical_width, logical_height)) = state.logical[index]
        .filter(|(_, (width, height))| *width > 0 && *height > 0)
        .context("合成器没有给出这块屏的逻辑尺寸")?;

    let local = logical_point(point, size, (logical_width, logical_height));

    // data device 必须在这块 surface 上屏**之前**建好。合成器按请求顺序处理：处理
    // 下面那次 commit 时 surface 一上屏，它就重新判定指针底下是谁、把这次拖放交给
    // 我们，而 `data_offer` 与 `enter` 是发给我们这个客户端**当时已有的** data
    // device 的。上屏之后才建的 data device 不会补发，此后指针不动也不会重新判定，
    // 整条路就卡在 `Unavailable`（sway 1.7 上实测如此）。
    state.device = Some(manager.get_data_device(&seat, &handle, ()));

    let surface = compositor.create_surface(&handle, ());
    let shell = layer.get_layer_surface(
        &surface,
        Some(&target),
        zwlr_layer_shell_v1::Layer::Overlay,
        "openuuyc-drag".to_owned(),
        &handle,
        (),
    );
    // 锚左上角加边距就是绝对定位：把这块方形摆在拖动点上，指针本来就在那儿。
    // 贴边时往里收，别让这块露到屏幕外面去（收进来那点也还盖着这个点）。
    let margin = |at: i32, total: i32| (at - SIDE / 2).clamp(0, (total - SIDE).max(0));
    shell.set_size(SIDE as u32, SIDE as u32);
    shell.set_anchor(zwlr_layer_surface_v1::Anchor::Top | zwlr_layer_surface_v1::Anchor::Left);
    shell.set_margin(
        margin(local.1, logical_height),
        0,
        0,
        margin(local.0, logical_width),
    );
    // 不占键盘焦点，也绝不参与布局：`-1` 就是「别把这块算进排布里」，否则
    // niri 会为了它把底下的窗口挤走。
    shell.set_keyboard_interactivity(zwlr_layer_surface_v1::KeyboardInteractivity::None);
    shell.set_exclusive_zone(-1);
    surface.commit();

    let deadline = Instant::now() + SETUP;
    while state.configure.is_none() && !state.closed && Instant::now() < deadline {
        dispatch(&mut queue, &mut state, &connection)?;
        wait(&mut queue, &mut state, &connection, POLL)?;
    }
    let (serial, width, height) = state
        .configure
        .context("合成器没有给出这块 surface 的尺寸")?;

    // 全透明：memfd 的页是内核给的零页，一个字节都不用写就是透明的。透明不代表
    // 收不到输入——surface 有缓冲才算上屏，上了屏才会被判定成指针底下的那个。
    let (buffer, _pool, _fd) = transparent(&shm, &handle, width, height)?;
    surface.attach(Some(&buffer), 0, 0);
    shell.ack_configure(serial);
    surface.commit();

    // 摆好了就等合成器把这次拖放交过来。指针底下换了 surface 时，sway 会自己
    // 重新判定一次，所以先等一小会儿、什么也不注入；等不到再请引擎把指针在原地重报
    // 一次——niri 只在指针真的动过之后才更新拖放目标（`layout.dnd_update` 只在
    // `on_pointer_motion*` 里调用），没有这一下它永远不会把拖放交过来。走的是引擎平时
    // 那条绝对定位，没有第二台注入设备。
    //
    // 两个时刻各推一次：通道满了、或者引擎那一刻正忙时，第一次可能没接上。
    //
    // 还是等不到就当这条路在这台合成器上走不通，和 X11 那条一样报 `Unavailable`
    // 收工——别在这里说「失败」：也可能只是这次拖动本来就结束了。
    let deadline = Instant::now() + ENTER_WAIT;
    let started = Instant::now();
    let mut realigns = REALIGN_AT.into_iter().map(|at| started + at).peekable();
    while !state.entered
        && !state.closed
        && !stop.load(Ordering::Acquire)
        && allowed()
        && Instant::now() < deadline
    {
        dispatch(&mut queue, &mut state, &connection)?;
        if realigns.peek().is_some_and(|at| Instant::now() >= *at) {
            let _ = realigns.next();
            let accepted = crate::platform::linux::input::realign_pointer();
            tracing::debug!(accepted, "请指针在原地动一下，让合成器重新判定指针底下是谁");
        }
        wait(&mut queue, &mut state, &connection, POLL)?;
    }
    if !state.entered {
        tracing::info!(device, "合成器没有把这次拖放交给摆出来的 surface");
        shell.destroy();
        surface.destroy();
        notify(Event::Unavailable);
        return Ok(());
    }

    let deadline = Instant::now() + LIMIT;
    while state.paths.is_none()
        && !state.refused
        && !state.closed
        && !stop.load(Ordering::Acquire)
        && allowed()
        && Instant::now() < deadline
    {
        dispatch(&mut queue, &mut state, &connection)?;
        connection.flush().context("发送 Wayland 请求失败")?;
        // `receive` 是个请求，得先发出去对端才会往管道里写。写完我们这一头的
        // 写端必须关掉，否则读端等不到 EOF。
        if let Some((mime, write, read)) = state.requested.take() {
            drop(write);
            let list = read_list(&read, READ)?;
            let paths = xdnd::parse_uri_list(&list);
            if paths.is_empty() {
                bail!("{mime} 里没有本地路径");
            }
            state.paths = Some(paths);
            state.captured = true;
            break;
        }
        wait(&mut queue, &mut state, &connection, POLL)?;
    }

    let Some(paths) = state.paths.take() else {
        shell.destroy();
        surface.destroy();
        // 接住了却拿不到路径，就把原因说出来：不说的话主控要等满 3 秒看门狗，
        // 界面上一句「取消了」，用户根本不知道是拖的东西不对。
        if state.refused {
            notify(Event::Failed("拖动的不是文件".to_owned()));
        }
        return Ok(());
    };
    tracing::info!(files = paths.len(), "Wayland 拖放接住了");
    notify(Event::Captured(paths, false));

    // 光说「接住了」不够：主控那边要等我们再说一句「原来那次拖动结束了」，才
    // 敢把物理按下从远端收回来交给本地 OLE（`returning::try_start_native` 卡着
    // 的就是这一句），在那之前用户一直看着「正在准备文件」。
    //
    // 结束的时刻我们这边只看得到两个事件：`drop`（松开落在我们这块上）和
    // `leave`（拖到别处去了）。两个都要等，所以这块 surface 现在还不能拆——拆
    // 了就一个都收不到。等不到也最多等到 `RELEASE`：宁可早交一会儿，也不能让
    // 主控干等（那时用户多半已经松手，整次拖放就废了）。
    let deadline = Instant::now() + RELEASE;
    while !state.ended
        && !state.closed
        && !stop.load(Ordering::Acquire)
        && allowed()
        && Instant::now() < deadline
    {
        dispatch(&mut queue, &mut state, &connection)?;
        connection.flush().context("发送 Wayland 请求失败")?;
        wait(&mut queue, &mut state, &connection, POLL)?;
    }
    tracing::info!(seen = state.ended, "Wayland 拖放交接结束");
    shell.destroy();
    surface.destroy();
    if !stop.load(Ordering::Acquire) && allowed() {
        notify(Event::Released);
    }
    Ok(())
}

/// 读一遍待处理事件。
fn dispatch(
    queue: &mut EventQueue<State>,
    state: &mut State,
    connection: &Connection,
) -> Result<()> {
    queue.dispatch_pending(state)?;
    connection.flush().context("发送 Wayland 请求失败")?;
    Ok(())
}

/// 等 socket 上有事件，最多 `timeout`。`dispatch_pending` 只处理已经读进来的，
/// 所以这里要先把连接上的数据读出来——采集那边是同一个套路。
fn wait(
    queue: &mut EventQueue<State>,
    state: &mut State,
    connection: &Connection,
    timeout: Duration,
) -> Result<()> {
    queue.dispatch_pending(state)?;
    connection.flush().context("发送 Wayland 请求失败")?;
    let Some(guard) = connection.prepare_read() else {
        return Ok(());
    };
    let mut poll = libc::pollfd {
        fd: guard.connection_fd().as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // Safe because `poll` only writes into the one descriptor it was given.
    let ready = unsafe { libc::poll(&mut poll, 1, timeout.as_millis() as i32) };
    if ready < 0 {
        let error = std::io::Error::last_os_error();
        drop(guard);
        bail!("等待 Wayland 事件失败：{error}");
    }
    if ready == 0 {
        drop(guard);
        return Ok(());
    }
    match guard.read() {
        Ok(_) => {}
        // socket 报告可读但什么都没剩下，是正常情况。
        Err(wayland_client::backend::WaylandError::Io(error))
            if error.kind() == std::io::ErrorKind::WouldBlock =>
        {
            return Ok(());
        }
        Err(error) => return Err(error).context("读取 Wayland 事件失败"),
    }
    Ok(())
}

/// 一块全透明的共享内存缓冲，连同撑住它的东西（池子和 fd 要活到 surface 销毁）。
fn transparent(
    shm: &wl_shm::WlShm,
    handle: &QueueHandle<State>,
    width: u32,
    height: u32,
) -> Result<(wl_buffer::WlBuffer, wl_shm_pool::WlShmPool, OwnedFd)> {
    let stride = width * 4;
    let length = (stride as usize * height as usize).max(4);
    let name = std::ffi::CString::new("openuuyc-drag")?;
    // Safe because the name is a valid C string and the flags are a plain bit set.
    let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        bail!("创建共享内存失败：{}", std::io::Error::last_os_error());
    }
    // Safe because the descriptor was just created by us and nothing else owns it.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    // Safe because the descriptor is ours and open for writing.
    if unsafe { libc::ftruncate(fd.as_raw_fd(), length as libc::off_t) } < 0 {
        bail!("调整共享内存大小失败：{}", std::io::Error::last_os_error());
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

/// 把对端写进管道的内容读完。它写完就关，所以读到 EOF 为止。
fn read_list(fd: &OwnedFd, timeout: Duration) -> Result<Vec<u8>> {
    // 非阻塞读取：`poll` 说可读之后仍然可能被别的东西抢先读空。
    // Safe because the descriptor is ours and `F_GETFL`/`F_SETFL` take no pointers.
    unsafe {
        let flags = libc::fcntl(fd.as_raw_fd(), libc::F_GETFL);
        libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK);
    }
    let deadline = Instant::now() + timeout;
    let mut data = Vec::new();
    let mut buffer = [0u8; 8192];
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            bail!("对端没有在时限内给出文件列表");
        }
        let mut poll = libc::pollfd {
            fd: fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // Safe because `poll` only writes into the one descriptor it was given.
        let ready = unsafe { libc::poll(&mut poll, 1, remaining.as_millis() as i32) };
        if ready < 0 {
            bail!("等待文件列表失败：{}", std::io::Error::last_os_error());
        }
        if ready == 0 {
            continue;
        }
        // Safe because `buffer` is a live slice and the descriptor is open.
        let read = unsafe { libc::read(fd.as_raw_fd(), buffer.as_mut_ptr().cast(), buffer.len()) };
        match read {
            0 => break,
            size if size > 0 => {
                data.extend_from_slice(&buffer[..size as usize]);
                if data.len() > MAX_LIST {
                    bail!("文件列表过大");
                }
            }
            _ => {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::WouldBlock {
                    continue;
                }
                bail!("读取文件列表失败：{error}");
            }
        }
    }
    Ok(data)
}

/// 指针进了我们这块 surface：把文件列表要过来。
fn request(state: &mut State) {
    if state.requested.is_some() || !state.entered {
        return;
    }
    let Some(offer) = state.offer.as_ref() else {
        return;
    };
    let Some(mime) = MIMES
        .iter()
        .find(|mime| state.mimes.iter().any(|offered| offered == *mime))
    else {
        return;
    };
    let (read, write) = match pipe() {
        Ok(ends) => ends,
        Err(error) => {
            tracing::debug!(%error, "建立管道失败");
            return;
        }
    };
    offer.receive(mime.to_string(), write.as_fd());
    state.requested = Some(((*mime).to_owned(), write, read));
}

fn pipe() -> Result<(OwnedFd, OwnedFd)> {
    let mut ends = [0i32; 2];
    // Safe because `ends` is a live array of exactly the two descriptors `pipe2` writes.
    if unsafe { libc::pipe2(ends.as_mut_ptr(), libc::O_CLOEXEC) } < 0 {
        bail!("创建管道失败：{}", std::io::Error::last_os_error());
    }
    // Safe because both descriptors were just created by us and nothing else owns them.
    unsafe { Ok((OwnedFd::from_raw_fd(ends[0]), OwnedFd::from_raw_fd(ends[1]))) }
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
        match interface.as_str() {
            "wl_compositor" => {
                state.compositor = Some(registry.bind::<wl_compositor::WlCompositor, _, _>(
                    name,
                    version.min(1),
                    handle,
                    (),
                ))
            }
            "wl_shm" => {
                state.shm =
                    Some(registry.bind::<wl_shm::WlShm, _, _>(name, version.min(1), handle, ()))
            }
            "wl_seat" => {
                state.seat =
                    Some(registry.bind::<wl_seat::WlSeat, _, _>(name, version.min(1), handle, ()))
            }
            "wl_data_device_manager" => {
                state.manager = Some(
                    registry.bind::<wl_data_device_manager::WlDataDeviceManager, _, _>(
                        name,
                        version.min(1),
                        handle,
                        (),
                    ),
                )
            }
            "zxdg_output_manager_v1" => {
                state.xdg = Some(
                    registry.bind::<zxdg_output_manager_v1::ZxdgOutputManagerV1, _, _>(
                        name,
                        version.min(3),
                        handle,
                        (),
                    ),
                )
            }
            "zwlr_layer_shell_v1" => {
                state.layer = Some(
                    registry.bind::<zwlr_layer_shell_v1::ZwlrLayerShellV1, _, _>(
                        name,
                        version.min(1),
                        handle,
                        (),
                    ),
                )
            }
            // `wl_output.name` 是 v4 才有的，也就是认这块屏靠的那个字段。
            "wl_output" => state.outputs.push(Output {
                proxy: registry.bind::<wl_output::WlOutput, _, _>(name, version.min(4), handle, ()),
                name: String::new(),
                physical: (0, 0),
            }),
            _ => {}
        }
    }
}

impl Dispatch<wl_output::WlOutput, ()> for State {
    fn event(
        state: &mut Self,
        output: &wl_output::WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(entry) = state
            .outputs
            .iter_mut()
            .find(|entry| entry.proxy == *output)
        else {
            return;
        };
        match event {
            wl_output::Event::Name { name } => entry.name = name,
            // 合成器可能把几个模式都报一遍，只认标了 current 的那个。
            wl_output::Event::Mode {
                flags,
                width,
                height,
                ..
            } => {
                if matches!(flags, WEnum::Value(mode) if mode.contains(wl_output::Mode::Current)) {
                    entry.physical = (width, height);
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure {
                serial,
                width,
                height,
            } => state.configure = Some((serial, width, height)),
            zwlr_layer_surface_v1::Event::Closed => state.closed = true,
            _ => {}
        }
    }
}

impl Dispatch<wl_data_device::WlDataDevice, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_data_device::WlDataDevice,
        event: wl_data_device::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_data_device::Event::DataOffer { id } => {
                state.mimes.clear();
                state.offer = Some(id);
                request(state);
            }
            // 指针落到我们这块 surface 上了。类型可能先到、也可能后到，两边都
            // 试一次，凑齐了才发 `receive`。
            wl_data_device::Event::Enter { .. } => {
                state.entered = true;
                request(state);
                // offer 与它的类型清单一定在 `enter` 之前到齐（协议如此），所以这
                // 一刻就能断定：有 offer、却没一个认得出的类型，那拖的不是文件。
                state.refused = state.requested.is_none()
                    && state.offer.is_some()
                    && !state
                        .mimes
                        .iter()
                        .any(|mime| MIMES.contains(&mime.as_str()));
            }
            // 拖起来的那次松开不会以 `wl_pointer.button` 的形式送到我们手上——
            // 隐式抓取把它吃掉了，落点是谁就以 `drop` 告诉谁。
            wl_data_device::Event::Drop => state.ended = true,
            wl_data_device::Event::Leave { .. } => {
                state.entered = false;
                if state.captured {
                    state.ended = true;
                }
            }
            _ => {}
        }
    }

    // `data_offer` 是「创建子对象」的事件，wayland-client 要在这里说明新对象用
    // 什么数据去建。少了这条，合成器真送来这个事件时会 panic，而那个 panic 要跨
    // 过 C 回调、没法 unwinding，**整个进程直接 abort**。本地探针碰不到这条——
    // 没有真的拖放就不会有 `data_offer`，只有真拖一次才踩得上。
    wayland_client::event_created_child!(Self, wl_data_device::WlDataDevice, [
        wl_data_device::EVT_DATA_OFFER_OPCODE => (wl_data_offer::WlDataOffer, ()),
    ]);
}

impl Dispatch<wl_data_offer::WlDataOffer, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_data_offer::WlDataOffer,
        event: wl_data_offer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_data_offer::Event::Offer { mime_type } = event {
            state.mimes.push(mime_type);
            request(state);
        }
    }
}

impl Dispatch<zxdg_output_v1::ZxdgOutputV1, ()> for State {
    fn event(
        state: &mut Self,
        output: &zxdg_output_v1::ZxdgOutputV1,
        event: zxdg_output_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(index) = state.described.iter().position(|it| it == output) else {
            return;
        };
        match event {
            zxdg_output_v1::Event::LogicalPosition { x, y } => {
                let entry = state.logical[index].get_or_insert(((0, 0), (0, 0)));
                entry.0 = (x, y);
            }
            zxdg_output_v1::Event::LogicalSize { width, height } => {
                let entry = state.logical[index].get_or_insert(((0, 0), (0, 0)));
                entry.1 = (width, height);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 拖动点的换算：这块屏是本机 DP-3 那样转过 90 度、1.6 倍缩放的，采集那边报
    /// 1600x2560，合成器报逻辑 1000x1600。
    #[test]
    fn the_drag_point_scales_with_the_output() {
        let point = Position { x: 800, y: 1280 };
        assert_eq!(logical_point(point, (1600, 2560), (1000, 1600)), (500, 800));
        // 没缩放的屏原样过去。
        assert_eq!(
            logical_point(point, (1600, 2560), (1600, 2560)),
            (800, 1280)
        );
    }

    /// 「创建子对象」的事件必须在 `Dispatch` 里给出特化，缺了会在合成器真送来
    /// 事件时 panic，而那个 panic 直接 abort 整个进程。这条就是把这个坑钉住：
    /// 不连真拖放也能查出来。
    ///
    /// 要的只是一个 `QueueHandle`，它只要求一条连接、不要求对面坐着合成器。所以
    /// 用一对 unix socket 自己喂一条连接进去：CI 和 Xorg 会话里没有 Wayland 显示，
    /// `connect_to_env` 会直接失败，而这条测试不该只在有合成器的机器上跑得起来。
    #[test]
    fn data_offer_has_a_child_specialization() {
        let (ours, _theirs) = std::os::unix::net::UnixStream::pair().unwrap();
        let connection = Connection::from_socket(ours).unwrap();
        let queue = connection.new_event_queue::<State>();
        let _ = <State as Dispatch<wl_data_device::WlDataDevice, ()>>::event_created_child(
            0,
            &queue.handle(),
        );
    }
}

wayland_client::delegate_noop!(State: ignore zxdg_output_manager_v1::ZxdgOutputManagerV1);
wayland_client::delegate_noop!(State: ignore wl_compositor::WlCompositor);
wayland_client::delegate_noop!(State: ignore wl_shm::WlShm);
wayland_client::delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
wayland_client::delegate_noop!(State: ignore wl_buffer::WlBuffer);
wayland_client::delegate_noop!(State: ignore wl_seat::WlSeat);
wayland_client::delegate_noop!(State: ignore wl_surface::WlSurface);
wayland_client::delegate_noop!(State: ignore wl_data_device_manager::WlDataDeviceManager);
wayland_client::delegate_noop!(State: ignore zwlr_layer_shell_v1::ZwlrLayerShellV1);
