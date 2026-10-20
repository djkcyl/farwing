#![windows_subsystem = "windows"]
#![allow(
    non_snake_case,
    reason = "The executable uses the Farwing product name."
)]

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use farwing::account::api;
use farwing::account::client::AuthenticatedClient;
use farwing::account::login;
use farwing::application::app;
use farwing::diagnostics::logging;
use farwing::media;
use farwing::product;
use farwing::session::controller;

#[derive(Parser)]
#[command(
    name = "Farwing",
    version,
    about = "Farwing Client — 自建远程桌面与串流客户端"
)]
struct Cli {
    #[arg(long, hide = true)]
    bootstrap_version: bool,
    /// 配置档名称，隔离身份、配置和凭据（开发时多实例并行）；未指定时读取 FARWING_PROFILE，否则为 default
    #[arg(long, global = true, value_name = "NAME")]
    profile: Option<String>,
    /// 日志过滤器，例如 info、debug、trace 或 farwing=trace
    #[arg(long, global = true)]
    log_level: Option<String>,
    /// 指定诊断日志文件（默认写入用户日志目录并自动轮转）
    #[arg(long, global = true)]
    log_file: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    #[command(hide = true)]
    WolConfigure {
        #[arg(long)]
        interface: String,
        #[arg(long)]
        mac: String,
    },
    /// 使用当前程序包更新已安装版本
    Update {
        /// 不显示更新窗口；需要管理员权限时仍请求 Windows UAC
        #[arg(long)]
        silent: bool,
        /// 禁止请求提权，权限不足时返回 740（用于无人值守脚本）
        #[arg(long, requires = "silent")]
        no_elevate: bool,
    },
    #[command(hide = true)]
    Notification { uri: String },
    /// 打开卸载窗口，选择是否保留驱动和本机数据
    Uninstall {
        #[arg(long, hide = true)]
        parent: Option<u32>,
    },
    #[command(hide = true)]
    Service,
    #[command(hide = true)]
    HostResident {
        #[arg(long)]
        parent: u32,
    },
    #[command(hide = true)]
    InputAgent {
        #[arg(long)]
        pipe: String,
        #[arg(long)]
        parent: u32,
    },
    #[command(hide = true)]
    CaptureAgent {
        #[arg(long)]
        pipe: String,
        #[arg(long)]
        parent: u32,
    },
    #[command(hide = true)]
    Component {
        #[arg(value_enum)]
        component: farwing::application::ComponentKind,
        #[arg(value_enum)]
        operation: farwing::application::ComponentOperation,
        #[arg(long)]
        allow_sas: bool,
        #[arg(long)]
        owner: Option<String>,
        #[command(flatten)]
        removal: farwing::application::RemovalOptions,
    },
    #[command(hide = true)]
    DisplayRecovery { token: String },
    #[command(hide = true)]
    PluginVideoHost,
    #[command(hide = true)]
    EncoderProbe,

    #[command(hide = true)]
    PluginHost { manifest: PathBuf },
    /// 打开完整图形设备中心
    Gui {
        #[arg(long)]
        background: bool,
        /// 初始码流帧率：auto、144、90、60 或 30
        #[arg(long)]
        fps: Option<media::FrameRateChoice>,
        /// 格式：auto、prefer-av1/prefer-hevc/prefer-h264；h264/h265/av1 为仅使用
        #[arg(long)]
        codec: Option<media::CodecPreference>,
        /// 是否优先使用平台原生硬件解码器
        #[arg(long, action = clap::ArgAction::Set)]
        hardware_decode: Option<bool>,
        /// 传输策略：auto、p2p 或 relay
        #[arg(long)]
        transport: Option<media::TransportChoice>,
    },
    /// 设置或查看本 profile 使用的 Farwing 服务器
    Server {
        #[command(subcommand)]
        action: ServerAction,
    },
    /// 使用服务器账号登录（账号由服务器管理员创建）
    Login {
        /// 服务器地址；省略时使用已保存的服务器
        #[arg(long)]
        server: Option<String>,
        /// 用户名；省略时交互输入
        #[arg(long)]
        username: Option<String>,
        /// 从标准输入读取密码（一行）
        #[arg(long)]
        password_stdin: bool,
    },
    /// 退出登录并从账号移除本机设备
    Logout,
    /// 无窗口运行被控端：保持在线并接受连接，按 Ctrl+C 退出
    Host {
        /// 开启“允许被控”并保存
        #[arg(long, conflicts_with = "deny")]
        allow: bool,
        /// 关闭“允许被控”并保存
        #[arg(long)]
        deny: bool,
        /// 同时允许被控端端口转发并保存
        #[arg(long)]
        allow_ports: bool,
        /// 开启访问码（临时访问码，8–32 位，须同时含字母和数字）并保存
        #[arg(long)]
        access_code: Option<String>,
        /// 不登录、不连接服务器，只接受局域网与地址直连（受信任设备、访问码）
        #[arg(long)]
        serverless: bool,
        /// 编码方式：auto（自动）、hardware（只用显卡）或 software（只用软件 H.264），并保存
        #[arg(long, conflicts_with = "serverless")]
        encoder: Option<String>,
    },
    /// 显示本机的设备 ID 与设备公钥（信任本机时对方需要）
    Id,
    /// 发现局域网中的 Farwing 被控端
    Lan {
        /// 等待应答的秒数
        #[arg(long, default_value_t = 2)]
        seconds: u64,
    },
    /// 管理本机信任的设备（无需访问码即可连接本机）
    Trusted {
        #[command(subcommand)]
        action: TrustedAction,
    },
    /// 以适合脚本处理的文本格式列出设备
    Devices,
    /// 打开指定设备的观看窗口，关闭窗口或按 Ctrl+C 退出
    Connect {
        /// 设备的完整名称（必须唯一且完全匹配）
        #[arg(required_unless_present_any = ["device_id", "direct"], conflicts_with_all = ["device_id", "direct"])]
        device: Option<String>,
        /// 按已核实的设备 ID 选择目标，避免重复或变化的别名选错设备
        #[arg(long, conflicts_with = "direct")]
        device_id: Option<String>,
        /// 不经服务器，直接连接此地址的被控端：主机名或 IP[:端口]
        #[arg(long)]
        direct: Option<String>,
        /// 直连时使用对方的访问码
        #[arg(long, requires = "direct")]
        code: Option<String>,
        /// 直连时请对方在被控端确认（不用访问码）
        #[arg(long, requires = "direct", conflicts_with = "code")]
        ask: bool,
        /// 静音启动，只影响本地播放
        #[arg(long)]
        mute: bool,
        /// 仅接收远端声音，不显示或控制画面
        #[arg(long)]
        audio_only: bool,
        /// 码流帧率：auto、144、90、60 或 30
        #[arg(long)]
        fps: Option<media::FrameRateChoice>,
        /// 格式：auto、prefer-av1/prefer-hevc/prefer-h264；h264/h265/av1 为仅使用
        #[arg(long)]
        codec: Option<media::CodecPreference>,
        /// 是否优先使用平台原生硬件解码器
        #[arg(long, action = clap::ArgAction::Set)]
        hardware_decode: Option<bool>,
        /// 传输策略：auto、p2p 或 relay
        #[arg(long)]
        transport: Option<media::TransportChoice>,
        /// 测量用：在本机收包处注入丢包与延迟（规则同 dev video --loss）
        #[arg(long, hide = true)]
        dev_loss: Option<String>,
        /// 测量用：不声明 video.recovery，丢包后只用关键帧恢复
        #[arg(long, hide = true)]
        dev_no_recovery: bool,
        /// 测量用：以观影模式（true）或非观影模式（false）开始，不保存
        #[arg(long, hide = true, action = clap::ArgAction::Set)]
        dev_cinema: Option<bool>,
        /// 测量用：自动画质从这一档开始（2–4），不使用也不保存记住的档位
        #[arg(long, hide = true, value_parser = clap::value_parser!(i32).range(2..=4))]
        dev_auto_quality: Option<i32>,
    },
    /// 经服务器连接一台设备（短号或设备 ID），打开观看窗口；无需登录
    Assist {
        /// 对方的 9 位短号或 16 位设备 ID
        target: String,
        /// 对方的访问码；对方信任本机或只要求确认时可省略
        #[arg(long)]
        code: Option<String>,
        /// 静音启动，只影响本地播放
        #[arg(long)]
        mute: bool,
    },
    /// 好友：添加、处理申请、删除
    Friend {
        #[command(subcommand)]
        action: FriendAction,
    },
    /// 查看或设置给好友的权限
    Grant {
        #[command(subcommand)]
        action: GrantAction,
    },
    /// 查看或注销本账号在各设备上的登录
    Sessions {
        /// 注销该设备上的登录
        #[arg(long)]
        revoke: Option<String>,
    },
    /// 开发验证：不打开窗口，直接调用功能服务
    #[command(hide = true)]
    Dev {
        #[command(subcommand)]
        action: DevAction,
    },
}

#[derive(Subcommand)]
enum DevAction {
    /// 通过端口转发服务运行一条规则并输出状态
    Ports {
        #[arg(long)]
        device_id: String,
        /// 本地监听地址，例如 127.0.0.1:18080
        #[arg(long)]
        local: std::net::SocketAddr,
        /// 被控端目标，例如 127.0.0.1:8080、[::1]:8080 或 localhost:8080（主机名在被控端解析）
        #[arg(long)]
        remote: String,
        #[arg(long)]
        udp: bool,
        #[arg(long, default_value_t = 30)]
        seconds: u64,
        #[arg(long)]
        transport: Option<media::TransportChoice>,
    },
    /// 向账号设备发送一次重启（或 --shutdown 关机）请求；由被控端许可决定是否执行
    Power {
        #[arg(long)]
        device_id: String,
        #[arg(long)]
        shutdown: bool,
    },
    /// 获取被控端的脱敏诊断包
    Diagnostics {
        #[arg(long)]
        device_id: String,
        #[arg(long, default_value_t = 120)]
        seconds: u64,
        /// 开始后这么多毫秒时取消（验证取消不留下半个诊断包）
        #[arg(long)]
        cancel_after_ms: Option<u64>,
    },
    /// 按脚本向被控端发送键鼠输入（无窗口的真实主控会话，用于验证被控端注入）
    Input {
        /// 账号或好友共享的设备
        #[arg(long, conflicts_with = "direct", required_unless_present = "direct")]
        device_id: Option<String>,
        /// 直连地址（已配对或本账号设备）
        #[arg(long)]
        direct: Option<String>,
        /// 脚本文件，每行一步，见 docs/client-usage.md
        #[arg(long)]
        script: std::path::PathBuf,
        /// 鼠标绝对坐标所在的屏幕编号，默认主屏
        #[arg(long)]
        screen: Option<i32>,
        /// 按键按下到抬起、以及相邻按键之间的毫秒数
        #[arg(long, default_value_t = 40)]
        hold_ms: u64,
        /// 允许 system sas / system lock（会锁定本机桌面）
        #[arg(long)]
        allow_disruptive: bool,
    },
    /// 加载已安装的插件（独立宿主进程、ABI、程序描述），并在显卡上运行画面滤镜示例做检查
    Plugins,
    /// 无窗口解码被控主屏，可注入丢包与延迟，输出恢复时长等 JSON 指标
    Video {
        #[arg(long)]
        device_id: String,
        #[arg(long, default_value_t = 60)]
        seconds: u64,
        /// 丢包与延迟规则，用分号分隔：frame:N[,every=M]、burst:起点毫秒+时长毫秒、ge:P,R[,seed=S]、delay:毫秒
        #[arg(long)]
        loss: Option<String>,
        /// 软件解码（默认按本机设置）
        #[arg(long)]
        software: bool,
        /// 格式：h264、h265、av1 或 prefer-*（被控设置了优先格式时以被控为准）
        #[arg(long)]
        codec: Option<media::CodecPreference>,
        /// 不声明 video.recovery：丢包后只用关键帧恢复（A/B 对照）
        #[arg(long)]
        no_recovery: bool,
        /// 恢复帧与关键帧一样等到“修不好”才请求（不提前判定，A/B 对照）
        #[arg(long)]
        recovery_late: bool,
        /// 码流帧率：auto、144、90、60 或 30（默认按本机设置）
        #[arg(long)]
        fps: Option<media::FrameRateChoice>,
    },
    /// 端到端检查剪贴板（主控为内存剪贴板；会改写本机 Windows 剪贴板）
    Clipboard {
        #[arg(long)]
        device_id: String,
        /// 存放临时测试文件的目录
        #[arg(long)]
        scratch: std::path::PathBuf,
        /// 本参与者没有自动同步（多人会话且无键鼠控制权）：只检查手动获取
        #[arg(long)]
        pull_only: bool,
    },
    /// 浏览被控端目录，可选下载/上传一项
    Files {
        #[arg(long)]
        device_id: String,
        /// 要浏览的远端目录
        #[arg(long)]
        list: String,
        /// 下载：远端路径
        #[arg(long, requires = "to")]
        download: Option<String>,
        /// 下载保存到的本地目录
        #[arg(long)]
        to: Option<String>,
        /// 上传：本地路径
        #[arg(long, requires = "remote_dir")]
        upload: Option<String>,
        /// 上传到的远端目录
        #[arg(long)]
        remote_dir: Option<String>,
        /// 同名处理：overwrite（覆盖）、rename（保留两份）或 skip（跳过）
        #[arg(long, default_value = "rename")]
        policy: String,
        #[arg(long, default_value_t = 60)]
        seconds: u64,
    },
}

#[derive(Subcommand)]
enum FriendAction {
    /// 列出好友与待处理的申请
    List,
    /// 按用户名发送好友申请（对方已向你申请时直接成为好友）
    Add { username: String },
    /// 接受收到的申请
    Accept { request_id: String },
    /// 拒绝收到的申请
    Reject { request_id: String },
    /// 撤回发出的申请
    Cancel { request_id: String },
    /// 解除好友关系（同时撤销双方的授权并结束相关会话）
    Remove { user_id: String },
}

#[derive(Subcommand)]
enum GrantAction {
    /// 显示给某位好友的授权
    Show { user_id: String },
    /// 设置给某位好友的授权；能力用逗号分隔，空字符串表示不授予
    Set {
        user_id: String,
        /// 默认授予（适用于没有单独设置的所有设备），例如 view,control,clipboard
        #[arg(long)]
        default: Option<String>,
        /// 单独设置某台设备：设备ID=能力列表，可重复
        #[arg(long = "device")]
        devices: Vec<String>,
        /// 删除某台设备的单独设置，可重复
        #[arg(long = "clear-device")]
        clear: Vec<String>,
    },
}

#[derive(Subcommand)]
enum TrustedAction {
    /// 列出本机信任的设备，以及信任本机的设备
    List,
    /// 信任一台设备（对方用 `Farwing id` 查看设备 ID 与公钥）
    Add {
        /// 对方的设备 ID
        device_id: String,
        /// 对方的设备公钥
        #[arg(long)]
        key: String,
        /// 显示名称
        #[arg(long, default_value = "")]
        name: String,
        /// 对方可以做什么，逗号分隔；默认为访客权限
        #[arg(long)]
        grants: Option<String>,
    },
    /// 不再信任一台设备（它与本机的现有连接会断开）
    Remove { device_id: String },
    /// 设置一台受信任设备可以在本机做什么，能力用逗号分隔
    Grant { device_id: String, grants: String },
}

#[derive(Subcommand)]
enum ServerAction {
    /// 保存服务器地址（可选固定证书指纹或导入自定义 CA）
    Set {
        /// 例如 farwing.example.com:8443 或 https://farwing.example.com
        address: String,
        /// 自签名证书的 SHA-256 指纹（`server probe` 可查看）
        #[arg(long)]
        pin: Option<String>,
        /// PEM 格式的自定义 CA 证书文件
        #[arg(long)]
        ca: Option<PathBuf>,
    },
    /// 显示当前服务器设置
    Show,
    /// 连接服务器，显示证书指纹与服务器信息（不保存）
    Probe { address: String },
}

fn main() -> Result<()> {
    // The profile selects every per-user directory, credential and named object,
    // so resolve it from the raw arguments before bootstrap, clap or logging run.
    if let Err(error) = product::profile_arg(std::env::args_os().skip(1))
        .and_then(|profile| product::init_profile(profile.as_deref()))
    {
        attach_parent_console();
        return Err(error);
    }
    farwing::application::bootstrap::initialize()?;
    let parsed = Cli::try_parse();

    if !parsed.as_ref().is_ok_and(|cli| {
        matches!(
            &cli.command,
            Some(
                Commands::PluginHost { .. }
                    | Commands::Notification { .. }
                    | Commands::PluginVideoHost
                    | Commands::EncoderProbe
                    | Commands::DisplayRecovery { .. }
                    | Commands::Component { .. }
                    | Commands::Service
                    | Commands::HostResident { .. }
                    | Commands::InputAgent { .. }
                    | Commands::CaptureAgent { .. }
            )
        )
    }) {
        attach_parent_console();
    }
    let cli = parsed.unwrap_or_else(|error| error.exit());
    anyhow::ensure!(
        cli.profile.as_deref().is_none_or(|p| p == product::profile()),
        "--profile 参数解析不一致"
    );
    if cli.bootstrap_version {
        println!("1");
        return Ok(());
    }
    let command = cli.command.unwrap_or(Commands::Gui {
        background: false,
        fps: None,
        codec: None,
        hardware_decode: None,
        transport: None,
    });

    if let Commands::Notification { uri } = &command {
        return app::notification_activation(uri);
    }
    if matches!(command, Commands::Gui { .. }) && farwing::application::route_installed_gui()? {
        farwing::application::bootstrap::handoff();
        return Ok(());
    }
    let _instance = if matches!(command, Commands::Gui { .. }) {
        match if matches!(
            command,
            Commands::Gui {
                background: true,
                ..
            }
        ) {
            app::instance::acquire_background()?
        } else {
            app::instance::acquire()?
        } {
            Some(instance) => Some(instance),
            None => {
                farwing::application::bootstrap::handoff();
                return Ok(());
            }
        }
    } else {
        None
    };
    let _logging = if matches!(command, Commands::Uninstall { .. }) {
        None
    } else {
        Some(logging::init(
            cli.log_level.as_deref(),
            cli.log_file.as_deref(),
        )?)
    };
    tracing::info!(target: "farwing", version = env!("CARGO_PKG_VERSION"), "application started");

    if let Commands::Update { silent, no_elevate } = command {
        if !silent {
            return farwing::application::update_application(false, false).map(|_| ());
        }
        let result = farwing::application::update_application(silent, no_elevate);
        let code = match result {
            Ok(false) => {
                println!("更新完成");
                0
            }
            Ok(true) => {
                println!("更新完成，需要重启 Windows；未自动重启");
                3010
            }
            Err(error) => {
                tracing::error!(error=%format!("{error:#}"), "application update failed");
                eprintln!("更新失败：{error:#}");
                farwing::application::update_error_code(&error)
            }
        };
        drop(_logging);
        std::process::exit(code);
    }

    if let Commands::Component {
        component,
        operation,
        allow_sas,
        owner,
        removal,
    } = command
    {
        let result = farwing::application::component_operation(
            component,
            operation,
            allow_sas,
            owner.as_deref(),
            removal,
        );
        let reboot = match result {
            Ok(reboot) => reboot,
            Err(error) => {
                drop(_logging);
                if let Some(code) = farwing::application::component_error_code(&error) {
                    std::process::exit(code);
                }
                return Err(error);
            }
        };
        drop(_logging);
        if removal.remove_data && !reboot {
            farwing::application::purge_machine_data()?;
        }
        if reboot {
            std::process::exit(3010);
        }
        return Ok(());
    }
    let result = match command {
        Commands::WolConfigure { interface, mac } => {
            farwing::application::configure_wol(interface, mac)
        }
        Commands::Update { .. } => unreachable!(),
        Commands::Uninstall { parent } => farwing::application::uninstall_application(parent),
        Commands::Component { .. } => unreachable!(),
        Commands::Service => farwing::application::host_service(),
        Commands::HostResident { parent } => farwing::application::host_resident(parent),
        Commands::InputAgent { pipe, parent } => farwing::application::input_agent(&pipe, parent),
        Commands::CaptureAgent { pipe, parent } => {
            farwing::application::capture_agent(&pipe, parent)
        }
        Commands::DisplayRecovery { token } => farwing::application::display_recovery(&token),
        Commands::PluginVideoHost => farwing::plugins::video::host(),
        Commands::EncoderProbe => farwing::application::encoder_probe(),
        Commands::Notification { .. } => {
            unreachable!("notification activation is handled before application startup")
        }

        Commands::PluginHost { manifest } => farwing::plugins::host(&manifest),
        Commands::Gui {
            background,
            fps,
            codec,
            hardware_decode,
            transport,
        } => {
            let (mut options, startup_warning) = match media::preferences::load() {
                Ok(options) => (options, None),
                Err(error) => (
                    Default::default(),
                    Some(format!("读取编解码设置失败，暂用默认设置：{error:#}")),
                ),
            };
            if let Some(fps) = fps {
                options.frame_rate = fps;
            }
            if let Some(codec) = codec {
                options.codec = codec;
            }
            if let Some(hardware) = hardware_decode {
                options.decoder.mode =
                    media::selection::DecoderPreference::from_hardware(hardware).mode;
            }
            if let Some(transport) = transport {
                options.transport = transport;
            }
            // A start with the session (to the tray) leaves the probe for later.
            media::prepare_decoder_support(
                options.decoder.mode,
                std::time::Duration::from_secs(if background { 30 } else { 0 }),
            );
            app::run(app::GuiOptions {
                background,
                media: options,
                startup_warning,
            })
        }
        Commands::Server { action } => match action {
            ServerAction::Set { address, pin, ca } => {
                farwing::application::cli::server_set(&address, pin.as_deref(), ca.as_deref())
            }
            ServerAction::Show => farwing::application::cli::server_show(),
            ServerAction::Probe { address } => tokio::runtime::Runtime::new()?
                .block_on(farwing::application::cli::server_probe(&address)),
        },
        Commands::Login {
            server,
            username,
            password_stdin,
        } => {
            let outcome = tokio::runtime::Runtime::new()?.block_on(login::interactive_login(
                server,
                username,
                password_stdin,
            ))?;
            println!(
                "{}",
                match outcome {
                    login::LoginOutcome::Restored => "已恢复保存的登录",
                    login::LoginOutcome::New => "登录成功",
                }
            );
            Ok(())
        }
        Commands::Logout => {
            tokio::runtime::Runtime::new()?.block_on(farwing::application::cli::logout())
        }
        Commands::Host {
            allow,
            deny,
            allow_ports,
            access_code,
            serverless,
            encoder,
        } => {
            let allow = if allow {
                Some(true)
            } else if deny {
                Some(false)
            } else {
                None
            };
            let runtime = tokio::runtime::Runtime::new()?;
            if serverless {
                runtime.block_on(farwing::application::cli::host_serverless(
                    allow,
                    allow_ports,
                    access_code,
                ))
            } else {
                runtime.block_on(farwing::application::cli::host(
                    allow,
                    allow_ports,
                    access_code,
                    encoder.as_deref(),
                ))
            }
        }
        Commands::Id => farwing::application::cli::id(),
        Commands::Lan { seconds } => tokio::runtime::Runtime::new()?
            .block_on(farwing::application::cli::lan(seconds)),
        Commands::Trusted { action } => match action {
            TrustedAction::List => farwing::application::cli::trusted_list(),
            TrustedAction::Add {
                device_id,
                key,
                name,
                grants,
            } => farwing::application::cli::trusted_add(&device_id, &key, &name, grants.as_deref()),
            TrustedAction::Remove { device_id } => {
                farwing::application::cli::trusted_remove(&device_id)
            }
            TrustedAction::Grant { device_id, grants } => {
                farwing::application::cli::trusted_grant(&device_id, &grants)
            }
        },
        Commands::Assist { target, code, mute } => {
            let mut options = media::preferences::load()?;
            options.muted = mute;
            tokio::runtime::Runtime::new()?.block_on(farwing::application::cli::assist(
                &target, code, options,
            ))
        }
        Commands::Dev { action } => {
            let mut options = media::preferences::load()?;
            let runtime = tokio::runtime::Runtime::new()?;
            match action {
                DevAction::Ports {
                    device_id,
                    local,
                    remote,
                    udp,
                    seconds,
                    transport,
                } => {
                    if let Some(transport) = transport {
                        options.transport = transport;
                    }
                    runtime.block_on(farwing::application::cli::dev_ports(
                        &device_id, local, &remote, udp, seconds, options,
                    ))
                }
                DevAction::Diagnostics {
                    device_id,
                    seconds,
                    cancel_after_ms,
                } => runtime.block_on(farwing::application::cli::dev_diagnostics(
                    &device_id,
                    seconds,
                    cancel_after_ms,
                    options,
                )),
                DevAction::Input {
                    device_id,
                    direct,
                    script,
                    screen,
                    hold_ms,
                    allow_disruptive,
                } => runtime.block_on(farwing::application::cli::dev_input(
                    device_id.as_deref(),
                    direct.as_deref(),
                    &script,
                    screen,
                    hold_ms,
                    allow_disruptive,
                    options,
                )),
                DevAction::Clipboard {
                    device_id,
                    scratch,
                    pull_only,
                } => runtime.block_on(farwing::application::cli::dev_clipboard(
                    &device_id, scratch, pull_only, options,
                )),
                DevAction::Video {
                    device_id,
                    seconds,
                    loss,
                    software,
                    codec,
                    no_recovery,
                    recovery_late,
                    fps,
                } => {
                    if let Some(fps) = fps {
                        options.frame_rate = fps;
                    }
                    if software {
                        options.decoder.mode = media::selection::ProcessingMode::Software;
                    }
                    if let Some(codec) = codec {
                        options.codec = codec;
                    }
                    if no_recovery {
                        farwing::application::cli::dev_video_without_recovery();
                    }
                    if recovery_late {
                        farwing::application::cli::dev_video_late_recovery();
                    }
                    runtime.block_on(farwing::application::cli::dev_video(
                        &device_id,
                        seconds,
                        loss.as_deref(),
                        options,
                    ))
                }
                DevAction::Plugins => farwing::application::cli::dev_plugins(),
                DevAction::Power {
                    device_id,
                    shutdown,
                } => runtime.block_on(farwing::application::cli::dev_power(&device_id, shutdown)),
                DevAction::Files {
                    device_id,
                    list,
                    download,
                    to,
                    upload,
                    remote_dir,
                    policy,
                    seconds,
                } => runtime.block_on(farwing::application::cli::dev_files(
                    &device_id,
                    &list,
                    download.zip(to),
                    upload.zip(remote_dir),
                    &policy,
                    seconds,
                    options,
                )),
            }
        }
        Commands::Devices => tokio::runtime::Runtime::new()?.block_on(print_devices()),
        Commands::Friend { action } => {
            use farwing::application::cli::FriendCommand as C;
            let command = match action {
                FriendAction::List => C::List,
                FriendAction::Add { username } => C::Add(username),
                FriendAction::Accept { request_id } => C::Answer(request_id, true),
                FriendAction::Reject { request_id } => C::Answer(request_id, false),
                FriendAction::Cancel { request_id } => C::Cancel(request_id),
                FriendAction::Remove { user_id } => C::Remove(user_id),
            };
            tokio::runtime::Runtime::new()?.block_on(farwing::application::cli::friend(command))
        }
        Commands::Grant { action } => {
            let runtime = tokio::runtime::Runtime::new()?;
            match action {
                GrantAction::Show { user_id } => {
                    runtime.block_on(farwing::application::cli::grant_show(&user_id))
                }
                GrantAction::Set {
                    user_id,
                    default,
                    devices,
                    clear,
                } => runtime.block_on(farwing::application::cli::grant_set(
                    &user_id, default, devices, clear,
                )),
            }
        }
        Commands::Sessions { revoke } => tokio::runtime::Runtime::new()?
            .block_on(farwing::application::cli::auth_sessions(revoke)),
        Commands::Connect {
            device,
            device_id,
            direct,
            code,
            ask,
            mute,
            audio_only,
            fps,
            codec,
            hardware_decode,
            transport,
            dev_loss,
            dev_no_recovery,
            dev_cinema,
            dev_auto_quality,
        } => {
            if let Some(cinema) = dev_cinema {
                farwing::application::cli::dev_cinema(cinema);
            }
            if let Some(quality) = dev_auto_quality {
                farwing::application::cli::dev_auto_quality(quality);
            }
            if let Some(spec) = dev_loss.as_deref() {
                farwing::application::cli::dev_impair(spec)?;
            }
            if dev_no_recovery {
                farwing::application::cli::dev_video_without_recovery();
            }
            let mut options = media::preferences::load()?;
            options.audio_only = audio_only;
            options.muted = mute;
            if let Some(fps) = fps {
                options.frame_rate = fps;
            }
            if let Some(codec) = codec {
                options.codec = codec;
            }
            if let Some(hardware) = hardware_decode {
                options.decoder.mode =
                    media::selection::DecoderPreference::from_hardware(hardware).mode;
            }
            if let Some(transport) = transport {
                options.transport = transport;
            }
            media::prepare_decoder_support(options.decoder.mode, std::time::Duration::ZERO);
            if let Some(address) = direct {
                let request = controller::DirectRequest::parse(&address, code, ask)?;
                tokio::runtime::Runtime::new()?
                    .block_on(controller::run_direct_viewer_window(request, options))
            } else {
                tokio::runtime::Runtime::new()?.block_on(connect_device(device, options, device_id))
            }
        }
    };
    if let Err(error) = &result {
        tracing::error!(target: "farwing", error = %format_args!("{error:#}"), "application stopped with an error");
    }
    drop(_instance);
    if result.is_ok() && farwing::application::take_installed_handoff() {
        farwing::application::launch_installed(std::env::args_os().skip(1))?;
    }
    result
}

fn attach_parent_console() {
    use windows::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};

    // Attach before printing clap output. Explorer has no parent console;
    // inherited STARTF_USESTDHANDLES pipes/files remain redirected on attach.
    // Never allocate a console just for launching the device center or viewer.
    let _ = unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
}

async fn connect_device(
    device: Option<String>,
    options: media::ConnectionMediaOptions,
    device_id: Option<String>,
) -> Result<()> {
    controller::run_saved_viewer_window(
        device.unwrap_or_else(|| device_id.clone().expect("clap requires a target")),
        options,
        device_id,
    )
    .await
}

async fn print_devices() -> Result<()> {
    let client = AuthenticatedClient::from_saved_session()?;
    let devices = client.list_devices().await;
    client.close().await;
    let devices = devices?;

    println!("本机设备：");
    print_device(None, &devices.current_device);
    println!("\n我的设备：");
    for (index, device) in devices.devices.iter().enumerate() {
        print_device(Some(index + 1), device);
    }
    if devices.devices.is_empty() {
        println!("  （无）");
    }
    if !devices.shared.is_empty() {
        println!("\n好友设备：");
        for (index, device) in devices.shared.iter().enumerate() {
            print_device(Some(index + 1), device);
            println!(
                "      所有者 {}，授权 {}",
                device
                    .owner
                    .as_ref()
                    .map_or("未知", |owner| owner.display_name()),
                if device.granted.is_empty() {
                    "（无）".to_owned()
                } else {
                    device.granted.join(",")
                }
            );
        }
    }
    Ok(())
}

fn print_device(index: Option<usize>, device: &api::DeviceInfo) {
    let prefix = index.map_or_else(|| "  ".to_owned(), |value| format!("  [{value}] "));
    println!(
        "{prefix}{} [{}] — {}，{}，可控 {}，会话参与者 {}",
        device.alias,
        device.device_id,
        device.status_label(),
        device.platform_label(),
        if device.controlled_support && device.controllable {
            "是"
        } else {
            "否"
        },
        device.participant_count()
    );
}
