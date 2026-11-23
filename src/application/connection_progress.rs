use crate::i18n::t;
use std::time::Instant;

const CONNECTION_PROGRESS_STEPS: u8 = 13;

#[derive(Clone, Debug)]
pub enum ConnectionProgressState {
    Working,
    Updating(Instant),
    Ready,
    Failed,
}

#[derive(Clone, Debug)]
pub struct ConnectionProgress {
    pub step: u8,
    pub title: String,
    pub detail: String,
    pub state: ConnectionProgressState,
    /// The other side ended the session (rather than it failing).
    pub(crate) ended: bool,
    #[cfg(windows)]
    pub(crate) background: Option<crate::application::wallpaper::Source>,
    /// An automatic reconnection is waiting; the user may skip the wait.
    pub(crate) retry: Option<RetryNow>,
}

/// Ends the current wait of an automatic reconnection ("Retry now").
#[derive(Clone, Debug, Default)]
pub(crate) struct RetryNow(std::sync::Arc<tokio::sync::Notify>);

impl RetryNow {
    pub(crate) fn trigger(&self) {
        self.0.notify_one();
    }
    pub(crate) async fn requested(&self) {
        self.0.notified().await;
    }
}

impl ConnectionProgress {
    pub(crate) fn updating(started: Instant) -> Self {
        Self {
            state: ConnectionProgressState::Updating(started),
            ..Self::working(
                1,
                t().viewer.waiting_for_update,
                t().viewer.waiting_for_update_detail,
            )
        }
    }

    pub fn working(step: u8, title: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            step,
            title: title.into(),
            detail: detail.into(),
            state: ConnectionProgressState::Working,
            ended: false,
            #[cfg(windows)]
            background: None,
            retry: None,
        }
    }

    pub fn ready(detail: impl Into<String>) -> Self {
        Self {
            step: CONNECTION_PROGRESS_STEPS,
            title: t().viewer.connected.to_owned(),
            detail: detail.into(),
            state: ConnectionProgressState::Ready,
            ended: false,
            #[cfg(windows)]
            background: None,
            retry: None,
        }
    }

    /// The other side ended the session; `reason` says why.
    pub(crate) fn ended(reason: impl Into<String>) -> Self {
        Self {
            title: t().viewer.connection_ended.to_owned(),
            ended: true,
            ..Self::failed(reason)
        }
    }

    pub fn failed(detail: impl Into<String>) -> Self {
        Self {
            step: 0,
            title: t().viewer.cannot_connect.to_owned(),
            detail: detail.into(),
            state: ConnectionProgressState::Failed,
            ended: false,
            #[cfg(windows)]
            background: None,
            retry: None,
        }
    }

    /// Offer "Retry now" while this progress is shown.
    pub(crate) fn with_retry(mut self, retry: &RetryNow) -> Self {
        self.retry = Some(retry.clone());
        self
    }

    /// A failure.
    pub(crate) fn from_error(error: &anyhow::Error) -> Self {
        Self::failed(format!("{error:#}"))
    }
}

#[cfg(windows)]
impl ConnectionProgress {
    pub(crate) fn background(source: crate::application::wallpaper::Source) -> Self {
        Self {
            background: Some(source),
            ..Self::working(0, "", "")
        }
    }
}
