//! VKMS 那一侧的活：造设备、租连接器、还连接器。
//!
//! 整个流程受内核的 configfs 形状限制，而形状决定了这里所有的取舍：
//!
//! * 设备一旦 `enabled=1` 就不能再往里面加显示器（`mkdir` 返回 EBUSY），所以开机
//!   时一次性把 [`SLOTS`] 套「连接器 + 编码器 + CRTC + 平面」都摆好，全部按断开
//!   （`status=2`）落盘，之后要一块屏就只是把某一路的连接器接上。
//! * 反过来，设备一旦被合成器认领就**不能**撤掉了：合成器拿到的是 `/dev/dri/cardN`
//!   的 fd，设备消失后它不会放手（实测 niri 会每秒重试一次 page flip 失败，并把
//!   那块屏永久留在输出列表里占着布局）。所以设备是长期存在的，撤屏只撤连接器——
//!   断开连接器是合成器每天都要处理的正常拔线，它会干净地放掉。
use anyhow::{Context, Result, bail, ensure};
use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const MODULE: &str = "vkms";
/// 这个参数本身就是 configfs 接口的一部分，老内核不认它。
const DEFAULT_DEV_PARAMETER: &str = "create_default_dev";
const CONFIGFS: &str = "/sys/kernel/config";
const ROOT: &str = "/sys/kernel/config/vkms";
/// 实例目录名，同时也是 faux 设备名（`/sys/devices/faux/openuuyc`），靠它认出
/// 哪张 DRM 卡是我们造的。
const INSTANCE: &str = "openuuyc";
/// 一次摆几套管线。主程序那边虚拟屏上限也是 3（见 `displays::Session::create`），
/// 两边不一样就会出现「主控能点、被控建不出来」。改这个数要重建设备，而重建设备会
/// 弄坏正在运行的合成器，所以它只能靠重启换。
const SLOTS: usize = 3;
/// 接上连接器后等内核把它报成 connected 的上限。
const ATTACH: Duration = Duration::from_secs(3);

pub struct Instance {
    /// 本实例占用的 DRM 卡名（`card5`），连接器要靠它拼路径。
    card: String,
}

/// 一次租用：内核给的名字，以及它在 configfs 里对应的那一套管线。
///
/// 字段对外只读：名字是辅助程序发给客户端的，客户端只能原样报回来，不能自己编一个。
#[derive(Clone, Debug)]
pub struct Lease {
    connector: String,
    slot: usize,
}

impl Lease {
    pub fn connector(&self) -> &str {
        &self.connector
    }
    pub fn slot(&self) -> usize {
        self.slot
    }
}

impl Instance {
    /// 接管已有的实例，或者按当前形状造一个。返回时保证 `SLOTS` 路连接器全部断开。
    pub fn open() -> Result<Self> {
        ensure_supported()?;
        load_module()?;
        let directory = instance_dir();
        if directory.exists() && !enabled(&directory)? {
            // 上一次没走到 enable 就死了。这种实例还没有 DRM 设备，撤掉重建最省事，
            // 也不会打扰任何合成器。
            crate::note("发现未启用的残留实例，重建");
            teardown(&directory)?;
        }
        if !directory.exists() {
            build(&directory)?;
        }
        let card = wait_for_card(Duration::from_secs(10))?;
        let instance = Self { card };
        instance.release_all()?;
        Ok(instance)
    }

    /// 现在能给出的路数。形状不对（连接器不够）时以实际数量为准，宁可少给也不要
    /// 去重建设备。
    pub fn capacity(&self) -> Result<usize> {
        Ok(self.connectors()?.len())
    }

    /// 租一路连接器出来，并报出内核给它的名字。
    pub fn attach(&self, taken: &BTreeSet<usize>) -> Result<Lease> {
        let capacity = self.capacity()?;
        ensure!(capacity > 0, "虚拟显示器设备没有可用的连接器");
        for slot in 0..capacity {
            if taken.contains(&slot) {
                continue;
            }
            let before = self.connected()?;
            set_status(slot, Status::Connected)?;
            match self.wait_for_new_connector(&before) {
                Ok(connector) => return Ok(Lease { connector, slot }),
                Err(error) => {
                    // 接上了却认不出来，就把这一路放回去，别让它以「已连接」的状态
                    // 留在那里占着合成器的布局。
                    let _ = set_status(slot, Status::Disconnected);
                    return Err(error);
                }
            }
        }
        bail!("虚拟显示器已经用满（{capacity} 块）")
    }

    /// 还一路。名字必须是 [`Self::attach`] 发出去的。
    pub fn detach(&self, lease: &Lease) -> Result<()> {
        set_status(lease.slot, Status::Disconnected)
    }

    /// 把所有路都断开。启动时和收到退出信号时各来一次：前者清掉上一次运行留下的
    /// 残留，后者保证我们这个进程没了以后屏幕上不多一块屏。
    pub fn release_all(&self) -> Result<()> {
        let connectors = self.connectors()?;
        for slot in 0..connectors.len() {
            if let Err(error) = set_status(slot, Status::Disconnected) {
                crate::note(&format!("第 {slot} 路连接器断开失败：{error:#}"));
            }
        }
        Ok(())
    }

    /// 本卡上的连接器，按内核给的编号排序：`Virtual-5, Virtual-6, Virtual-7` →
    /// 下标 0,1,2 就是我们 configfs 里的 `conn0,conn1,conn2`。
    ///
    /// 这个对应关系是内核 `drm_connector_init` 按创建顺序发号决定的，代码里不假设
    /// 它——租用时是「刚接上的那一路」现场读出来的，排序只用来数数。
    fn connectors(&self) -> Result<Vec<String>> {
        let mut names: Vec<(u32, String)> = Vec::new();
        let directory = Path::new("/sys/class/drm");
        for entry in std::fs::read_dir(directory).context("无法读取 /sys/class/drm")? {
            let entry = entry?;
            let file = entry.file_name().to_string_lossy().into_owned();
            let Some(rest) = file.strip_prefix(&format!("{}-", self.card)) else {
                continue;
            };
            // 带 writeback 的卡会多出一个 `Writeback-1`，它不是显示器。
            if rest.starts_with("Writeback-") {
                continue;
            }
            let Some(index) = rest.strip_prefix("Virtual-") else {
                continue;
            };
            // 报给客户端的是 DRM 连接器名（`Virtual-5`），不是 sysfs 目录名
            // （`card5-Virtual-5`）：合成器只认前者。
            names.push((
                index.parse().context("连接器编号不是数字")?,
                rest.to_owned(),
            ));
        }
        names.sort();
        Ok(names.into_iter().map(|(_, name)| name).collect())
    }

    /// 现在处于 connected 的连接器名字。
    fn connected(&self) -> Result<Vec<String>> {
        let mut connected = Vec::new();
        for name in self.connectors()? {
            if read_trim(&self.sysfs(&name).join("status"))? == "connected" {
                connected.push(name);
            }
        }
        Ok(connected)
    }

    /// 某一路连接器在 sysfs 里的目录。
    fn sysfs(&self, connector: &str) -> PathBuf {
        Path::new("/sys/class/drm").join(format!("{}-{connector}", self.card))
    }

    /// 等一个「刚才还不处于 connected」的连接器出现。
    ///
    /// 不去猜 configfs 里的某一路对应哪个内核名字：接上以后看多了哪一个，那一个就是。
    fn wait_for_new_connector(&self, before: &[String]) -> Result<String> {
        let deadline = Instant::now() + ATTACH;
        loop {
            for name in self.connected()? {
                if !before.contains(&name) {
                    return Ok(name);
                }
            }
            ensure!(
                Instant::now() < deadline,
                "连接器接上后内核没有报出新的显示器"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Status {
    Connected = 1,
    Disconnected = 2,
}

fn instance_dir() -> PathBuf {
    Path::new(ROOT).join(INSTANCE)
}
fn connector_slot(slot: usize) -> PathBuf {
    instance_dir()
        .join("connectors")
        .join(format!("conn{slot}"))
}

/// 让内核加载 vkms。已经加载过就什么都不做——参数是加载那一刻定的，改不了。
fn load_module() -> Result<()> {
    if Path::new("/sys/module/vkms").exists() {
        return Ok(());
    }
    let output = std::process::Command::new("modprobe")
        .args([MODULE, "create_default_dev=N"])
        .output()
        .context("无法执行 modprobe")?;
    ensure!(
        output.status.success(),
        "加载 {MODULE} 模块失败：{}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

/// 内核的 VKMS 太老，没有 configfs 接口：这台机器换内核之前都做不了。
///
/// 单独一个类型，好让调用方把它和「这一次没成」（modprobe 失败、等不到 DRM 卡……）
/// 分开报：前者是「内核不支持」，后者只是当前不可用。
#[derive(Debug)]
pub struct KernelTooOld(pub String);

impl std::fmt::Display for KernelTooOld {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}
impl std::error::Error for KernelTooOld {}

/// 这个内核的 VKMS 有没有 configfs 接口。没有就不要碰它。
///
/// 连接器上的 `status`、`create_default_dev` 参数和 configfs 接口是同一组补丁
/// （`drm/vkms: Add configfs support`）带进主线的，Linux 6.19 才有。老内核上
/// `modprobe vkms create_default_dev=N` 只会打一行 `unknown parameter ... ignored`
/// 然后把模块**照常加载**（`kernel/module/main.c` 的 `unknown_module_param_cb`），
/// 于是用户桌面上平白多出一块一直连着的 `Virtual-1`，而 `/sys/kernel/config/vkms`
/// 根本不存在、建屏必然失败。所以加载之前先问清楚，不支持就什么都别做。
fn ensure_supported() -> Result<()> {
    // 接口在就说明模块已经按支持 configfs 的版本加载好了。
    if Path::new(ROOT).exists() {
        return Ok(());
    }
    if Path::new("/sys/module/vkms").exists() {
        return Err(KernelTooOld(format!(
            "本机加载的 VKMS 模块没有 configfs 接口（{ROOT} 不存在），虚拟屏需要 Linux 6.19 或更新的内核"
        ))
        .into());
    }
    ensure!(
        Path::new(CONFIGFS).exists(),
        "系统没有挂载 configfs（{CONFIGFS}），无法创建虚拟显示器"
    );
    if !module_has_default_dev_parameter()? {
        return Err(KernelTooOld(format!(
            "本机内核 {} 的 VKMS 不支持 configfs，虚拟屏需要 Linux 6.19 或更新的内核",
            kernel_release()
        ))
        .into());
    }
    Ok(())
}

/// `modinfo -F parm vkms` 里有没有 `create_default_dev`。有它就是 6.19 之后的模块。
fn module_has_default_dev_parameter() -> Result<bool> {
    let output = std::process::Command::new("modinfo")
        .args(["-F", "parm", MODULE])
        .output()
        .context("无法执行 modinfo（需要 kmod）")?;
    ensure!(
        output.status.success(),
        "无法读取 {MODULE} 模块的信息：{}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .any(|line| line.trim().starts_with(DEFAULT_DEV_PARAMETER)))
}

/// 内核版本，只用来写进给用户看的原因里。
fn kernel_release() -> String {
    std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .map(|text| text.trim().to_owned())
        .unwrap_or_else(|_| "未知版本".into())
}

fn enabled(instance: &Path) -> Result<bool> {
    Ok(read_trim(&instance.join("enabled"))? == "1")
}

/// 按 configfs 的规矩把实例搭起来：先摆满管线，最后才 enable。
fn build(instance: &Path) -> Result<()> {
    std::fs::create_dir_all(instance)
        .with_context(|| format!("无法创建 {}", instance.display()))?;
    for slot in 0..SLOTS {
        let crtc = instance.join(format!("crtcs/crtc{slot}"));
        let encoder = instance.join(format!("encoders/encoder{slot}"));
        let plane = instance.join(format!("planes/plane{slot}"));
        let connector = connector_slot(slot);
        for directory in [&crtc, &encoder, &plane, &connector] {
            std::fs::create_dir_all(directory)
                .with_context(|| format!("无法创建 {}", directory.display()))?;
        }
        // 管线要连起来才成一条：平面和编码器都能用这个 CRTC，连接器能用这个编码器。
        symlink(
            &crtc,
            &plane.join("possible_crtcs").join(format!("crtc{slot}")),
        )?;
        symlink(
            &crtc,
            &encoder.join("possible_crtcs").join(format!("crtc{slot}")),
        )?;
        symlink(
            &encoder,
            &connector
                .join("possible_encoders")
                .join(format!("encoder{slot}")),
        )?;
        // 一个 CRTC 至少要有主平面；不设的话 enable 时内核直接报错。
        write_attr(&plane.join("type"), "1")?;
        // 先全部断开。要哪一块再单独接上。
        set_status(slot, Status::Disconnected)?;
    }
    write_attr(&instance.join("enabled"), "1")?;
    Ok(())
}

/// 撤掉实例。**只能在它还没 enable 过的时候用**：那样内核里根本没有 DRM 设备，
/// 也就没有合成器认领过它。已经被认领过的设备撤掉会把合成器的输出列表弄脏。
fn teardown(instance: &Path) -> Result<()> {
    ensure!(
        !enabled(instance)?,
        "拒绝撤掉一个已经启用的实例：合成器认领过它，撤掉会留下无法回收的输出"
    );
    for slot in 0..SLOTS {
        let crtc = instance.join(format!("crtcs/crtc{slot}"));
        let encoder = instance.join(format!("encoders/encoder{slot}"));
        let plane = instance.join(format!("planes/plane{slot}"));
        let connector = connector_slot(slot);
        for link in [
            plane.join("possible_crtcs").join(format!("crtc{slot}")),
            encoder.join("possible_crtcs").join(format!("crtc{slot}")),
            connector
                .join("possible_encoders")
                .join(format!("encoder{slot}")),
        ] {
            remove_ignore_missing(&link)?;
        }
        for directory in [&plane, &crtc, &encoder, &connector] {
            remove_ignore_missing(directory)?;
        }
    }
    remove_ignore_missing(instance)?;
    Ok(())
}

/// 等我们的 DRM 卡出现。设备名就是 faux 设备名，因此认得出是哪一张。
fn wait_for_card(deadline: Duration) -> Result<String> {
    let until = Instant::now() + deadline;
    loop {
        if let Ok(entries) = std::fs::read_dir("/sys/class/drm") {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if !name.starts_with("card") || name.contains('-') {
                    continue;
                }
                let device = entry.path().join("device");
                if std::fs::read_link(&device).is_ok_and(|target| target.ends_with(INSTANCE)) {
                    return Ok(name);
                }
            }
        }
        ensure!(
            Instant::now() < until,
            "VKMS 设备已经启用，但 /sys/class/drm 里找不到它"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn set_status(slot: usize, status: Status) -> Result<()> {
    write_attr(
        &connector_slot(slot).join("status"),
        &(status as u8).to_string(),
    )
}

/// 写 configfs 属性。一次 write 必须把整份内容带过去：内核是按这一次调用来解析的，
/// 分两次写第二段会被当成新的值。
fn write_attr(path: &Path, value: &str) -> Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .with_context(|| format!("无法写入 {}", path.display()))?;
    let written = file
        .write(value.as_bytes())
        .with_context(|| format!("写入 {} 失败", path.display()))?;
    ensure!(
        written == value.len(),
        "{} 只写进去了一部分",
        path.display()
    );
    Ok(())
}

fn read_trim(path: &Path) -> Result<String> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("无法读取 {}", path.display()))?;
    Ok(text.trim().to_owned())
}

fn symlink(target: &Path, link: &Path) -> Result<()> {
    if link.exists() {
        return Ok(());
    }
    std::os::unix::fs::symlink(target, link)
        .with_context(|| format!("无法建立管线连接 {}", link.display()))
}

fn remove_ignore_missing(path: &Path) -> Result<()> {
    match std::fs::remove_file(path).or_else(|error| match error.kind() {
        std::io::ErrorKind::IsADirectory => std::fs::remove_dir(path),
        _ => Err(error),
    }) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        other => other.with_context(|| format!("无法删除 {}", path.display())),
    }
}
