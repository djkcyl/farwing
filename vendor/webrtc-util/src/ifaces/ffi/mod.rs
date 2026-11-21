#[cfg(target_family = "windows")]
mod windows;
#[cfg(target_family = "windows")]
pub(crate) use self::windows::native_interfaces;

#[cfg(target_family = "unix")]
mod unix;
use std::io;

use crate::ifaces::Interface;

pub fn ifaces() -> io::Result<Vec<Interface>> {
    native_interfaces(false)
}

#[cfg(target_family = "unix")]
pub(crate) fn native_interfaces(_: bool) -> io::Result<Vec<Interface>> {
    unix::ifaces()
}
