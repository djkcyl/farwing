//! Connection records and account events (`docs/connection-records.md`),
//! read from the server page by page.
use super::*;
use crate::i18n::t;
use farwing_protocol::api::{AccountEvent, ConnectionRecord};

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum RecordsTab {
    #[default]
    Connections,
    Events,
}

#[derive(Default)]
pub(super) struct RecordsUi {
    pub tab: RecordsTab,
    pub connections: Option<Vec<ConnectionRecord>>,
    pub events: Option<Vec<AccountEvent>>,
    pub more_connections: bool,
    pub more_events: bool,
    pub loading: bool,
    pub error: Option<String>,
    /// Records opened to their details.
    pub open: std::collections::HashSet<i64>,
}

pub(super) enum RecordOperation {
    /// Connections (`before`: the last one shown, for the next page).
    Connections(Option<i64>),
    Events(Option<i64>),
}

pub(super) enum RecordResult {
    Connections(Option<i64>, Vec<ConnectionRecord>),
    Events(Option<i64>, Vec<AccountEvent>),
}

pub(super) async fn execute(
    client: Arc<AuthenticatedClient>,
    operation: RecordOperation,
) -> Result<RecordResult> {
    Ok(match operation {
        RecordOperation::Connections(before) => {
            let list = client
                .request(|api| async move { api.records(None, before).await })
                .await?;
            RecordResult::Connections(before, list)
        }
        RecordOperation::Events(before) => {
            let list = client
                .request(|api| async move { api.account_events(before).await })
                .await?;
            RecordResult::Events(before, list)
        }
    })
}

impl DeviceCenterApp {
    pub(super) fn request_records(&mut self, operation: RecordOperation) {
        if self.records.loading || self.logout_pending || self.login_running {
            return;
        }
        self.records.loading = true;
        self.records.error = None;
        if self
            .worker
            .commands
            .send(GuiCommand::Records {
                generation: self.login_generation,
                operation,
            })
            .is_err()
        {
            self.records.loading = false;
            self.records.error = Some(t().app.request_send_failed.into());
        }
    }

    pub(super) fn finish_records(&mut self, result: std::result::Result<RecordResult, String>) {
        let page = crate::account::api::RECORD_PAGE;
        self.records.loading = false;
        match result {
            Ok(RecordResult::Connections(before, list)) => {
                self.records.more_connections = list.len() == page;
                match (&mut self.records.connections, before) {
                    (Some(shown), Some(_)) => shown.extend(list),
                    (shown, _) => *shown = Some(list),
                }
            }
            Ok(RecordResult::Events(before, list)) => {
                self.records.more_events = list.len() == page;
                match (&mut self.records.events, before) {
                    (Some(shown), Some(_)) => shown.extend(list),
                    (shown, _) => *shown = Some(list),
                }
            }
            Err(error) => self.records.error = Some(error),
        }
    }
}
