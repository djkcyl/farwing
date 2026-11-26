//! Remote shutdown and restart through systemd-logind, the session manager
//! every mainstream distribution runs. The request is non-interactive: when
//! polkit wants an administrator's approval for the signed-in user, the host
//! reports that instead of prompting on its own screen.
use anyhow::{Context, Result, bail};
use zbus::blocking::{Connection, Proxy};

fn manager(connection: &Connection) -> Result<Proxy<'static>> {
    Proxy::new(
        connection,
        "org.freedesktop.login1",
        "/org/freedesktop/login1",
        "org.freedesktop.login1.Manager",
    )
    .context("systemd-logind 不可用")
}

fn allowed(manager: &Proxy<'_>, method: &str, what: &str) -> Result<()> {
    let answer: String = manager
        .call(method, &())
        .with_context(|| format!("查询{what}权限失败"))?;
    match answer.as_str() {
        "yes" => Ok(()),
        "challenge" => bail!("{what}需要管理员授权，请在本机 polkit 中允许当前用户"),
        _ => bail!("系统策略不允许当前用户{what}"),
    }
}

pub(crate) fn probe() -> Result<()> {
    let connection = Connection::system().context("连接系统总线失败")?;
    let manager = manager(&connection)?;
    allowed(&manager, "CanPowerOff", "关机")?;
    allowed(&manager, "CanReboot", "重启")
}

pub(crate) fn execute(action: crate::features::host::power::Action) -> Result<()> {
    let connection = Connection::system().context("连接系统总线失败")?;
    let manager = manager(&connection)?;
    let (check, method, what) = match action {
        crate::features::host::power::Action::Shutdown => ("CanPowerOff", "PowerOff", "关机"),
        crate::features::host::power::Action::Reboot => ("CanReboot", "Reboot", "重启"),
    };
    allowed(&manager, check, what)?;
    manager
        .call::<_, _, ()>(method, &(false,))
        .with_context(|| format!("systemd-logind 未接受{what}请求"))
}
