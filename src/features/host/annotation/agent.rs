//! Session-owned drawing on the interactive default desktop.
//!
//! Linux has no overlay renderer yet: opening a board is refused with a
//! reason, everything else answers as an idle Windows host would.
#[cfg(windows)]
use super::model::Model;
use super::{Action, Context};
#[cfg(windows)]
use crate::platform::windows::{annotation::Overlay, host_service::process};
use anyhow::Result;
use std::time::Duration;
#[cfg(windows)]
use std::time::Instant;
use tokio_util::sync::CancellationToken;

#[cfg(not(windows))]
#[derive(Default)]
pub(super) struct Backend;
#[cfg(not(windows))]
impl Backend {
    pub fn interval(&self) -> Duration {
        Duration::from_millis(50)
    }
    pub fn exchange(
        &mut self,
        context: Context,
        command: Option<Action>,
        cancel: &CancellationToken,
    ) -> Result<i32> {
        if !context.allowed || cancel.is_cancelled() {
            return Ok(4);
        }
        match command.as_ref().map(Action::toggle) {
            None | Some(Some(false)) => Ok(0),
            Some(Some(true)) => anyhow::bail!("Linux 暂不支持被控端批注（远程画笔）"),
            Some(None) => Ok(4),
        }
    }
}

#[cfg(windows)]
#[derive(Default)]
pub(super) struct Backend {
    local: Option<Local>,
}
#[cfg(windows)]
impl Backend {
    pub fn interval(&self) -> Duration {
        if self.local.as_ref().is_some_and(|l| l.model.animated()) {
            Duration::from_millis(16)
        } else {
            Duration::from_millis(50)
        }
    }
    pub fn exchange(
        &mut self,
        context: Context,
        command: Option<Action>,
        cancel: &CancellationToken,
    ) -> Result<i32> {
        if !context.allowed || cancel.is_cancelled() {
            self.local = None;
            return Ok(4);
        }
        // The installed resident already owns this interactive session. Drawing
        // here retains its UIAccess token without elevating the user backend
        // that performs file and clipboard work. Never draw in Session 0.
        let session = process::session(std::process::id())?;
        if session == 0 || session != process::active_session() {
            self.local = None;
            return Ok(2);
        }
        let opening = command.as_ref().and_then(Action::toggle) == Some(true);
        let closing = command.as_ref().and_then(Action::toggle) == Some(false);
        if self.local.is_none() && !opening {
            return Ok(if closing || command.is_none() { 0 } else { 4 });
        }
        if self.local.is_none() {
            if !logged_on(session) {
                return Ok(2);
            }
            self.local = Some(Local::new()?);
        }
        let code = self
            .local
            .as_mut()
            .unwrap()
            .exchange(context, command.as_ref())?;
        if closing {
            self.local = None;
        }
        Ok(code)
    }
}

#[cfg(windows)]
struct Local {
    model: Model,
    context: Context,
    overlay: Overlay,
    shown: bool,
    revision: u64,
    desktop: Option<(Instant, i32)>,
    last_animation: Instant,
}
#[cfg(windows)]
impl Local {
    fn new() -> Result<Self> {
        Ok(Self {
            model: Model::default(),
            context: Context::default(),
            overlay: Overlay::new()?,
            shown: false,
            revision: u64::MAX,
            desktop: None,
            last_animation: Instant::now(),
        })
    }
    fn exchange(&mut self, context: Context, command: Option<&Action>) -> Result<i32> {
        self.overlay.pump();
        self.model.reconcile(&self.context, &context);
        self.context = context;
        let desktop = match self
            .desktop
            .filter(|(at, _)| command.is_none() && at.elapsed() < Duration::from_millis(50))
        {
            Some((_, code)) => code,
            None => {
                let code = if !logged_on(process::active_session()) {
                    2
                } else if !crate::features::host::clipboard::agent::desktop_available() {
                    3
                } else {
                    0
                };
                self.desktop = Some((Instant::now(), code));
                code
            }
        };
        if !self.context.allowed {
            self.model.clear();
        }
        let code = command.map_or(0, |c| self.model.apply_action(&self.context, c, desktop));
        if desktop != 0 {
            self.model.suspend();
        }
        if desktop == 0 && self.last_animation.elapsed() >= Duration::from_millis(16) {
            let now = Instant::now();
            self.model.tick(now);
            self.last_animation = now;
        }
        let visible = self.model.enabled && self.context.allowed && desktop == 0;
        if !visible {
            self.overlay.hide();
        } else if !self.shown
            || self.revision != self.model.revision
            || self.overlay.needs_refresh()
        {
            if !self.shown {
                self.model
                    .dirty
                    .extend(self.context.screens.iter().map(|s| s.id));
            }
            self.overlay.render(
                &self.context.screens,
                &self.model.strokes,
                &self.model.boards,
                &self.model.dirty,
                Model::screen(&self.context, None),
            )?;
            self.model.dirty.clear();
        }
        if visible {
            self.overlay.maintain_visibility()?;
        }
        self.shown = visible;
        self.revision = self.model.revision;
        Ok(code)
    }
}

#[cfg(windows)]
fn logged_on(session: u32) -> bool {
    use windows::{Win32::System::RemoteDesktop::*, core::PWSTR};
    if session == u32::MAX {
        return false;
    }
    unsafe {
        let mut text = PWSTR::null();
        let mut bytes = 0;
        if WTSQuerySessionInformationW(None, session, WTSUserName, &mut text, &mut bytes).is_err() {
            return false;
        }
        let available = !text.is_null() && bytes > 2 && *text.0 != 0;
        if !text.is_null() {
            WTSFreeMemory(text.0.cast());
        }
        available
    }
}
