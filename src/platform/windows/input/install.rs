//! Independent optional HID driver; does not deploy or modify the host service.
use crate::platform::windows::components::{Kind, Status, driver::Package};
use anyhow::Result;
use windows::core::GUID;
include!(concat!(env!("OUT_DIR"), "/input_driver.rs"));

const PACKAGE: Package = Package {
    kind: Kind::InputDriver,
    class: GUID::from_u128(0x4d36e97d_e325_11ce_bfc1_08002be10318),
    hardware: r"ROOT\FARWING_INPUT",
    node: "FarwingInput",
    description: "Farwing Input",
    inf: "FarwingInput.inf",
    catalog: "FarwingInput.cat",
    files: FILES,
    repair: true,
};
pub(crate) fn bundled() -> bool {
    PACKAGE.bundled()
}
pub(crate) fn status() -> Result<Status> {
    PACKAGE.status()
}
pub(crate) fn install() -> Result<bool> {
    PACKAGE.install()
}
pub(crate) fn uninstall() -> Result<bool> {
    let plan = PACKAGE.removal()?;
    plan.execute()
}
