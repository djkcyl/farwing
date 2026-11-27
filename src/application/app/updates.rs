//! Release checks against the configured Farwing Server's update feed.
//!
//! The check reads administrator-published metadata without logging in.
//! Without a configured server, nothing is contacted.
use crate::account::server::ServerConfig;
use crate::i18n::t;
use anyhow::{Context, Result};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};
use tokio::sync::oneshot;
#[cfg(target_os = "linux")]
#[path = "../linux/updates.rs"]
mod linux;
#[path = "updates/settings.rs"]
mod settings;

const RETRY_INTERVAL: Duration = Duration::from_secs(10);

#[cfg(target_os = "linux")]
type ReleaseSource = std::result::Result<Option<ServerConfig>, String>;

#[derive(Debug)]
pub(super) enum State {
    Checking,
    Current,
    Ahead,
    Available {
        version: String,
        url: String,
        notes: String,
        published: Option<String>,
        prerelease: bool,
    },
    NoRelease,
    /// The server did not answer: its state, not the release's.
    Unreachable,
    Failed(String),
}

pub(super) struct UpdateCheck {
    pub state: State,
    pub dialog_open: bool,
    notified_version: Option<String>,
    pending: Option<Receiver<Result<State>>>,
    cancel: Option<oneshot::Sender<()>>,
    retry_at: Instant,
    settings: settings::Settings,
    pub settings_error: Option<String>,
    #[cfg(target_os = "linux")]
    source: ReleaseSource,
}

impl UpdateCheck {
    pub fn start(ctx: &egui::Context) -> Self {
        let (settings, settings_error) = match settings::Settings::load() {
            Ok(settings) => (settings, None),
            Err(error) => (Default::default(), Some(format!("{error:#}"))),
        };
        let mut check = Self {
            state: State::Checking,
            dialog_open: false,
            notified_version: None,
            pending: None,
            cancel: None,
            retry_at: Instant::now(),
            settings,
            settings_error,
            #[cfg(target_os = "linux")]
            source: Ok(None),
        };
        check.request(ctx);
        check
    }
    pub fn allow_prerelease(&self) -> bool {
        self.settings.allow_prerelease
    }
    pub fn set_allow_prerelease(&mut self, allow: bool, ctx: &egui::Context) -> Result<()> {
        if allow == self.allow_prerelease() {
            return Ok(());
        }
        let next = settings::Settings {
            allow_prerelease: allow,
        };
        if let Err(error) = next.save() {
            self.settings_error = Some(format!("{error:#}"));
            return Err(error);
        }
        self.settings = next;
        self.settings_error = None;
        self.reset();
        self.request(ctx);
        Ok(())
    }
    fn reset(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            let _ = cancel.send(());
        }
        // Drop the old mailbox before a replacement can publish its result.
        self.pending = None;
        self.dialog_open = false;
        self.notified_version = None;
        self.state = State::Checking;
        self.retry_at = Instant::now();
    }

    pub fn request(&mut self, ctx: &egui::Context) {
        #[cfg(target_os = "linux")]
        self.change_source(configured_source());
        self.request_current(ctx);
    }

    #[cfg(target_os = "linux")]
    pub fn recheck_source(&mut self, ctx: &egui::Context) {
        if self.change_source(configured_source()) {
            self.request_current(ctx);
        }
    }

    #[cfg(target_os = "linux")]
    fn change_source(&mut self, source: ReleaseSource) -> bool {
        if self.source == source {
            return false;
        }
        self.reset();
        self.source = source;
        true
    }

    fn request_current(&mut self, ctx: &egui::Context) {
        if self.pending.is_some() || Instant::now() < self.retry_at {
            return;
        }
        self.retry_at = Instant::now() + RETRY_INTERVAL;
        let (tx, rx) = mpsc::channel();
        let (cancel_tx, cancel_rx) = oneshot::channel();
        let ctx = ctx.clone();
        let allow_prerelease = self.allow_prerelease();
        #[cfg(target_os = "linux")]
        let source = self.source.clone();
        let worker = std::thread::Builder::new()
            .name("release-check".into())
            .spawn(move || {
                let result = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => {
                        let result = runtime.block_on(async {
                            let check = async {
                                #[cfg(windows)]
                                let source = ServerConfig::load()?;
                                #[cfg(target_os = "linux")]
                                let source = source.map_err(anyhow::Error::msg)?;
                                latest(source, allow_prerelease).await
                            };
                            tokio::select! {
                                biased;
                                _ = cancel_rx => None,
                                result = tokio::time::timeout(Duration::from_secs(25), check) =>
                                    Some(result.context(t().app.update_check_timeout).and_then(|r| r)),
                            }
                        });
                        runtime.shutdown_background();
                        result
                    }
                    Err(error) => Some(Err(error).context(t().app.cant_start_update_check)),
                };
                if let Some(result) = result {
                    let _ = tx.send(result);
                    ctx.request_repaint();
                }
            });
        match worker {
            Ok(_) => {
                self.pending = Some(rx);
                self.cancel = Some(cancel_tx);
                self.state = State::Checking;
            }
            Err(error) => {
                self.state =
                    State::Failed((t().app.cant_start_update_check_with)(&error.to_string()))
            }
        }
    }

    /// Check again at once after a failed check: the server is back.
    pub fn retry_failed(&mut self, ctx: &egui::Context) {
        if matches!(self.state, State::Failed(_) | State::Unreachable) {
            self.retry_at = Instant::now();
            self.request(ctx);
        }
    }

    pub fn poll(&mut self) {
        let Some(rx) = &self.pending else {
            return;
        };
        let result = match rx.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => Err(anyhow::anyhow!(t().app.update_check_incomplete)),
        };
        self.pending = None;
        self.cancel = None;
        self.state = result.unwrap_or_else(|error| {
            tracing::warn!(error = %format!("{error:#}"), "release check failed");
            if out_of_reach(&error) {
                State::Unreachable
            } else {
                State::Failed(format!("{error:#}"))
            }
        });
        if let State::Available { version, .. } = &self.state
            && self.notified_version.as_ref() != Some(version)
        {
            self.notified_version = Some(version.clone());
            self.dialog_open = true;
        }
    }

    pub fn retry_wait(&self) -> u64 {
        self.retry_at
            .saturating_duration_since(Instant::now())
            .as_secs_f64()
            .ceil() as u64
    }
}

impl Drop for UpdateCheck {
    fn drop(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            let _ = cancel.send(());
        }
    }
}

/// The server could not be reached (or did not answer in time).
fn out_of_reach(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause.is::<tokio::time::error::Elapsed>()
            || cause
                .downcast_ref::<crate::account::api::ApiFailure>()
                .is_some_and(|failure| crate::account::session_restore::out_of_reach(failure.code))
    })
}

#[cfg(target_os = "linux")]
fn configured_source() -> ReleaseSource {
    ServerConfig::load().map_err(|error| format!("{error:#}"))
}

async fn latest(source: Option<ServerConfig>, allow_prerelease: bool) -> Result<State> {
    let Some(server) = source else {
        return Ok(State::NoRelease);
    };
    let api = crate::account::api::ServerApi::new(&server)?;
    let channel = if allow_prerelease { "beta" } else { "stable" };
    let release = api
        .latest_release(channel)
        .await
        .context(t().app.cant_get_version)?;
    match release {
        Some(release) => release_state(release, env!("CARGO_PKG_VERSION"), allow_prerelease),
        None => Ok(State::NoRelease),
    }
}

fn release_state(
    release: crate::account::api::ReleaseInfo,
    current: &str,
    allow_prerelease: bool,
) -> Result<State> {
    let version = semver::Version::parse(release.version.strip_prefix('v').unwrap_or(&release.version))
        .context(t().app.invalid_server_version)?;
    let prerelease = release.prerelease || !version.pre.is_empty();
    if prerelease && !allow_prerelease {
        return Ok(State::NoRelease);
    }
    let current = semver::Version::parse(current).context(t().app.invalid_current_version)?;
    Ok(match version.cmp_precedence(&current) {
        std::cmp::Ordering::Equal => State::Current,
        std::cmp::Ordering::Less => State::Ahead,
        std::cmp::Ordering::Greater => {
            // Only an absolute HTTPS link may be opened from the dialog.
            let url = url::Url::parse(&release.url)
                .ok()
                .filter(|url| url.scheme() == "https")
                .map(String::from)
                .unwrap_or_default();
            State::Available {
                prerelease,
                version: version.to_string(),
                url,
                notes: release.notes,
                published: (release.published_at > 0)
                    .then(|| chrono::DateTime::from_timestamp(release.published_at, 0))
                    .flatten()
                    .map(|date| date.format("%Y-%m-%d").to_string()),
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(version: &str, prerelease: bool) -> crate::account::api::ReleaseInfo {
        crate::account::api::ReleaseInfo {
            version: version.into(),
            prerelease,
            published_at: 1_760_000_000,
            notes: "notes".into(),
            url: "https://example.com/farwing.exe".into(),
        }
    }

    #[test]
    fn compares_versions() {
        assert!(matches!(
            release_state(release("1.0.1", false), "1.0.0", false).unwrap(),
            State::Available { .. }
        ));
        assert!(matches!(
            release_state(release("1.0.0", false), "1.0.0", false).unwrap(),
            State::Current
        ));
        assert!(matches!(
            release_state(release("1.1.0-beta.1", true), "1.0.0", false).unwrap(),
            State::NoRelease
        ));
        assert!(matches!(
            release_state(release("0.9.0", false), "1.0.0", true).unwrap(),
            State::Ahead
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn switching_release_sources_discards_pending_results() {
        let old = ServerConfig::new("old.example", None, None).unwrap();
        let new = ServerConfig::new("new.example", None, None).unwrap();
        let (tx, rx) = mpsc::channel();
        let (cancel, mut canceled) = oneshot::channel();
        let mut check = UpdateCheck {
            state: State::Checking,
            dialog_open: false,
            notified_version: None,
            pending: Some(rx),
            cancel: Some(cancel),
            retry_at: Instant::now() + RETRY_INTERVAL,
            settings: Default::default(),
            settings_error: None,
            source: Ok(Some(old.clone())),
        };
        tx.send(release_state(release("1.0.1", false), "1.0.0", false))
            .unwrap();

        assert!(!check.change_source(Ok(Some(old))));
        assert!(check.change_source(Ok(Some(new))));
        assert!(matches!(canceled.try_recv(), Ok(())));
        check.poll();

        assert!(matches!(check.state, State::Checking));
        assert_eq!(check.retry_wait(), 0);
    }
}
