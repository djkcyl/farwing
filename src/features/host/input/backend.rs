//! Local and service execution share the same engine and ownership contract.
#[cfg(windows)]
use super::broker::Remote;
use super::{engine::Engine, geometry::Geometry, wire::Event};
#[cfg(windows)]
use crate::platform::windows::host_service::{process, resident};
use anyhow::Result;

pub(super) struct Native {
    engine: Engine,
    // Present only in the already verified service-owned resident. The GUI
    // cannot select this authority by filename, configuration or elevation.
    #[cfg(windows)]
    session: Option<NativeSession>,
}
#[cfg(windows)]
#[derive(Clone, Copy)]
struct NativeSession {
    id: u32,
    hardware: bool,
}
#[cfg(windows)]
impl NativeSession {
    fn current(self) -> bool {
        process::active_session() == self.id
            && (!self.hardware
                || self.id
                    == unsafe {
                        windows::Win32::System::RemoteDesktop::WTSGetActiveConsoleSessionId()
                    })
    }
}
impl Native {
    /// Whether the session this engine injects into is still the active one.
    /// Linux runs only inside the signed-in desktop, which is always current.
    fn current(&self) -> bool {
        #[cfg(windows)]
        return self.session.is_none_or(NativeSession::current);
        #[cfg(not(windows))]
        true
    }
    /// Whether this engine is the installed service's own resident one.
    fn service(&self) -> bool {
        #[cfg(windows)]
        return self.session.is_some();
        #[cfg(not(windows))]
        false
    }
    fn check(&self) -> Result<()> {
        anyhow::ensure!(self.current(), "输入所属Windows会话已结束");
        Ok(())
    }
}
pub(super) enum Backend {
    Local(Native),
    #[cfg(windows)]
    Service(Remote),
}
impl Backend {
    pub fn new(
        policy: super::wire::Policy,
        require_service: bool,
        permitted: impl Fn() -> bool,
    ) -> Result<Self> {
        #[cfg(windows)]
        {
            if resident::is_owner() {
                let session = process::session(std::process::id())?;
                let hardware = session
                    == unsafe {
                        windows::Win32::System::RemoteDesktop::WTSGetActiveConsoleSessionId()
                    };
                anyhow::ensure!(
                    permitted() && session == process::active_session(),
                    "输入会话已结束"
                );
                return Ok(Self::Local(Native {
                    engine: Engine::new(hardware, policy, std::process::id(), true)?,
                    session: Some(NativeSession {
                        id: session,
                        hardware,
                    }),
                }));
            }
            if let Some(remote) = Remote::connect(policy, &permitted)? {
                return Ok(Self::Service(remote));
            }
        }
        #[cfg(not(windows))]
        let _ = permitted;
        anyhow::ensure!(!require_service, "系统被控服务尚未恢复");
        Ok(Self::Local(Native {
            engine: Engine::new(false, policy, std::process::id(), false)?,
            #[cfg(windows)]
            session: None,
        }))
    }
    pub fn service(&self) -> bool {
        match self {
            Self::Local(native) => native.service(),
            #[cfg(windows)]
            Self::Service(_) => true,
        }
    }
    pub fn healthy(&self) -> bool {
        match self {
            Self::Local(native) => native.current(),
            #[cfg(windows)]
            Self::Service(remote) => remote.healthy(),
        }
    }
    pub fn backend(&self) -> &str {
        match self {
            Self::Local(e) => e.engine.backend(),
            #[cfg(windows)]
            Self::Service(e) => e.backend(),
        }
    }
    pub fn apply(
        &mut self,
        events: Vec<Event>,
        geometry: Geometry,
        permitted: impl Fn() -> bool,
    ) -> Result<()> {
        match self {
            Self::Local(e) => {
                e.check()?;
                #[cfg(windows)]
                let session = e.session;
                #[cfg(windows)]
                let allowed = || permitted() && session.is_none_or(NativeSession::current);
                #[cfg(not(windows))]
                let allowed = &permitted;
                for event in events {
                    e.engine.apply(event, geometry.clone(), &allowed)?;
                    if e.engine.take_sas() {
                        #[cfg(windows)]
                        {
                            anyhow::ensure!(
                                session.is_some(),
                                "Ctrl+Alt+Del需要安装并启用被控服务"
                            );
                            super::broker::secure_attention(&allowed)?;
                        }
                    }
                }
                Ok(())
            }
            #[cfg(windows)]
            Self::Service(e) => e.apply(events, geometry, permitted),
        }
    }
    /// 请合成器重新判定一次指针底下是谁——Wayland 的拖放接取面要靠这一下。只有
    /// Linux 有这条：别处的合成器自己会在面上屏时重判。
    #[cfg(target_os = "linux")]
    pub fn realign_pointer(&mut self) -> Result<bool> {
        match self {
            Self::Local(native) => native.engine.realign_pointer(),
        }
    }
    pub fn release(&mut self) -> Result<()> {
        match self {
            Self::Local(e) => e.engine.release(),
            #[cfg(windows)]
            Self::Service(e) => e.release(),
        }
    }
    pub fn synchronize(&mut self, geometry: Geometry) -> Result<()> {
        match self {
            Self::Local(e) => {
                e.check()?;
                e.engine.synchronize(geometry)
            }
            #[cfg(windows)]
            Self::Service(e) => e.synchronize(geometry),
        }
    }
    pub fn configure(&mut self, configuration: super::config::Configuration) -> Result<()> {
        match self {
            Self::Local(e) => {
                e.check()?;
                e.engine.configure(configuration)
            }
            #[cfg(windows)]
            Self::Service(e) => e.configure(configuration),
        }
    }
    pub fn mouse_policy(&self) -> super::config::MousePolicy {
        match self {
            Self::Local(e) => e.engine.mouse_policy(),
            #[cfg(windows)]
            Self::Service(e) => e.mouse_policy(),
        }
    }
    pub fn tick(&mut self) -> Result<()> {
        match self {
            Self::Local(e) => {
                e.check()?;
                e.engine.tick()
            }
            #[cfg(windows)]
            Self::Service(e) => e.tick(),
        }
    }
}
