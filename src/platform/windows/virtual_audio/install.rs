//! Optional audio component, using the common PnP/installation transaction.
use crate::i18n::t;
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
            Ok(Some(reason)) => (t().system.driver_audio_unavailable)(&reason),
            Ok(None) => t().system.driver_audio_interface_unavailable.into(),
            Err(status) => (t().system.driver_audio_status_failed)(&format!("{status:#}")),
        }
    } else if error.code() == ERROR_ACCESS_DENIED.to_hresult() {
        t().system.driver_audio_access_denied.into()
    } else if error.code() == ERROR_SHARING_VIOLATION.to_hresult() {
        t().system.driver_audio_in_use_elsewhere.into()
    } else {
        t().system.driver_audio_open_failed.into()
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
                    "{}",
                    t().system.driver_audio_in_use
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
                ensure!(
                    missing,
                    "{}",
                    (t().system.driver_audio_unavailable_or_busy)(&format!("{error:#}"))
                );
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
        f.write_str(t().system.driver_audio_test_signing)
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
            let mut defaults = super::defaults::Defaults::snapshot()
                .context(t().system.driver_audio_save_defaults_failed)?;
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
                        Err(removal) => Err(error.context((t().system.driver_audio_rollback_failed)(&format!(
                            "{removal:#}"
                        )))),
                    }
                }
                result => result.context(t().system.driver_audio_install_failed),
            }
            .and_then(|reboot| {
                if !reboot {
                    defaults.wait_for_install()?;
                }
                Ok(reboot)
            });
            let restored = defaults
                .restore()
                .context(t().system.driver_audio_restore_defaults_failed);
            match (installed, restored) {
                (Err(error), _) => Err(error),
                (Ok(_), Err(error)) => Err(error),
                (Ok(reboot), Ok(())) => Ok(reboot),
            }
        }
        Operation::Uninstall => {
            super::Defaults::recover_defaults()
                .context(t().system.component_restore_audio_before_uninstall_failed)?;
            PACKAGE.removal()?.execute()
        }
    }
}
