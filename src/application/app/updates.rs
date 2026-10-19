//! Release checks against the configured Farwing Server's update feed.
//!
//! The administrator publishes release metadata in the server configuration;
//! the check needs no login. Without a configured server or feed there is no
//! update source and nothing is contacted.
use anyhow::{Context, Result};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};
use tokio::sync::oneshot;
mod settings;

const RETRY_INTERVAL: Duration = Duration::from_secs(10);

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
        };
        check.request(ctx);
        check
    }
    pub fn allow_prerelease(&self) -> bool {
        self.settings.allow_prerelease
    }
    pub fn release_label(&self) -> &'static str {
        if self.allow_prerelease() {
            "可用版本"
        } else {
            "正式版"
        }
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
        self.change_channel(next);
        self.request(ctx);
        Ok(())
    }
    fn change_channel(&mut self, next: settings::Settings) {
        if let Some(cancel) = self.cancel.take() {
            let _ = cancel.send(());
        }
        // Drop the old mailbox before a replacement can publish its result.
        self.pending = None;
        self.settings = next;
        self.settings_error = None;
        self.dialog_open = false;
        self.notified_version = None;
        self.state = State::Checking;
        self.retry_at = Instant::now();
    }

    pub fn request(&mut self, ctx: &egui::Context) {
        if self.pending.is_some() || Instant::now() < self.retry_at {
            return;
        }
        self.retry_at = Instant::now() + RETRY_INTERVAL;
        let (tx, rx) = mpsc::channel();
        let (cancel_tx, cancel_rx) = oneshot::channel();
        let ctx = ctx.clone();
        let allow_prerelease = self.allow_prerelease();
        let worker = std::thread::Builder::new()
            .name("release-check".into())
            .spawn(move || {
                let result = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => {
                        let result = runtime.block_on(async {
                            tokio::select! {
                                biased;
                                _ = cancel_rx => None,
                                result = tokio::time::timeout(Duration::from_secs(25), latest(allow_prerelease)) =>
                                    Some(result.context("更新检查超时").and_then(|r|r)),
                            }
                        });
                        runtime.shutdown_background();
                        result
                    }
                    Err(error) => Some(Err(error).context("无法启动更新检查")),
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
            Err(error) => self.state = State::Failed(format!("无法启动更新检查：{error}")),
        }
    }

    pub fn poll(&mut self) {
        let Some(rx) = &self.pending else {
            return;
        };
        let result = match rx.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => Err(anyhow::anyhow!("更新检查未完成")),
        };
        self.pending = None;
        self.cancel = None;
        self.state = result.unwrap_or_else(|error| {
            tracing::warn!(error = %format!("{error:#}"), "release check failed");
            State::Failed(format!("{error:#}"))
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

async fn latest(allow_prerelease: bool) -> Result<State> {
    let Some(server) = crate::account::server::ServerConfig::load()? else {
        return Ok(State::NoRelease);
    };
    let api = crate::account::api::ServerApi::new(&server)?;
    let channel = if allow_prerelease { "beta" } else { "stable" };
    let release = api
        .latest_release(channel)
        .await
        .context("无法从 Farwing 服务器获取版本信息")?;
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
        .context("服务器提供的版本号格式不正确")?;
    let prerelease = release.prerelease || !version.pre.is_empty();
    if prerelease && !allow_prerelease {
        return Ok(State::NoRelease);
    }
    let current = semver::Version::parse(current).context("当前程序版本号格式不正确")?;
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
}
