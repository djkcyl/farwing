//! Optional audio component, using the common PnP/installation transaction.
use crate::platform::windows::components::{Kind, Operation, Status, driver::Package};
use anyhow::{Context, Result, ensure};
use windows::core::GUID;
include!(concat!(env!("OUT_DIR"), "/audio_driver.rs"));
const PACKAGE: Package = Package {
    kind: Kind::AudioDriver,
    class: GUID::from_u128(0x4d36e96c_e325_11ce_bfc1_08002be10318),
    hardware: r"ROOT\FARWING_AUDIO",
    node: "FarwingAudio",
    description: "Farwing Virtual Audio",
    inf: "FarwingAudio.inf",
    catalog: "FarwingAudio.cat",
    files: FILES,
    repair: true,
};
pub(crate) fn status() -> Result<Status> {
    PACKAGE.status()
}
pub(crate) fn needs_package_update() -> Result<bool> {
    PACKAGE.needs_package_update()
}
pub(crate) fn bridge_open_error(error: windows::core::Error) -> anyhow::Error {
    use windows::Win32::Foundation::{
        ERROR_ACCESS_DENIED, ERROR_DEV_NOT_EXIST, ERROR_DEVICE_NOT_CONNECTED, ERROR_FILE_NOT_FOUND,
        ERROR_PATH_NOT_FOUND, ERROR_SHARING_VIOLATION,
    };
    let missing = [
        ERROR_FILE_NOT_FOUND,
        ERROR_PATH_NOT_FOUND,
        ERROR_DEV_NOT_EXIST,
        ERROR_DEVICE_NOT_CONNECTED,
    ]
    .iter()
    .any(|code| error.code() == code.to_hresult());
    let reason = if missing {
        match PACKAGE.unavailable_reason() {
            Ok(Some(reason)) => format!("Farwing 虚拟声卡不可用：{reason}"),
            Ok(None) => "Farwing 虚拟声卡接口不可用".into(),
            Err(status) => format!("Farwing 虚拟声卡接口不可用，设备状态读取失败：{status:#}"),
        }
    } else if error.code() == ERROR_ACCESS_DENIED.to_hresult() {
        "无法访问 Farwing 虚拟声卡".into()
    } else if error.code() == ERROR_SHARING_VIOLATION.to_hresult() {
        "Farwing 虚拟声卡正被其他会话或进程使用".into()
    } else {
        "无法打开 Farwing 虚拟声卡接口".into()
    };
    // Preserve the original Windows error for policy classification and the
    // installer's occupancy guard; the diagnostic text is not an error code.
    anyhow::Error::new(error).context(reason)
}
fn idle() -> Result<()> {
    if status()?.installed {
        // If another process owns the bridge, open fails and the mutation is
        // refused. Active WASAPI users also prevent removal, even without us.
        match super::Bridge::open() {
            Ok(mut bridge) => {
                let state = bridge.state()?;
                ensure!(
                    state.microphone_running == 0 && state.speaker_running == 0,
                    "虚拟声卡正在使用"
                );
            }
            Err(error) => {
                use windows::Win32::Foundation::{
                    ERROR_DEV_NOT_EXIST, ERROR_DEVICE_NOT_CONNECTED, ERROR_FILE_NOT_FOUND,
                    ERROR_PATH_NOT_FOUND,
                };
                let missing = error
                    .downcast_ref::<windows::core::Error>()
                    .is_some_and(|error| {
                        [
                            ERROR_FILE_NOT_FOUND,
                            ERROR_PATH_NOT_FOUND,
                            ERROR_DEV_NOT_EXIST,
                            ERROR_DEVICE_NOT_CONNECTED,
                        ]
                        .iter()
                        .any(|code| error.code() == code.to_hresult())
                    });
                ensure!(missing, "虚拟声卡接口不可用或正在使用中：{error:#}");
            }
        }
    }
    Ok(())
}
/// Windows refused the test-signed audio driver in this boot (code 52) and
/// the new installation was taken back.
#[derive(Debug)]
pub(crate) struct TestSigningRequired;
impl std::fmt::Display for TestSigningRequired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("虚拟声卡驱动未加载：需要允许测试签名驱动")
    }
}
impl std::error::Error for TestSigningRequired {}

pub(crate) fn preflight() -> Result<()> {
    idle()?;
    // Resolve ownership before other components are stopped or removed.
    drop(PACKAGE.removal()?);
    Ok(())
}
pub(crate) fn execute(operation: Operation) -> Result<bool> {
    idle()?;
    match operation {
        Operation::Install => {
            let mut defaults =
                super::defaults::Defaults::snapshot().context("无法保存安装前的默认音频设备")?;
            let existed = status()?.installed;
            // Windows decides whether this boot permits the test-signed image.
            // TESTSIGNING is not the only supported development route (the
            // Startup Settings one-boot option leaves that BCD setting off).
            // Catalog verification, package ownership and PnP checks still run.
            let installed = match PACKAGE.install() {
                Err(error)
                    if !existed
                        && error
                            .downcast_ref::<crate::platform::windows::components::driver::NotStarted>()
                            .is_some_and(|failure| failure.unsigned()) =>
                {
                    // A device Windows will not start in this boot is of no
                    // use: take the installation back rather than leave it.
                    match PACKAGE.removal().and_then(|removal| removal.execute()) {
                        Ok(_) => Err(TestSigningRequired.into()),
                        Err(removal) => Err(error.context(format!(
                            "虚拟声卡驱动未加载，撤回安装失败：{removal:#}"
                        ))),
                    }
                }
                result => result.context("虚拟声卡安装或加载失败"),
            }
            .and_then(|reboot| {
                if !reboot {
                    defaults.wait_for_install()?;
                }
                Ok(reboot)
            });
            let restored = defaults.restore().context("安装后恢复默认音频设备失败");
            match (installed, restored) {
                (Err(error), _) => Err(error),
                (Ok(_), Err(error)) => Err(error),
                (Ok(reboot), Ok(())) => Ok(reboot),
            }
        }
        Operation::Uninstall => {
            super::Defaults::recover_defaults().context("卸载前恢复默认音频设备失败")?;
            PACKAGE.removal()?.execute()
        }
    }
}
