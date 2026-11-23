//! Linux registration metadata. The installation ID and device key come from
//! the credential store, not from a machine identifier.
use anyhow::{Context, Result, ensure};
use std::fs;

pub(crate) struct DeviceProfile {
    pub(crate) name: String,
    pub(crate) os: String,
}

pub(crate) fn computer_name() -> Result<String> {
    let name = fs::read_to_string("/proc/sys/kernel/hostname").context("read Linux hostname")?;
    let name = name.trim();
    ensure!(!name.is_empty(), "Linux hostname is empty");
    Ok(name.to_owned())
}

impl DeviceProfile {
    pub(crate) fn read() -> Result<Self> {
        let name = computer_name()?;

        let kernel = fs::read_to_string("/proc/sys/kernel/osrelease")
            .context("read Linux kernel release")?;
        let kernel = format!("Linux {}", kernel.trim());
        let release = fs::read_to_string("/etc/os-release")
            .or_else(|_| fs::read_to_string("/usr/lib/os-release"));
        let distro = release.as_deref().ok().and_then(|text| {
            text.lines()
                .find_map(|line| line.strip_prefix("PRETTY_NAME="))
                .map(|value| value.trim().trim_matches(['\'', '"']))
                .filter(|value| !value.is_empty() && *value != "Linux")
        });
        let os = match distro {
            Some(distro) => format!("{distro} · {kernel}"),
            None => kernel,
        };
        Ok(Self { name, os })
    }
}
