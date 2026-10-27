//! Notifications (`docs/notifications.md`): checked when one is pushed
//! and once a minute; new ones are shown as notification cards, the list
//! and the settings are on the 通知 page.
use super::*;
use crate::i18n::t;
use farwing_protocol::api::{Notice, NoticeSetting};

/// The account's notifications are checked at least this often.
const CHECK_EVERY: Duration = Duration::from_secs(60);
const PAGE: usize = 50;
/// Newest notifications looked at for cards.
const CHECK_PAGE: usize = 20;

#[derive(Default)]
pub(super) struct NoticesUi {
    /// The page's list (`None` until it is opened).
    pub list: Option<Vec<Notice>>,
    pub more: bool,
    pub unread: u32,
    pub settings: Option<Vec<NoticeSetting>>,
    pub loading: bool,
    pub error: Option<String>,
    /// The newest notification already shown or known (`None` before the
    /// first check: what is already there gets no card).
    seen: Option<i64>,
    pushes: u64,
    next_check: Option<Instant>,
    checking: bool,
}

pub(super) enum NoticeOperation {
    /// The newest few, for cards and the unread count.
    Check,
    /// The page (`before`: the last one shown); the first page also marks
    /// them read.
    Page(Option<i64>),
    Settings,
    Save(String, bool),
}

pub(super) enum NoticeResult {
    Check(Vec<Notice>, u32),
    Page(Option<i64>, Vec<Notice>),
    Settings(Vec<NoticeSetting>),
}

pub(super) async fn execute(
    client: Arc<AuthenticatedClient>,
    operation: NoticeOperation,
) -> Result<NoticeResult> {
    Ok(match operation {
        NoticeOperation::Check => {
            let list = client
                .request(|api| async move { api.notices(None, CHECK_PAGE).await })
                .await?;
            NoticeResult::Check(list.notices, list.unread)
        }
        NoticeOperation::Page(before) => {
            let list = client
                .request(|api| async move { api.notices(before, PAGE).await })
                .await?;
            if before.is_none() && list.unread > 0 {
                client
                    .request(|api| async move { api.read_notices().await })
                    .await?;
            }
            NoticeResult::Page(before, list.notices)
        }
        NoticeOperation::Settings => {
            let settings = client
                .request(|api| async move { api.notice_settings().await })
                .await?;
            NoticeResult::Settings(settings.settings)
        }
        NoticeOperation::Save(kind, enabled) => {
            let settings = client
                .request(|api| async move { api.save_notice_setting(&kind, enabled).await })
                .await?;
            NoticeResult::Settings(settings.settings)
        }
    })
}

impl DeviceCenterApp {
    fn send_notices(&mut self, operation: NoticeOperation) -> bool {
        self.worker
            .commands
            .send(GuiCommand::Notices {
                generation: self.login_generation,
                operation,
            })
            .is_ok()
    }

    pub(super) fn request_notices(&mut self, operation: NoticeOperation) {
        if self.notices.loading || self.logout_pending || self.login_running {
            return;
        }
        self.notices.loading = true;
        self.notices.error = None;
        if !self.send_notices(operation) {
            self.notices.loading = false;
            self.notices.error = Some(t().app.request_send_failed.into());
        }
    }

    /// Check for new notifications when one was pushed, and once a minute.
    pub(super) fn tick_notices(&mut self, signed_in: bool) {
        if !signed_in || self.logout_pending || self.login_running || self.notices.checking {
            return;
        }
        let pushes = crate::account::notices::pushes();
        let due = self.notices.next_check.is_none_or(|at| Instant::now() >= at);
        if pushes == self.notices.pushes && !due {
            return;
        }
        self.notices.pushes = pushes;
        self.notices.next_check = Some(Instant::now() + CHECK_EVERY);
        self.notices.checking = self.send_notices(NoticeOperation::Check);
    }

    pub(super) fn finish_notices(&mut self, result: std::result::Result<NoticeResult, String>) {
        match result {
            Ok(NoticeResult::Check(list, unread)) => {
                self.notices.checking = false;
                self.notices.unread = unread;
                let newest = list.iter().map(|n| n.id).max();
                if let Some(seen) = self.notices.seen {
                    for notice in list.iter().rev().filter(|n| n.id > seen && !n.read) {
                        let (title, body) = (t().notices.text)(&notice.kind, &notice.data);
                        self.notifications.show_notice(notice.id, title, body);
                    }
                }
                self.notices.seen = self.notices.seen.max(newest).or(Some(0));
            }
            Ok(NoticeResult::Page(before, list)) => {
                self.notices.loading = false;
                self.notices.more = list.len() == PAGE;
                if before.is_none() {
                    self.notices.unread = 0;
                    self.notices.seen = self.notices.seen.max(list.iter().map(|n| n.id).max());
                }
                match (&mut self.notices.list, before) {
                    (Some(shown), Some(_)) => shown.extend(list),
                    (shown, _) => *shown = Some(list),
                }
            }
            Ok(NoticeResult::Settings(settings)) => {
                self.notices.loading = false;
                self.notices.settings = Some(settings);
            }
            Err(error) => {
                self.notices.checking = false;
                self.notices.loading = false;
                self.notices.error = Some(error);
            }
        }
    }
}
