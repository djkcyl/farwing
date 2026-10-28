//! Session-owned drawing on the interactive default desktop.
use super::{Context, Outcome, Refusal, model::Model};
use crate::protocol::annotation::Op;
use crate::platform::windows::{annotation::Overlay, host_service::process};
use anyhow::Result;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

#[derive(Default)]
pub(super) struct Backend {
    local: Option<Local>,
}
impl Backend {
    /// Apply `op` (or only keep the overlay current) and show the result.
    pub fn exchange(
        &mut self,
        context: Context,
        op: Option<&Op>,
        cancel: &CancellationToken,
    ) -> Result<Outcome> {
        if !context.allowed || cancel.is_cancelled() {
            self.local = None;
            return Ok(Err(Refusal::NotAllowed));
        }
        // The installed resident already owns this interactive session. Drawing
        // here retains its UIAccess token without elevating the user backend
        // that performs file and clipboard work. Never draw in Session 0.
        let session = process::session(std::process::id())?;
        if session == 0 || session != process::active_session() {
            self.local = None;
            return Ok(Err(Refusal::SignedOut));
        }
        let opening = matches!(op, Some(Op::Show(true)));
        let closing = matches!(op, Some(Op::Show(false)));
        if self.local.is_none() && !opening {
            return Ok(if closing || op.is_none() {
                Ok(())
            } else {
                Err(Refusal::Off)
            });
        }
        if self.local.is_none() {
            if !process::logged_on(session) {
                return Ok(Err(Refusal::SignedOut));
            }
            self.local = Some(Local::new()?);
        }
        let outcome = self.local.as_mut().unwrap().exchange(context, op)?;
        if closing {
            self.local = None;
        }
        Ok(outcome)
    }
}

struct Local {
    model: Model,
    context: Context,
    overlay: Overlay,
    shown: bool,
    revision: u64,
    desktop: Option<(Instant, Outcome)>,
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
    fn exchange(&mut self, context: Context, op: Option<&Op>) -> Result<Outcome> {
        self.overlay.pump();
        self.model.reconcile(&self.context, &context);
        self.context = context;
        let desktop = match self
            .desktop
            .filter(|(at, _)| op.is_none() && at.elapsed() < Duration::from_millis(50))
        {
            Some((_, desktop)) => desktop,
            None => {
                let desktop = if !process::logged_on(process::active_session()) {
                    Err(Refusal::SignedOut)
                } else if !crate::features::host::clipboard::agent::desktop_available() {
                    Err(Refusal::Locked)
                } else {
                    Ok(())
                };
                self.desktop = Some((Instant::now(), desktop));
                desktop
            }
        };
        if !self.context.allowed {
            self.model.clear();
        }
        let outcome = op.map_or(Ok(()), |op| self.model.apply(&self.context, op, desktop));
        let visible = self.model.enabled && self.context.allowed && desktop.is_ok();
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
        Ok(outcome)
    }
}
