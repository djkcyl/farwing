//! Local and service execution share the same engine and ownership contract.
use super::{
    broker::Remote,
    engine::{Engine, Geometry},
    wire::Event,
};
use crate::i18n::t;
use crate::platform::windows::host_service::{process, resident};
use anyhow::Result;

pub(super) struct Native {
    engine: Engine,
    // Present only in the already verified service-owned resident. The GUI
    // cannot select this authority by filename, configuration or elevation.
    session: Option<NativeSession>,
}
#[derive(Clone, Copy)]
struct NativeSession {
    id: u32,
    hardware: bool,
}
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
    fn check(&self) -> Result<()> {
        anyhow::ensure!(
            self.session.is_none_or(NativeSession::current),
            "{}",
            t().features.input_windows_session_ended
        );
        Ok(())
    }
}
pub(super) enum Backend {
    Local(Native),
    Service(Remote),
}
impl Backend {
    pub fn new(require_service: bool, permitted: impl Fn() -> bool) -> Result<Self> {
        if resident::is_owner() {
            let session = process::session(std::process::id())?;
            let hardware = session
                == unsafe { windows::Win32::System::RemoteDesktop::WTSGetActiveConsoleSessionId() };
            anyhow::ensure!(
                permitted() && session == process::active_session(),
                "{}",
                t().features.input_session_ended
            );
            return Ok(Self::Local(Native {
                engine: Engine::new(hardware, std::process::id(), true)?,
                session: Some(NativeSession {
                    id: session,
                    hardware,
                }),
            }));
        }
        Ok(match Remote::connect(permitted)? {
            Some(remote) => Self::Service(remote),
            None => {
                anyhow::ensure!(
                    !require_service,
                    "{}",
                    t().features.input_service_not_restored
                );
                Self::Local(Native {
                    engine: Engine::new(false, std::process::id(), false)?,
                    session: None,
                })
            }
        })
    }
    pub fn service(&self) -> bool {
        matches!(
            self,
            Self::Service(_)
                | Self::Local(Native {
                    session: Some(_),
                    ..
                })
        )
    }
    pub fn healthy(&self) -> bool {
        match self {
            Self::Local(native) => native.session.is_none_or(NativeSession::current),
            Self::Service(remote) => remote.healthy(),
        }
    }
    pub fn backend(&self) -> &str {
        match self {
            Self::Local(e) => e.engine.backend(),
            Self::Service(e) => e.backend(),
        }
    }
    /// `hold` is the owner's held-key timeout.
    pub fn apply(
        &mut self,
        events: Vec<Event>,
        geometry: Geometry,
        hold: std::time::Duration,
        permitted: impl Fn() -> bool,
    ) -> Result<()> {
        match self {
            Self::Local(e) => {
                e.check()?;
                let session = e.session;
                let allowed = || permitted() && session.is_none_or(NativeSession::current);
                e.engine.hold_timeout(hold);
                for event in events {
                    e.engine.apply(event, geometry.clone(), &allowed)?;
                    if e.engine.take_sas() {
                        anyhow::ensure!(
                            session.is_some(),
                            "{}",
                            t().features.input_sas_needs_service
                        );
                        super::broker::secure_attention(&allowed)?;
                    }
                }
                Ok(())
            }
            Self::Service(e) => e.apply(events, geometry, hold, permitted),
        }
    }
    pub fn release(&mut self) -> Result<()> {
        match self {
            Self::Local(e) => e.engine.release(),
            Self::Service(e) => e.release(),
        }
    }
    pub fn synchronize(&mut self, geometry: Geometry) -> Result<()> {
        match self {
            Self::Local(e) => {
                e.check()?;
                e.engine.synchronize(geometry)
            }
            Self::Service(e) => e.synchronize(geometry),
        }
    }
    pub fn tick(&mut self) -> Result<()> {
        match self {
            Self::Local(e) => {
                e.check()?;
                e.engine.tick()
            }
            Self::Service(e) => e.tick(),
        }
    }
}
