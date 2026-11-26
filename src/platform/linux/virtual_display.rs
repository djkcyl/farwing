//! 被控端的虚拟显示器：借 root 的辅助程序接一路 VKMS 连接器出来，再让合成器认下它。
//!
//! 为什么必须是「辅助程序造、合成器认」这两步：凭空多一块显示器要写 configfs，只有
//! root 能写（见 `crates/vkms`）；而分辨率由合成器决定，因为 VKMS 的连接器只有一份
//! 固定的无 EDID 模式表，首选还是 1024x768。两步都成了，采集那边什么都不用改——
//! 它是按合成器报出来的输出枚举的，新屏自然就在里面。
use anyhow::{Context, Result};

/// 能不能对主控说「这台机器能加虚拟屏」。
///
/// 三件事都要成立：辅助程序装了、当前桌面接得住新输出、内核的 VKMS 有 configfs 接口。
/// 全都只看本地文件和命令，不连辅助程序——握手时连一次会顺手把那个 root 服务拉起来。
/// 内核那一项查不出来（没有 modinfo）时不拦，交给辅助程序在建屏时说明原因。
pub(crate) fn ready() -> bool {
    openuuyc_vkms::installed()
        && super::display::compositor::available()
        && !matches!(kernel_support(), Kernel::Unsupported(_))
}

/// 内核的 VKMS 能不能经 configfs 现造设备。
#[derive(Debug, PartialEq, Eq)]
enum Kernel {
    Supported,
    /// 确定不行，带着给用户看的原因。
    Unsupported(String),
    /// 查不出来。
    Unknown,
}

/// 不需要 root、也不加载模块地判断内核的 VKMS 有没有 configfs 接口。
///
/// 和辅助程序启动时的检查（`crates/vkms` 的 `ensure_supported`）看的是同一组东西：
/// configfs 接口和 `create_default_dev` 参数是同一组补丁带进 Linux 6.19 的，模块加载
/// 了就看 sysfs，没加载就问 `modinfo`（普通用户也能跑，只是 Debian 系的 PATH 里没有
/// /usr/sbin）。
fn kernel_support() -> Kernel {
    const TOO_OLD: &str = "VKMS 不支持 configfs，虚拟屏需要 Linux 6.19 或更新的内核";
    let release = std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .map(|text| text.trim().to_owned())
        .unwrap_or_else(|_| "未知版本".into());
    if std::path::Path::new("/sys/kernel/config/vkms").exists() {
        return Kernel::Supported;
    }
    if std::path::Path::new("/sys/module/vkms").exists() {
        // 模块已经加载、却没有 configfs 接口：要么是老模块，要么 configfs 没挂载。
        // 后者交给辅助程序去说。
        return if std::path::Path::new("/sys/module/vkms/parameters/create_default_dev").exists() {
            Kernel::Supported
        } else {
            Kernel::Unsupported(format!("本机内核 {release} 加载的 {TOO_OLD}"))
        };
    }
    for program in ["modinfo", "/usr/sbin/modinfo", "/sbin/modinfo"] {
        let Ok(output) = std::process::Command::new(program)
            .args(["-F", "parm", "vkms"])
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
        else {
            continue;
        };
        if !output.status.success() {
            // modinfo 跑起来了却找不到模块：内核根本没编 VKMS。
            return Kernel::Unsupported(format!(
                "本机内核 {release} 没有 VKMS 模块（CONFIG_DRM_VKMS），无法添加虚拟屏"
            ));
        }
        return if lists_default_dev_parameter(&String::from_utf8_lossy(&output.stdout)) {
            Kernel::Supported
        } else {
            Kernel::Unsupported(format!("本机内核 {release} 的 {TOO_OLD}"))
        };
    }
    Kernel::Unknown
}

/// `modinfo -F parm vkms` 的输出里有没有 `create_default_dev`。
fn lists_default_dev_parameter(parameters: &str) -> bool {
    parameters
        .lines()
        .any(|line| line.trim().starts_with("create_default_dev:"))
}

/// 一次被控会话里的虚拟显示器。持有一条到辅助程序的连接。
///
/// 连接本身就是租约：辅助程序在我们断开时把所有连接器还回去，所以进程被杀掉也不会
/// 在用户屏幕上留下收不回的虚拟屏。
pub(crate) struct Session {
    /// 辅助程序的套接字。平时就是 [`openuuyc_vkms::SOCKET`]，测试里换成假的。
    socket: String,
    /// 连接按需建立：没租到屏的连接会被辅助程序按空闲超时收掉，所以这里是
    /// `Option`，下次要用再连。
    client: Option<openuuyc_vkms::Client>,
    leased: Vec<String>,
    /// 连接断过以后，旧连接上租的屏：辅助程序看到连接断开时已经把它们收回了（它重启
    /// 过、被 OOM 杀过……），这边只是还没告诉上层。
    lost: Vec<String>,
}

impl Session {
    pub(crate) fn open() -> Result<Self> {
        Self::open_at(openuuyc_vkms::SOCKET)
    }

    fn open_at(socket: &str) -> Result<Self> {
        let mut session = Self {
            socket: socket.to_owned(),
            client: None,
            leased: Vec::new(),
            lost: Vec::new(),
        };
        session.client()?;
        Ok(session)
    }

    /// 拿一条能用的连接。连上先问一句：套接字在、但服务拒绝我们（比如没有活动的
    /// 图形会话）时，这里就要报出来，而不是等主控点了「添加虚拟屏」才发现。
    fn client(&mut self) -> Result<&mut openuuyc_vkms::Client> {
        if self.client.is_none() {
            let mut client = openuuyc_vkms::Client::connect_to(&self.socket)?;
            let status = client.status().context("虚拟显示器辅助程序不可用")?;
            tracing::debug!(capacity = status.capacity, "虚拟显示器辅助程序已就绪");
            self.client = Some(client);
        }
        self.client.as_mut().context("虚拟显示器辅助程序不可用")
    }

    /// 跑一次请求。连接已经不能用时（辅助程序空闲超时收掉了、或者它重启过）丢掉它
    /// 重连一次再试。
    ///
    /// 重试是安全的：断开连接本身就等于归还这一条连接租的所有连接器，所以死在半路的
    /// 那次请求不会在屏幕上留下收拾不掉的东西。反过来，旧连接上租着的屏这时候也已经
    /// 没了，要从 `leased` 挪到 `lost`：留在 `leased` 里的话，之后删它们只会一直得到
    /// 「不是这条连接创建的虚拟显示器」。
    fn call<T>(&mut self, run: impl Fn(&mut openuuyc_vkms::Client) -> Result<T>) -> Result<T> {
        let first = run(self.client()?);
        match first {
            Ok(value) => Ok(value),
            Err(error) if disconnected(&error) => {
                tracing::debug!(%error, "虚拟显示器辅助程序的连接已失效，重连一次");
                self.client = None;
                if !self.leased.is_empty() {
                    tracing::warn!(
                        connectors = ?self.leased,
                        "到虚拟显示器辅助程序的连接断过，这些虚拟屏已经被它收回"
                    );
                    self.lost.append(&mut self.leased);
                }
                run(self.client()?)
            }
            Err(error) => Err(error),
        }
    }

    /// 要一块 `width`x`height`@`hz` 的屏，返回内核给它的连接器名。
    pub(crate) fn create(&mut self, width: u32, height: u32, hz: u32) -> Result<String> {
        let connector = self.call(|client| client.create(width, height, hz))?;
        // 合成器没接住就立刻还回去：一块没有合成器接管、也没人能用的连接器留在那里，
        // 只会让下一次「添加虚拟屏」挑到它然后失败。
        if let Err(error) = super::display::compositor::adopt(&connector, width, height, hz) {
            if let Err(cleanup) = self.call(|client| client.destroy(&connector)) {
                tracing::error!(%cleanup, connector, "虚拟显示器回滚失败");
            }
            return Err(error);
        }
        self.leased.push(connector.clone());
        Ok(connector)
    }

    pub(crate) fn remove(&mut self, connector: &str) -> Result<()> {
        let result = self.call(|client| client.destroy(connector));
        // 连接断过的话，这块屏在旧连接断开时就已经被收回了：要删的东西已经不在，
        // 这次删除算成功。
        if let Some(index) = self.lost.iter().position(|name| name == connector) {
            self.lost.remove(index);
            return Ok(());
        }
        result?;
        self.leased.retain(|name| name != connector);
        Ok(())
    }

    /// 跟辅助程序对一次账，返回已经不归我们的那些连接器（并且不再记着它们）。
    ///
    /// 上层在加屏之前问一次：辅助程序重启过的话，旧连接上的屏都没了，它们还留在上层
    /// 的记账里就会占着「最多 3 块」的名额，而且永远删不掉。没租着屏就不去惊动辅助
    /// 程序。
    pub(crate) fn sync(&mut self) -> Result<Vec<String>> {
        if !self.leased.is_empty() {
            let status = self.call(|client| client.status())?;
            let (kept, gone): (Vec<String>, Vec<String>) = std::mem::take(&mut self.leased)
                .into_iter()
                .partition(|name| status.leased.contains(name));
            if !gone.is_empty() {
                tracing::warn!(connectors = ?gone, "虚拟显示器辅助程序已经不再为本连接保留这些虚拟屏");
            }
            self.leased = kept;
            self.lost.extend(gone);
        }
        Ok(std::mem::take(&mut self.lost))
    }

    /// 会话结束时把所有屏还回去。失败也不拦着退出：辅助程序那边看到连接断开一样会还。
    ///
    /// 不重连：连接已经断了的话，辅助程序那时就把这条连接上的屏全收回了，在新连接上
    /// 删只会得到「不是这条连接创建的」。
    pub(crate) fn release_all(&mut self) {
        let leased = std::mem::take(&mut self.leased);
        self.lost.clear();
        let Some(mut client) = self.client.take() else {
            return;
        };
        for connector in leased {
            match client.destroy(&connector) {
                Ok(()) => {}
                Err(error) if disconnected(&error) => {
                    tracing::debug!(%error, "到虚拟显示器辅助程序的连接已断开，它已经收回了这些屏");
                    return;
                }
                Err(error) => tracing::warn!(%error, connector, "虚拟显示器归还失败"),
            }
        }
        // 这里丢掉连接：辅助程序看到它断开，把剩下的（如果有）也一并收回。
    }
}

/// 这条连接已经不能用了（而不是辅助程序拒绝了这次请求）。
fn disconnected(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.is::<openuuyc_vkms::Disconnected>())
}

/// 虚拟屏辅助程序现在的状态，直接就是界面上要显示的那几句话。
///
/// 每一态都是用户能采取不同行动的分界：没装要去装，服务没启用要去启用，桌面接不住
/// 要换桌面，内核太老要升级，能用就没事。
#[derive(Clone)]
pub(crate) struct Availability {
    label: String,
    detail: Option<String>,
}

impl Availability {
    /// 界面上的短标签。
    pub(crate) fn label(&self) -> &str {
        &self.label
    }
    /// 需要解释时的原因，能用时为 `None`。
    pub(crate) fn detail(&self) -> Option<&str> {
        self.detail.as_deref()
    }
}

/// 本机现在能不能加虚拟屏。诊断页拿它显示这一行。
///
/// 每一态都对应一件用户能去做的事：装辅助程序、开服务、换桌面、升级内核——所以
/// 「内核不支持」不能和「服务连不上」混着说。
pub(crate) fn availability() -> Availability {
    if !openuuyc_vkms::installed() {
        return Availability {
            label: "未安装".into(),
            detail: Some(
                "安装 openuuyc-vkms 后，被控端才能给主控添加虚拟显示器（需要 systemd 与内核的 VKMS 模块）。"
                    .into(),
            ),
        };
    }
    if !std::path::Path::new(openuuyc_vkms::SOCKET).exists() {
        return Availability {
            label: "已安装，服务未启用".into(),
            detail: Some("运行 systemctl enable --now openuuyc-vkms.socket 之后即可使用。".into()),
        };
    }
    // 服务能起来不等于现在能用：合成器接不住的话，屏建出来也没人把它摆进桌面。
    if !super::display::compositor::available() {
        return Availability {
            label: "已安装，当前桌面不支持".into(),
            detail: Some(super::display::compositor::unavailable_reason()),
        };
    }
    // 内核不够新的话不必去连辅助程序（那会把一个 root 服务拉起来）就能说清楚。
    if let Kernel::Unsupported(reason) = kernel_support() {
        return Availability {
            label: "内核不支持".into(),
            detail: Some(reason),
        };
    }
    match openuuyc_vkms::probe() {
        Ok(status) => Availability {
            label: match status.capacity {
                0 => "可用".into(),
                capacity => format!("可用（最多 {capacity} 块）"),
            },
            detail: (!status.in_use.is_empty())
                .then(|| format!("已在使用：{}", status.in_use.join("、"))),
        },
        // 「内核不支持 configfs」是这台机器做不了，和「服务没起来」不是一回事，
        // 混在一起说会让人去重装服务，白费功夫。
        Err(error)
            if error
                .chain()
                .any(|cause| cause.is::<openuuyc_vkms::Unsupported>()) =>
        {
            Availability {
                label: "内核不支持".into(),
                detail: Some(format!("{error:#}")),
            }
        }
        Err(error) => Availability {
            label: "已安装，当前不可用".into(),
            detail: Some(format!("{error:#}")),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openuuyc_vkms::{Reply, Request, Status};
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;

    /// 6.8 的 vkms 只有这三个参数；6.19 起多了 `create_default_dev`。
    #[test]
    fn the_configfs_parameter_marks_a_new_enough_vkms() {
        let old = "enable_cursor:Enable/Disable cursor support (bool)\n\
                   enable_writeback:Enable/Disable writeback connector support (bool)\n\
                   enable_overlay:Enable/Disable overlay support (bool)\n";
        assert!(!lists_default_dev_parameter(old));
        let new = format!("{old}create_default_dev:Create or not the default VKMS device (bool)\n");
        assert!(lists_default_dev_parameter(&new));
        assert!(!lists_default_dev_parameter(""));
    }

    /// 真机：在这台机器上实际问一次（只读 sysfs、跑 modinfo，不加载模块）。
    #[test]
    #[ignore = "结果取决于本机内核"]
    fn asks_the_running_kernel() {
        println!("{:?}", kernel_support());
    }

    /// 一个假的辅助程序：按顺序接 `connections` 条连接，每条请求交给 `answer`
    /// （参数是第几条连接、请求），它返回 `None` 就当场断开这条连接，模拟辅助程序重启。
    fn fake_helper(
        name: &str,
        connections: usize,
        answer: impl Fn(usize, &Request) -> Option<Reply> + Send + 'static,
    ) -> (String, std::thread::JoinHandle<()>) {
        let directory = std::env::temp_dir().join(format!(
            "openuuyc-virtual-display-{}-{name}",
            std::process::id()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("control.sock");
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let server = std::thread::spawn(move || {
            for index in 0..connections {
                let (stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut writer = stream;
                let mut line = String::new();
                while {
                    line.clear();
                    reader.read_line(&mut line).unwrap_or(0) > 0
                } {
                    let request: Request = serde_json::from_str(line.trim()).unwrap();
                    let Some(reply) = answer(index, &request) else {
                        break;
                    };
                    let mut text = serde_json::to_vec(&reply).unwrap();
                    text.push(b'\n');
                    writer.write_all(&text).unwrap();
                }
            }
        });
        (path.to_string_lossy().into_owned(), server)
    }

    fn status(leased: &[&str]) -> Reply {
        Reply::status(Status {
            capacity: 3,
            leased: leased.iter().map(|name| (*name).to_owned()).collect(),
            in_use: Vec::new(),
        })
    }

    /// 辅助程序重启过：旧连接上的屏已经没了，对账时要把它们交出去，不再记着。
    #[test]
    fn screens_on_a_lost_connection_are_handed_back_to_the_caller() {
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let (socket, server) = fake_helper("sync", 2, move |connection, request| {
            match (connection, request) {
                // 第一条连接：握手的那次 status 照答，对账的那次直接断开。
                (0, Request::Status)
                    if calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 =>
                {
                    Some(status(&["Virtual-1", "Virtual-2"]))
                }
                (0, _) => None,
                (_, Request::Status) => Some(status(&[])),
                _ => Some(Reply::failed("意外的请求")),
            }
        });
        let mut session = Session::open_at(&socket).unwrap();
        session.leased = vec!["Virtual-1".into(), "Virtual-2".into()];
        let mut lost = session.sync().unwrap();
        lost.sort();
        assert_eq!(lost, ["Virtual-1", "Virtual-2"]);
        assert!(session.leased.is_empty());
        // 交出去以后就不再记着了。
        assert!(session.sync().unwrap().is_empty());
        drop(session);
        server.join().unwrap();
        let _ = std::fs::remove_dir_all(std::path::Path::new(&socket).parent().unwrap());
    }

    /// 删的正好是重连时丢掉的那块：它已经不在了，删除算成功，而不是一直报
    /// 「不是这条连接创建的虚拟显示器」。
    #[test]
    fn removing_a_screen_lost_with_the_old_connection_succeeds() {
        let (socket, server) = fake_helper("remove", 2, |connection, request| {
            match (connection, request) {
                (_, Request::Status) => Some(status(&[])),
                (0, Request::Destroy { .. }) => None,
                (_, Request::Destroy { connector }) => Some(Reply::failed(format!(
                    "{connector} 不是这条连接创建的虚拟显示器"
                ))),
                _ => Some(Reply::failed("意外的请求")),
            }
        });
        let mut session = Session::open_at(&socket).unwrap();
        session.leased = vec!["Virtual-1".into(), "Virtual-2".into()];
        session.remove("Virtual-1").unwrap();
        assert!(session.leased.is_empty());
        // 另一块也跟着旧连接没了，留给上层对账时去掉。
        assert_eq!(session.sync().unwrap(), ["Virtual-2"]);
        // 真的不是我们的屏，照样报错。
        assert!(session.remove("Virtual-9").is_err());
        drop(session);
        server.join().unwrap();
        let _ = std::fs::remove_dir_all(std::path::Path::new(&socket).parent().unwrap());
    }

    /// 真机端到端：要装好并启用辅助程序，还要合成器能接管新输出。
    ///
    /// 它会在桌面上真的多出一块屏再撤掉，所以不放进默认测试集：
    /// `cargo test --lib virtual_display -- --ignored --nocapture`
    #[test]
    #[ignore = "需要本机装好并启用 openuuyc-vkms，且合成器能接管新输出"]
    fn leases_a_screen_from_the_helper_and_gives_it_back() {
        let mut session = Session::open().expect("辅助程序不可用");
        let connector = session.create(1920, 1080, 60).expect("无法创建虚拟屏");
        assert!(
            connector.starts_with("Virtual-"),
            "连接器名异常：{connector}"
        );
        // 采集那边必须能按连接器名算出屏幕清单里的 identity 看见它——主控点「添加虚拟屏」
        // 就是在等这一步，等不到会报 -1（屏幕上明明多了一块）。
        let identity = crate::platform::display::topology::identity(&connector);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let screen = loop {
            if let Ok(screens) = crate::platform::capture::screens()
                && let Some(screen) = screens
                    .into_iter()
                    .find(|s| s.identity.as_deref() == Some(identity.as_str()))
            {
                break screen;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "{connector}（identity {identity}）没进屏幕清单"
            );
            std::thread::sleep(std::time::Duration::from_millis(100));
        };
        println!(
            "{connector}: identity={:?} device_name={} {}x{}",
            screen.identity, screen.device_name, screen.width, screen.height
        );
        session.release_all();
    }
}
