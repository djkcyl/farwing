//! Single management boundary for the application, service and optional drivers.
pub(crate) mod application;
pub(crate) mod driver;
pub(crate) mod files;
pub(crate) mod suite;
use anyhow::{Context, Result, ensure};
use std::sync::atomic::{AtomicBool, Ordering};
use windows::{
    Win32::{
        Foundation::*,
        System::Threading::*,
        UI::{Shell::*, WindowsAndMessaging::SW_HIDE},
    },
    core::{PCWSTR, w},
};
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Kind {
    Application,
    Suite,
    #[value(skip)]
    HostService,
    #[value(skip)]
    InputDriver,
    #[value(skip)]
    DisplayDriver,
    AudioDriver,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Operation {
    Install,
    Uninstall,
}
#[derive(Clone, Copy, Debug, Default, clap::Args)]
pub struct RemovalOptions {
    #[arg(long)]
    pub remove_display_driver: bool,
    #[arg(long)]
    pub remove_audio_driver: bool,
    #[arg(long)]
    pub remove_data: bool,
}
impl RemovalOptions {
    fn validate(self, kind: Kind, operation: Operation) -> Result<()> {
        ensure!(
            !(self.remove_display_driver || self.remove_audio_driver || self.remove_data)
                || (operation == Operation::Uninstall
                    && matches!(kind, Kind::Suite | Kind::Application)),
            "附加卸载选项只适用于整套卸载"
        );
        ensure!(
            !self.remove_data || kind == Kind::Application,
            "清除数据仅适用于卸载程序"
        );
        Ok(())
    }
    pub(super) fn arguments(self) -> String {
        [
            (self.remove_display_driver, " --remove-display-driver"),
            (self.remove_audio_driver, " --remove-audio-driver"),
            (self.remove_data, " --remove-data"),
        ]
        .into_iter()
        .filter_map(|(enabled, flag)| enabled.then_some(flag))
        .collect()
    }
    pub(super) fn preflight(self) -> Result<()> {
        if self.remove_display_driver {
            super::display::install::preflight(Operation::Uninstall)?;
        }
        if self.remove_audio_driver {
            super::virtual_audio::install::preflight()?;
        }
        Ok(())
    }
    pub(super) fn remove_optional_drivers(self) -> Result<bool> {
        let mut reboot = false;
        if self.remove_audio_driver {
            reboot |= super::virtual_audio::install::execute(Operation::Uninstall)?;
        }
        if self.remove_display_driver {
            reboot |= super::display::install::uninstall()?;
        }
        Ok(reboot)
    }
}
static MAINTENANCE: AtomicBool = AtomicBool::new(false);
pub(crate) fn maintaining() -> bool {
    MAINTENANCE.load(Ordering::Acquire)
}
pub(crate) fn elevated() -> Result<bool> {
    use windows::Win32::Security::*;
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)?;
        let token = super::host_service::pipe::Handle(token);
        let mut elevation = TOKEN_ELEVATION::default();
        let mut length = 0;
        GetTokenInformation(
            token.0,
            TokenElevation,
            Some((&mut elevation as *mut TOKEN_ELEVATION).cast()),
            std::mem::size_of_val(&elevation) as u32,
            &mut length,
        )?;
        Ok(elevation.TokenIsElevated != 0)
    }
}
struct Maintenance;
impl Drop for Maintenance {
    fn drop(&mut self) {
        MAINTENANCE.store(false, Ordering::Release);
    }
}
pub(crate) struct Status {
    pub label: String,
    pub installed: bool,
    pub removable: bool,
    pub ready: bool,
}
include!(concat!(env!("OUT_DIR"), "/driver_certificate.rs"));
impl Kind {
    pub(crate) const ALL: [Self; 4] = [
        Self::HostService,
        Self::InputDriver,
        Self::DisplayDriver,
        Self::AudioDriver,
    ];
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Application => "程序",
            Self::Suite => "服务",
            Self::HostService => "被控服务",
            Self::InputDriver => "输入驱动",
            Self::DisplayDriver => "虚拟显示驱动",
            Self::AudioDriver => "虚拟音频驱动",
        }
    }
    /// (subject, SHA-1 thumbprint, DER bytes) of the bundled driver signing
    /// certificate, if this build embeds one.
    pub(crate) fn certificate(self) -> Option<(&'static str, &'static str, &'static [u8])> {
        static SUBJECT: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        match self {
            Self::HostService | Self::Suite | Self::Application => None,
            Self::InputDriver | Self::DisplayDriver | Self::AudioDriver => {
                let (thumbprint, bytes) = CERTIFICATE?;
                let subject = SUBJECT.get_or_init(|| certificate_subject(bytes));
                Some((subject.as_str(), thumbprint, bytes))
            }
        }
    }
}
/// X.500 subject of the bundled certificate, shown before the user confirms
/// adding it to the machine trust stores.
fn certificate_subject(bytes: &[u8]) -> String {
    use windows::Win32::Security::Cryptography::*;
    unsafe {
        let context = CertCreateCertificateContext(X509_ASN_ENCODING, bytes);
        if context.is_null() {
            return "组件签名证书无效".into();
        }
        let kind = CERT_X500_NAME_STR.0;
        let para = Some(std::ptr::from_ref(&kind).cast());
        let mut name = vec![0u16; 512];
        let length = CertGetNameStringW(context, CERT_NAME_RDN_TYPE, 0, para, Some(&mut name));
        let _ = CertFreeCertificateContext(Some(context));
        String::from_utf16_lossy(&name[..(length as usize).saturating_sub(1).min(name.len())])
    }
}
/// Called only by the elevated, explicitly confirmed installer. Trust the pinned
/// embedded public certificate directly; do not reopen a user-writable temp file.
pub(crate) fn trust_certificate(kind: Kind) -> Result<()> {
    use windows::Win32::Security::Cryptography::*;
    ensure!(
        !matches!(kind, Kind::HostService | Kind::Suite | Kind::Application),
        "被控服务不需要驱动证书"
    );
    // Without a bundled certificate the catalog must chain to a signer
    // Windows already trusts; verify_catalog checks that afterwards.
    let Some((_, expected, bytes)) = kind.certificate() else {
        return Ok(());
    };
    unsafe {
        let context = CertCreateCertificateContext(X509_ASN_ENCODING, bytes);
        ensure!(!context.is_null(), "组件签名证书无效");
        let mut hash = [0u8; 20];
        let mut length = hash.len() as u32;
        let result = CertGetCertificateContextProperty(
            context,
            CERT_SHA1_HASH_PROP_ID,
            Some(hash.as_mut_ptr().cast()),
            &mut length,
        );
        let _ = CertFreeCertificateContext(Some(context));
        result?;
        let actual = hash.iter().map(|b| format!("{b:02X}")).collect::<String>();
        ensure!(
            length == 20 && actual == expected,
            "组件签名证书与内置指纹不匹配"
        );
        for name in [w!("ROOT"), w!("TrustedPublisher")] {
            let store = CertOpenStore(
                CERT_STORE_PROV_SYSTEM_W,
                CERT_QUERY_ENCODING_TYPE(0),
                None,
                CERT_OPEN_STORE_FLAGS(CERT_SYSTEM_STORE_LOCAL_MACHINE)
                    | CERT_STORE_OPEN_EXISTING_FLAG,
                Some(name.as_ptr().cast()),
            )?;
            let result = CertAddEncodedCertificateToStore(
                Some(store),
                X509_ASN_ENCODING,
                bytes,
                CERT_STORE_ADD_USE_EXISTING,
                None,
            );
            let close = CertCloseStore(Some(store), 0);
            result?;
            close?;
        }
    }
    Ok(())
}

pub(crate) fn status(kind: Kind) -> Result<Status> {
    match kind {
        Kind::Application => suite::status(),
        Kind::Suite => suite::status(),
        Kind::HostService => super::host_service::install::status(),
        Kind::InputDriver => super::input::install::status(),
        Kind::DisplayDriver => super::display::install::status(),
        Kind::AudioDriver => super::virtual_audio::install::status(),
    }
}
pub(crate) fn execute(
    kind: Kind,
    operation: Operation,
    allow_sas: bool,
    owner: Option<&str>,
    removal: RemovalOptions,
) -> Result<bool> {
    let result = (|| {
        ensure!(
            matches!(kind, Kind::Application | Kind::Suite | Kind::AudioDriver),
            "请使用统一安装或卸载入口"
        );
        ensure!(
            crate::product::is_default_profile(),
            "非默认 profile 不能安装或卸载服务与驱动"
        );
        ensure!(
            owner.is_none() || (kind == Kind::Suite && operation == Operation::Install),
            "安装用户只适用于安装服务"
        );
        removal.validate(kind, operation)?;
        ensure!(
            (kind == Kind::Suite && operation == Operation::Install) || !allow_sas,
            "SAS策略只属于被控服务"
        );
        let _serial = Serial::acquire()?;
        super::host_service::install::preflight()?;
        match (kind, operation) {
            (Kind::Application, Operation::Uninstall) => application::uninstall(removal),
            (Kind::Application, Operation::Install) => anyhow::bail!("请使用程序安装入口"),
            (Kind::Suite, operation) => suite::execute(operation, owner, allow_sas, removal),
            (Kind::AudioDriver, operation) => super::virtual_audio::install::execute(operation),
            _ => unreachable!("component target checked above"),
        }
    })();
    match &result {
        Ok(reboot) => tracing::info!(?kind, ?operation, reboot, "component operation completed"),
        Err(error) => {
            tracing::error!(?kind, ?operation, error = %format!("{error:#}"), "component operation failed")
        }
    }
    result
}
struct Serial(super::host_service::pipe::Handle);
impl Serial {
    fn acquire() -> Result<Self> {
        // Components are machine-wide, so this lock ignores the profile.
        let name = windows::core::HSTRING::from(crate::product::machine_global_object(
            "ComponentMutation",
        ));
        let handle =
            super::host_service::pipe::Handle(unsafe { CreateMutexW(None, false, &name) }?);
        let result = unsafe { WaitForSingleObject(handle.0, 0) };
        ensure!(
            result == WAIT_OBJECT_0 || result == WAIT_ABANDONED,
            "已有组件操作正在进行"
        );
        Ok(Self(handle))
    }
}
impl Drop for Serial {
    fn drop(&mut self) {
        let _ = unsafe { ReleaseMutex(self.0.0) };
    }
}

pub(crate) fn request(
    kind: Kind,
    operation: Operation,
    allow_sas: bool,
    removal: RemovalOptions,
) -> Result<bool> {
    request_with_resume(kind, operation, allow_sas, removal, true)
}
pub(crate) fn request_update(resume: bool) -> Result<bool> {
    request_with_resume(
        Kind::Suite,
        Operation::Install,
        false,
        Default::default(),
        resume,
    )
}
fn request_with_resume(
    kind: Kind,
    operation: Operation,
    allow_sas: bool,
    removal: RemovalOptions,
    resume: bool,
) -> Result<bool> {
    removal.validate(kind, operation)?;
    ensure!(
        MAINTENANCE
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok(),
        "已有组件操作正在进行"
    );
    let _maintenance = Maintenance;
    if kind == Kind::Application {
        ensure!(operation == Operation::Uninstall, "无效程序操作");
        return application::request_uninstall(removal);
    }
    if kind == Kind::Suite {
        return suite::request(operation, allow_sas, removal, resume);
    }
    if kind == Kind::AudioDriver {
        return elevate(
            &format!(
                "component audio-driver {}",
                if operation == Operation::Install {
                    "install"
                } else {
                    "uninstall"
                }
            ),
            kind,
        );
    }
    anyhow::bail!("请使用统一安装或卸载入口")
}
pub(super) fn elevate(args: &str, kind: Kind) -> Result<bool> {
    let exe = dunce::canonicalize(std::env::current_exe()?)?;
    let exe: Vec<u16> = exe
        .as_os_str()
        .to_string_lossy()
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let args: Vec<u16> = args.encode_utf16().chain(Some(0)).collect();
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_FLAG_NO_UI,
        lpVerb: w!("runas"),
        lpFile: PCWSTR(exe.as_ptr()),
        lpParameters: PCWSTR(args.as_ptr()),
        nShow: SW_HIDE.0,
        ..Default::default()
    };
    unsafe { ShellExecuteExW(&mut info) }.context("组件操作未启动")?;
    let process = super::host_service::pipe::Handle(info.hProcess);
    ensure!(!process.0.is_invalid(), "组件操作进程不可用");
    ensure!(
        unsafe { WaitForSingleObject(process.0, INFINITE) } == WAIT_OBJECT_0,
        "无法等待组件操作完成"
    );
    let mut code = 0;
    unsafe {
        GetExitCodeProcess(process.0, &mut code)?;
    }
    if code == 170 {
        anyhow::bail!("被控服务仍有活动会话");
    }
    if code == 2404 {
        return Err(super::display::install::DriverInUse.into());
    }
    if code == 577 {
        return Err(super::virtual_audio::install::TestSigningRequired.into());
    }
    if code == 32 {
        anyhow::bail!("仍有 Farwing 程序在运行");
    }
    ensure!(
        code == 0 || code == 3010,
        "{}操作失败（退出码 {code}）",
        kind.label()
    );
    Ok(code == 3010)
}
