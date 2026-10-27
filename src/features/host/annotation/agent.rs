//! Session-owned drawing on the interactive default desktop.
use super::{Action, Context, model::Model};
use crate::platform::windows::{annotation::Overlay, host_service::process};
use anyhow::Result;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

#[derive(Default)]
pub(super) struct Backend {
    local: Option<Local>,
}
impl Backend {
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
            if !process::logged_on(session) {
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

struct Local {
    model: Model,
    context: Context,
    overlay: Overlay,
    shown: bool,
    revision: u64,
    desktop: Option<(Instant, i32)>,
}
impl Local {
    fn new() -> Result<Self> {
        Ok(Self {
            model: Model::default(),
            context: Context::default(),
            overlay: Overlay::new()?,
            shown: false,
            revision: u64::MAX,
            desktop: None,
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
                let code = if !process::logged_on(process::active_session()) {
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
