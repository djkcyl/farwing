//! Display-specific ownership and occupancy; package mechanics are shared.
use super::virtual_driver::Driver;
use crate::platform::windows::components::{
    Kind, Operation, Status,
    driver::{Package, Removal},
};
use anyhow::{Context, Result, ensure};
use windows::core::GUID;
include!(concat!(env!("OUT_DIR"), "/display_driver.rs"));
const PACKAGE: Package = Package {
    kind: Kind::DisplayDriver,
    class: GUID::from_u128(0x4D36E968_E325_11CE_BFC1_08002BE10318),
    hardware: r"ROOT\FARWING_DISPLAY",
    node: "FarwingDisplay",
    description: "Farwing Virtual Display Adapter",
    inf: "FarwingDisplay.inf",
    catalog: "FarwingDisplay.cat",
    files: FILES,
    repair: true,
};
#[derive(Debug, thiserror::Error)]
#[error("虚拟显示驱动仍有显示器在使用")]
pub(crate) struct DriverInUse;
pub(crate) fn bundled() -> bool {
    PACKAGE.bundled()
}
pub(crate) fn status() -> Result<Status> {
    let mut status = PACKAGE.status()?;
    if status.ready && Driver::device_instance().is_err() {
        status.label = "已安装，显示接口未就绪".into();
        status.ready = false;
    }
    Ok(status)
}
pub(crate) fn preflight(operation: Operation) -> Result<()> {
    let current = status()?;
    // Updating only the application/service leaves a current driver and its
    // persistent screen intact. Occupancy matters only if we replace/remove it.
    if operation == Operation::Install && current.ready {
        return Ok(());
    }
    if operation == Operation::Uninstall || current.installed {
        drop(removal_plan()?);
    }
    Ok(())
}
pub(crate) fn install() -> Result<bool> {
    let _serial = super::recovery::Serial::acquire()?;
    if status()?.ready {
        return Ok(false);
    }
    preflight(Operation::Install)?;
    PACKAGE.install()
}
fn removal_plan() -> Result<Removal> {
    let plan = PACKAGE.removal()?;
    if let Some(instance) = plan.instance()? {
        for target in super::topology::Topology::query(false)?
            .targets()?
            .iter()
            .filter(|t| t.available)
        {
            let path = target
                .adapter_path
                .trim_start_matches(r"\\?\")
                .trim_start_matches(r"\??\");
            let owner = path.split("#{").next().unwrap_or(path).replace('#', r"\");
            if owner.eq_ignore_ascii_case(&instance) {
                return Err(DriverInUse.into());
            }
        }
        if Driver::device_instance().is_ok() {
            drop(Driver::open().context("虚拟显示驱动正在使用中，无法卸载")?);
        }
    }
    Ok(plan)
}
pub(crate) fn uninstall() -> Result<bool> {
    let _serial = super::recovery::Serial::acquire()?;
    let reboot = removal_plan()?.execute()?;
    if !reboot {
        ensure!(!status()?.removable, "卸载尚未完成");
    }
    Ok(reboot)
}
