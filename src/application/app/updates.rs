//! Public release checks are independent of UU login and device sessions.
use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};
use tokio::sync::oneshot;
mod settings;
mod version;
use version::ReleaseVersion;

// Repository identity survives the OpenUUYC -> Farwing rename. Name-based API
// URLs return 301, while this client deliberately does not follow redirects.
#[cfg(not(target_os = "linux"))]
const LATEST_RELEASE: &str = "https://api.github.com/repositories/1364197594/releases/latest";
#[cfg(not(target_os = "linux"))]
const RELEASE_LIST: &str = "https://api.github.com/repositories/1364197594/releases";
#[cfg(not(target_os = "linux"))]
pub(super) const PROJECT: &str = "https://github.com/djkcyl/farwing";
#[cfg(not(target_os = "linux"))]
pub(super) const RELEASES: &str = "https://github.com/djkcyl/farwing/releases";
#[cfg(target_os = "linux")]
const LATEST_RELEASE: &str = "https://api.github.com/repositories/1374243181/releases/latest";
#[cfg(target_os = "linux")]
const RELEASE_LIST: &str = "https://api.github.com/repositories/1374243181/releases";
#[cfg(target_os = "linux")]
pub(super) const PROJECT: &str = "https://github.com/DoyoDia/openuuyc-linux";
#[cfg(target_os = "linux")]
pub(super) const RELEASES: &str = "https://github.com/DoyoDia/openuuyc-linux/releases";
const RETRY_INTERVAL: Duration = Duration::from_secs(10);

pub(super) fn version_label() -> &'static str {
    let version = option_env!("OPENUUYC_RELEASE_TAG")
        .filter(|tag| !tag.is_empty())
        .unwrap_or(env!("CARGO_PKG_VERSION"));
    version.strip_prefix('v').unwrap_or(version)
}

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

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    body: Option<String>,
    published_at: Option<String>,
}

async fn latest(allow_prerelease: bool) -> Result<State> {
    let client = crate::transport::http_client()
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(12))
        .user_agent(concat!("OpenUUYC/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("无法创建更新检查连接")?;
    if !allow_prerelease {
        return match fetch(&client, LATEST_RELEASE, 512 * 1024).await? {
            Some(body) => release_state(&body, version_label()),
            None => Ok(State::NoRelease),
        };
    }
    let mut releases = Vec::new();
    let mut total_bytes = 0usize;
    for page in 1..=10 {
        let url = format!("{RELEASE_LIST}?per_page=100&page={page}");
        let Some(body) = fetch(&client, &url, 2 * 1024 * 1024).await? else {
            if page == 1 {
                return Ok(State::NoRelease);
            }
            bail!("GitHub 版本列表读取不完整，请重试");
        };
        total_bytes += body.len();
        ensure!(total_bytes <= 8 * 1024 * 1024, "GitHub 版本列表过大");
        let batch: Vec<Release> =
            serde_json::from_slice(&body).context("无法解析 GitHub 版本列表")?;
        let complete = batch.len() < 100;
        releases.extend(batch);
        if complete {
            return select_release(releases, version_label(), true);
        }
    }
    bail!("GitHub 版本列表过长，未完成检查")
}

async fn fetch(
    client: &reqwest::Client,
    url: &str,
    maximum_bytes: usize,
) -> Result<Option<Vec<u8>>> {
    let mut response = crate::transport::http::send(
        crate::transport::http::Operation::Update,
        client
            .get(url)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2026-03-10"),
    )
    .await
    .context("无法连接 GitHub，请检查网络后重试")?;
    match response.status() {
        reqwest::StatusCode::NOT_FOUND => return Ok(None),
        reqwest::StatusCode::FORBIDDEN | reqwest::StatusCode::TOO_MANY_REQUESTS => {
            bail!("GitHub 暂时限制请求，请稍后重试")
        }
        _ => {
            response
                .error_for_status_ref()
                .context("GitHub 更新检查失败")?;
            ensure!(
                response.status().is_success(),
                "GitHub 更新接口返回意外 HTTP 状态：{}",
                response.status()
            );
        }
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.context("读取版本信息失败")? {
        ensure!(
            body.len() + chunk.len() <= maximum_bytes,
            "GitHub 版本信息过大"
        );
        body.extend_from_slice(&chunk);
    }
    Ok(Some(body))
}

fn select_release(releases: Vec<Release>, current: &str, allow_prerelease: bool) -> Result<State> {
    let selected = releases
        .into_iter()
        .filter_map(|release| {
            if release.draft || release.published_at.is_none() {
                return None;
            }
            let version =
                ReleaseVersion::parse(&release.tag_name, cfg!(target_os = "linux")).ok()?;
            if !allow_prerelease && (release.prerelease || !version.base.pre.is_empty()) {
                return None;
            }
            Some((version, release))
        })
        .max_by(|(a, ra), (b, rb)| {
            a.cmp_precedence(b)
                .then_with(|| rb.prerelease.cmp(&ra.prerelease))
        });
    match selected {
        Some((version, release)) => state_for_release(release, version, current),
        None => Ok(State::NoRelease),
    }
}

fn release_state(body: &[u8], current: &str) -> Result<State> {
    let release: Release = serde_json::from_slice(body).context("无法解析 GitHub 版本信息")?;
    ensure!(
        !release.draft && !release.prerelease,
        "GitHub 未返回正式版本"
    );
    let version = ReleaseVersion::parse(&release.tag_name, cfg!(target_os = "linux"))
        .context("GitHub 正式版本号格式不正确")?;
    ensure!(version.base.pre.is_empty(), "GitHub 最新版本不是正式版本");
    state_for_release(release, version, current)
}

fn state_for_release(release: Release, version: ReleaseVersion, current: &str) -> Result<State> {
    let current = ReleaseVersion::current(current, cfg!(target_os = "linux"))
        .context("当前程序版本号格式不正确")?;
    match version.cmp_precedence(&current) {
        std::cmp::Ordering::Equal => Ok(State::Current),
        std::cmp::Ordering::Less => Ok(State::Ahead),
        std::cmp::Ordering::Greater => {
            let mut url = url::Url::parse(RELEASES)?;
            url.path_segments_mut()
                .expect("GitHub base URL")
                .push("tag")
                .push(&release.tag_name);
            Ok(State::Available {
                prerelease: release.prerelease || !version.base.pre.is_empty(),
                version: release
                    .tag_name
                    .strip_prefix('v')
                    .unwrap_or(&release.tag_name)
                    .to_owned(),
                url: url.to_string(),
                notes: release.body.unwrap_or_default(),
                published: release.published_at.and_then(|value| {
                    chrono::DateTime::parse_from_rfc3339(&value)
                        .ok()
                        .map(|date| date.format("%Y-%m-%d").to_string())
                }),
            })
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    fn release(tag: &str, prerelease: bool) -> Release {
        Release {
            tag_name: tag.to_owned(),
            draft: false,
            prerelease,
            body: Some("release notes".into()),
            published_at: Some("2026-10-31T00:00:00Z".into()),
        }
    }

    #[test]
    fn same_linux_build_does_not_prompt_to_update() {
        let state = select_release(
            vec![release("v1.0.2-beta.5-linux.10", true)],
            "1.0.2-beta.5-linux.10",
            true,
        )
        .unwrap();
        assert!(matches!(state, State::Current));
    }

    #[test]
    fn next_linux_build_is_selected_numerically() {
        let state = select_release(
            vec![
                release("v1.0.2-beta.5-linux.10", true),
                release("v1.0.2-beta.5-linux.9", true),
                release("v9.0.0", false),
            ],
            "1.0.2-beta.5-linux.9",
            true,
        )
        .unwrap();
        let State::Available {
            version,
            url,
            prerelease,
            ..
        } = state
        else {
            panic!("expected a Linux update");
        };
        assert_eq!(version, "1.0.2-beta.5-linux.10");
        assert_eq!(
            url,
            "https://github.com/DoyoDia/openuuyc-linux/releases/tag/v1.0.2-beta.5-linux.10"
        );
        assert!(prerelease);
    }

    #[test]
    fn stable_linux_release_is_available_on_stable_channel() {
        let state = select_release(
            vec![release("v1.0.3-linux.1", false)],
            "1.0.2-beta.5-linux.10",
            false,
        )
        .unwrap();
        assert!(matches!(
            state,
            State::Available {
                prerelease: false,
                ..
            }
        ));
    }

    #[test]
    fn stable_endpoint_accepts_linux_build_suffix() {
        let body = br#"{"tag_name":"v1.0.3-linux.1","draft":false,"prerelease":false,"published_at":"2026-10-31T00:00:00Z"}"#;
        assert!(matches!(
            release_state(body, "1.0.2-beta.5-linux.10").unwrap(),
            State::Available {
                prerelease: false,
                ..
            }
        ));
    }

    #[test]
    fn stable_channel_excludes_linux_betas() {
        let state = select_release(
            vec![release("v1.0.3-beta.1-linux.1", true)],
            "1.0.2-linux.1",
            false,
        )
        .unwrap();
        assert!(matches!(state, State::NoRelease));
    }

    #[test]
    fn older_linux_build_does_not_request_a_downgrade() {
        let state = select_release(
            vec![release("v1.0.2-beta.5-linux.9", true)],
            "1.0.2-beta.5-linux.10",
            true,
        )
        .unwrap();
        assert!(matches!(state, State::Ahead));
    }
}
