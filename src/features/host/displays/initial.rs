//! Initial layout policy, before capabilities are probed or media tracks start.
use super::*;
use crate::features::stream_control::publisher::{ConnectOptions, Resolution};
use crate::i18n::t;

fn size(value: Option<&Resolution>) -> Option<(u32, u32)> {
    let value = value?;
    let (w, h) = (
        u32::try_from(value.width).ok()?,
        u32::try_from(value.height).ok()?,
    );
    (w > 0 && h > 0).then_some((w, h))
}

impl Session {
    pub(crate) async fn prepare(&self, options: ConnectOptions) -> Result<capture::Screen> {
        ensure!(
            options.kind == 1 && matches!(options.connect_type, 1 | 2),
            "{}",
            t().features.screen_connection_type_unsupported
        );
        // Changing this host's resolution needs the `display` capability,
        // also when the session starts: without it the screens are viewed
        // as they are (resolution "follow the host").
        let mut options = options;
        if !self.lease.display_expansion_allowed()
            && let Some(params) = options.params.as_mut()
            && matches!(params.resolution_type, 2 | 4)
        {
            tracing::info!(
                requested = params.resolution_type,
                "initial resolution change refused without the display capability"
            );
            params.resolution_type = 3;
        }
        // Only the first preparation may change the layout; later
        // participants view the screens as they are.
        let previous = lock(&self.prepared).clone();
        if let Some(previous) = previous {
            return self.select(&previous, options.screen_id).await;
        }
        ensure!(
            !options.force_virtual || self.lease.display_expansion_allowed(),
            "{}",
            t().features.screen_force_virtual_not_allowed
        );
        ensure!(
            options.virtual_modes.len() <= 64,
            "{}",
            t().features.screen_too_many_virtual_modes
        );
        if let Some(params) = &options.params {
            ensure!(
                (0..=4).contains(&params.resolution_type),
                "{}",
                t().features.screen_resolution_policy_invalid
            );
            if params.resolution_type == 4 {
                validate_modes(&[size(params.chosen_resolution.as_ref())
                    .context(t().features.screen_resolution_missing)?])?;
            }
        }
        let modes: Vec<_> = options
            .virtual_modes
            .iter()
            .map(|m| Ok((u32::try_from(m.width)?, u32::try_from(m.height)?)))
            .collect::<Result<_>>()?;
        if !modes.is_empty() {
            validate_modes(&modes)?;
        }
        tokio::task::spawn_blocking(recovery::recover_abandoned).await??;
        let screen = self
            .run(move |state, lease| state.prepare(options, modes, lease))
            .await?;
        *lock(&self.prepared) = Some(screen.clone());
        Ok(screen)
    }

    /// A screen of the current layout for a later participant: the requested
    /// one, else the first participant's, else the primary.
    async fn select(&self, previous: &capture::Screen, requested: i32) -> Result<capture::Screen> {
        let screens = tokio::task::spawn_blocking(capture::screens)
            .await
            .context(t().features.screen_read_interrupted)??;
        screens
            .iter()
            .find(|s| requested >= 0 && s.id == requested)
            .or_else(|| {
                screens
                    .iter()
                    .find(|s| s.identity.is_some() && s.identity == previous.identity)
            })
            .or_else(|| screens.iter().find(|s| s.primary))
            .or(screens.first())
            .cloned()
            .context(t().features.screen_none_available)
    }
    pub(crate) fn initial_screen(&self, screen: &capture::Screen) -> capture::Screen {
        let state = lock(&self.state);
        let mut initial = screen.clone();
        if let Some(saved) = state
            .journal
            .baseline
            .targets
            .iter()
            .find(|t| screen.identity.as_deref() == Some(t.identity.as_str()))
        {
            initial.width = saved.width;
            initial.height = saved.height;
            initial.left = saved.left;
            initial.top = saved.top;
            initial.dpi_scale = state
                .journal
                .dpi
                .iter()
                .find(|(id, _)| id == &saved.identity)
                .map(|(_, dpi)| *dpi);
        }
        initial
    }
}

impl State {
    fn initial_resolution(
        &mut self,
        target: &Target,
        width: u32,
        height: u32,
        lease: &Lease,
    ) -> Result<bool> {
        let previous = self.journal.mode_changes.clone();
        self.intend_mode(&target.identity, width, height)?;
        match target.set_resolution(width, height, || lease.requested()) {
            Ok(()) => Ok(true),
            Err(error)
                if error
                    .downcast_ref::<display::topology::ModeTestRejected>()
                    .is_some() =>
            {
                ensure!(
                    lease.requested(),
                    "{}",
                    t().features.screen_operation_cancelled
                );
                // Preflight did not modify Windows. Do not leave a fictitious
                // desired mode for cleanup, or persist it as successfully applied.
                self.journal.desired = None;
                self.journal.mode_changes = previous;
                self.save()?;
                tracing::warn!(width, height, %error,
                    "initial display mode rejected; retaining current mode for connection");
                Ok(false)
            }
            Err(error) => Err(error),
        }
    }

    fn prepare(
        &mut self,
        options: ConnectOptions,
        modes: Vec<(u32, u32)>,
        lease: &Lease,
    ) -> Result<capture::Screen> {
        if self.fallback_pin.is_none() {
            self.fallback_pin = fallback::pin().unwrap_or_else(|error| {
                tracing::warn!(error=%format!("{error:#}"), "optional display driver unavailable");
                None
            });
        }
        let mut screens = capture::screens()?;
        tracing::info!(
            screens = screens.len(),
            forced_virtual = options.force_virtual,
            saved_virtual = self.preferences.default_virtual,
            "host initial display selection"
        );
        let choice = options.params.as_ref().map_or(0, |p| p.resolution_type);
        let local = size(
            options
                .params
                .as_ref()
                .and_then(|p| p.local_resolution.as_ref()),
        );
        let virtual_size = if choice == 0 {
            size(options.virtual_initial.as_ref())
        } else {
            local
        }
        .unwrap_or((1920, 1080));
        let mut automatic = None;
        let targets = Topology::query(true)?.metadata()?;
        if !options.force_virtual
            && !self.preferences.default_virtual
            && !self.preferences.super_enabled
            && !targets.iter().any(|t| t.available && !t.foreign())
        {
            automatic = fallback::ensure(
                self.fallback_driver()?,
                virtual_size.0,
                virtual_size.1,
                &[],
                true,
            )?;
            screens = capture::screens()?;
        }
        if !options.force_virtual && self.preferences.super_enabled {
            return self.enter_super(virtual_size.0, virtual_size.1, 0, true, lease);
        }
        if options.force_virtual || self.preferences.default_virtual {
            validate_modes(&[virtual_size])?;
            let result = self.create(
                Virtual {
                    guid: uuid::Uuid::new_v4().to_string(),
                    identity: None,
                    width: virtual_size.0,
                    height: virtual_size.1,
                    hz: 144,
                    dpi: 0,
                    kind: 3,
                    resolution_type: choice.max(1),
                    modes,
                    layout: None,
                },
                lease,
                options.force_virtual || self.preferences.default_virtual,
            );
            if result.is_ok() && options.force_virtual && !self.has_saved_preferences {
                self.preferences.default_virtual = true;
                self.save_preferences()?;
            }
            return result;
        }
        let saved = self.preferences.manual.clone();
        ensure!(
            saved.len() <= 3 && saved.len() + screens.len() <= 5,
            "{}",
            t().features.screen_saved_layout_too_many
        );
        self.restoring = true;
        let restore = (|| -> Result<()> {
            for mut spec in saved {
                validate_modes(&[(spec.width, spec.height)])?;
                spec.identity = None;
                self.create(spec, lease, false)?;
            }
            // Physical modes belong to the current desktop owner. Previous
            // connections may remember virtual resources, but must not replay
            // a physical resolution/DPI over a game or a local user's choice.
            Ok(())
        })();
        self.restoring = false;
        restore?;
        let mut screens = capture::screens()?;
        for screen in &mut screens {
            if let Some(owned) =
                self.journal.owned.iter().find(|o| {
                    o.identity.is_some() && o.identity.as_ref() == screen.identity.as_ref()
                })
            {
                screen.id = display::source_id(&format!("farwing-virtual/{}", owned.guid))?;
                screen.render_adapter = self.render_adapter;
            }
        }
        let selected = if let Some(automatic) = automatic.as_ref() {
            screens.iter().find(|s| s.identity == automatic.identity)
        } else if options.screen_id == -1 {
            screens.iter().find(|s| s.primary).or(screens.first())
        } else {
            screens.iter().find(|s| s.id == options.screen_id)
        }
        .cloned()
        .context(t().features.screen_requested_unavailable)?;
        // The saved controller layout owns initial physical/virtual modes.
        // Incoming resolution choices apply only when no layout was saved.
        if self.has_saved_preferences {
            return Ok(selected);
        }
        let requested = match choice {
            2 => local,
            4 => size(
                options
                    .params
                    .as_ref()
                    .and_then(|p| p.chosen_resolution.as_ref()),
            ),
            _ => None,
        };
        if let Some((width, height)) = requested {
            validate_modes(&[(width, height)])?;
            if (selected.width, selected.height) == (width, height) {
                // Preserve the explicit preference without starting a recovery
                // guard or writing mutation intent for an unchanged mode.
                self.remember_physical(
                    selected
                        .identity
                        .as_deref()
                        .context(t().features.screen_initial_target_missing)?,
                    Some(choice),
                )?;
                return Ok(selected);
            }
            let target = Topology::query(true)?
                .targets()?
                .into_iter()
                .find(|t| selected.identity.as_deref() == Some(t.identity.as_str()))
                .context(t().features.screen_initial_target_missing)?;
            if !target
                .modes
                .iter()
                .any(|m| m.width == width && m.height == height)
            {
                return self.enter_super(width, height, 0, false, lease);
            }
            self.begin()?;
            let changed = self.initial_resolution(&target, width, height, lease)?;
            self.applied()?;
            if changed {
                self.remember_physical(&target.identity, Some(choice))?;
            }
            return find_screen(&target.identity);
        }
        Ok(selected)
    }
}
