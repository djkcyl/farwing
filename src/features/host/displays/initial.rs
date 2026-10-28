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
        // Only the first preparation may change the layout; later
        // participants view the screens as they are.
        let previous = lock(&self.prepared).clone();
        if let Some(previous) = previous {
            return self.select(&previous).await;
        }
        tokio::task::spawn_blocking(recovery::recover_abandoned).await??;
        let screen = self
            .run(move |state, lease| state.prepare(options, lease))
            .await?;
        *lock(&self.prepared) = Some(screen.clone());
        Ok(screen)
    }

    /// A screen of the current layout for a later participant: the first
    /// participant's, else the primary.
    async fn select(&self, previous: &capture::Screen) -> Result<capture::Screen> {
        let screens = tokio::task::spawn_blocking(capture::screens)
            .await
            .context(t().features.screen_read_interrupted)??;
        screens
            .iter()
            .find(|s| s.identity.is_some() && s.identity == previous.identity)
            .or_else(|| screens.iter().find(|s| s.primary))
            .or(screens.first())
            .cloned()
            .context(t().features.screen_none_available)
    }
    /// The mode of `screen` before this session first changed the layout;
    /// `None` while it has not.
    pub(crate) fn initial_size(&self, screen: &capture::Screen) -> Option<(u32, u32)> {
        let state = lock(&self.state);
        state
            .journal
            .baseline
            .targets
            .iter()
            .find(|t| screen.identity.as_deref() == Some(t.identity.as_str()))
            .map(|saved| (saved.width, saved.height))
    }
}

impl State {
    fn prepare(&mut self, options: ConnectOptions, lease: &Lease) -> Result<capture::Screen> {
        if self.fallback_pin.is_none() {
            self.fallback_pin = fallback::pin().unwrap_or_else(|error| {
                tracing::warn!(error=%format!("{error:#}"), "optional display driver unavailable");
                None
            });
        }
        let mut screens = capture::screens()?;
        tracing::info!(screens = screens.len(), "host initial display selection");
        // A screen the host creates (fallback, super screen) takes the
        // controller's display size.
        let virtual_size = size(
            options
                .params
                .as_ref()
                .and_then(|p| p.local_resolution.as_ref()),
        )
        .unwrap_or((1920, 1080));
        let mut automatic = None;
        let targets = Topology::query(true)?.metadata()?;
        if !self.preferences.super_enabled
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
        if self.preferences.super_enabled {
            let dpi = self.preferences.super_dpi;
            return self.enter_super(virtual_size.0, virtual_size.1, dpi, lease);
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
        // The host's screens are viewed as they are; the primary one first.
        let selected = if let Some(automatic) = automatic.as_ref() {
            screens.iter().find(|s| s.identity == automatic.identity)
        } else {
            screens.iter().find(|s| s.primary).or(screens.first())
        }
        .cloned()
        .context(t().features.screen_requested_unavailable)?;
        Ok(selected)
    }
}
