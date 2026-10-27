use super::*;
use crate::i18n::t;
use crate::account::assist::{
    AssistRequest, SavedDevice, SavedKind, SavedLists, normalize_target,
};

#[derive(Default)]
pub(super) struct AssistUi {
    pub connect_id: String,
    pub direct_code: String,
    pub connect_error: Option<(String, Instant)>,
    pub code: String,
    pub password_prompt: Option<PendingAssist>,
    pub lists: Option<SavedLists>,
    pub loading: bool,
    pub busy: bool,
    pub querying: bool,
    pub sequence: u64,
    pub message: String,
    pub message_until: Option<Instant>,
    pub error_dialog: Option<String>,
    pub last_list_error: Option<String>,
    pub favorite_editor: Option<FavoriteEditor>,
    pub delete_prompt: Option<DeletePrompt>,
}
pub(super) struct PendingAssist {
    pub id: String,
    /// A request opened by the preflight (confirmation mode, no code).
    pub request: Option<farwing_protocol::api::AssistPending>,
    pub focus: bool,
    pub publisher_id: Option<String>,
}
pub(super) struct FavoriteEditor {
    pub id: String,
    pub remark: String,
    pub editing: bool,
    pub code: String,
    pub code_changed: bool,
}
pub(super) enum DeletePrompt {
    Device(SavedKind, SavedDevice),
    Recent,
}

pub(super) enum AssistOperation {
    QueryMode {
        id: String,
        code: String,
        publisher_id: Option<String>,
    },
    Save {
        id: String,
        remark: String,
        code: Option<String>,
    },
    Delete {
        kind: SavedKind,
        /// [`crate::account::assist::SavedDevice::key`].
        key: String,
    },
    ClearRecent,
}
impl AssistOperation {
    pub fn is_query(&self) -> bool {
        matches!(self, Self::QueryMode { .. })
    }
}
pub(super) enum AssistResult {
    /// Ready to connect: with a code, or with a request the host will
    /// confirm.
    Ready {
        id: String,
        code: String,
        request: Option<farwing_protocol::api::AssistPending>,
        publisher_id: Option<String>,
    },
    /// The host requires a code and none is saved.
    NeedsCode {
        id: String,
        publisher_id: Option<String>,
    },
    Updated(&'static str),
}

pub(super) fn operation_error(error: anyhow::Error) -> String {
    // A server failure shows its translated text, never the server's message.
    if let Some(failure) = error.downcast_ref::<crate::account::api::ApiFailure>() {
        failure.to_string()
    } else {
        error.to_string()
    }
}

impl AssistUi {
    pub fn fail(&mut self, message: impl Into<String>) {
        self.message.clear();
        self.message_until = None;
        self.error_dialog = Some(message.into());
    }

    pub fn list_failure(&mut self, message: String) {
        if self.last_list_error.as_ref() != Some(&message) {
            self.error_dialog = Some(message.clone());
        }
        self.last_list_error = Some(message);
    }
}
pub(super) async fn execute(
    client: Arc<AuthenticatedClient>,
    operation: AssistOperation,
) -> Result<AssistResult> {
    Ok(match operation {
        AssistOperation::QueryMode {
            id,
            code,
            publisher_id,
        } => {
            // Saved codes belong to an account.
            let (code, publisher_id) = if client.is_guest() {
                (code, publisher_id)
            } else {
                client
                    .resolve_assist_code(id.clone(), code, publisher_id)
                    .await?
            };
            if !code.is_empty() {
                AssistResult::Ready {
                    id,
                    code,
                    request: None,
                    publisher_id,
                }
            } else {
                // Without a code, ask the host directly: a host that trusts
                // this device or only confirms answers with an open
                // request; one that requires a code refuses with
                // `assist_code_required`.
                match client.assist_join(&id, Vec::new()).await {
                    Ok(request) if request.trusted || !request.mode.needs_password() => AssistResult::Ready {
                        id,
                        code,
                        request: Some(request),
                        publisher_id,
                    },
                    Ok(request) => {
                        let _ = client.assist_cancel(&request.request_id).await;
                        AssistResult::NeedsCode { id, publisher_id }
                    }
                    Err(error)
                        if crate::account::api::failure_code(&error)
                            == Some(crate::account::api::ErrorCode::AssistCodeRequired) =>
                    {
                        AssistResult::NeedsCode { id, publisher_id }
                    }
                    Err(error)
                        if crate::account::api::failure_code(&error)
                            == Some(crate::account::api::ErrorCode::AssistUnavailable) =>
                    {
                        anyhow::bail!(t().app.assist_unavailable)
                    }
                    Err(error) => return Err(error),
                }
            }
        }
        AssistOperation::Save { id, remark, code } => {
            client.save_assist_favorite(&id, &remark, code).await?;
            AssistResult::Updated(t().app.favorite_updated)
        }
        AssistOperation::Delete { kind, key } => {
            client.delete_assist_saved(kind, &key).await?;
            AssistResult::Updated(t().app.record_removed)
        }
        AssistOperation::ClearRecent => {
            client.clear_assist_recent().await?;
            AssistResult::Updated(t().app.recent_cleared)
        }
    })
}

impl DeviceCenterApp {
    pub(super) fn request_assist_refresh(&mut self) {
        if self.logout_pending || self.assist.loading {
            return;
        }
        if self.worker.commands.send(GuiCommand::RefreshAssist).is_ok() {
            self.assist.loading = true;
        }
    }
    pub(super) fn request_assist_connect(&mut self, value: String, code: String) {
        if self.assist.password_prompt.is_some() {
            return;
        }
        self.assist.connect_error = None;
        match normalize_target(&value) {
            Ok(id) => {
                self.assist.connect_id = id.clone();
                self.assist.direct_code.clear();
                self.assist.code.clear();
                let publisher_id = self
                    .assist
                    .lists
                    .as_ref()
                    .and_then(|l| {
                        l.favorites
                            .iter()
                            .chain(&l.recent)
                            .find(|d| d.connect_id == id)
                    })
                    .map(|d| d.publisher_device_id.clone());
                if let Some(session) = self.viewers.active.iter().find(|s| {
                    s.key == super::viewing::Key::Assistance(id.clone())
                        || publisher_id
                            .as_ref()
                            .is_some_and(|p| s.device_id.as_ref() == Some(p))
                }) {
                    if !session.closing {
                        session.handle.request_audio_only(false);
                        session.handle.focus();
                    }
                    return;
                }
                self.request_assist_operation(AssistOperation::QueryMode {
                    id,
                    code,
                    publisher_id,
                });
            }
            Err(error) => {
                self.assist.connect_error =
                    Some((error.to_string(), Instant::now() + Duration::from_secs(3)));
            }
        }
    }
    pub(super) fn request_assist_operation(&mut self, operation: AssistOperation) {
        if self.assist.busy || self.logout_pending || self.login_running || self.mutation_pending {
            return;
        }
        self.assist.sequence = self.assist.sequence.wrapping_add(1);
        self.assist.querying = operation.is_query();
        self.assist.message = if self.assist.querying {
            t().app.contacting
        } else {
            t().app.updating_records
        }
        .into();
        self.assist.message_until = None;
        self.assist.error_dialog = None;
        self.assist.busy = self
            .worker
            .commands
            .send(GuiCommand::AssistOperation {
                generation: self.login_generation,
                sequence: self.assist.sequence,
                operation,
            })
            .is_ok();
        if !self.assist.busy {
            self.assist.fail(t().app.request_send_failed);
        }
    }
    pub(super) fn cancel_assist_check(&mut self) {
        if !self.assist.querying {
            return;
        }
        self.assist.sequence = self.assist.sequence.wrapping_add(1);
        self.assist.busy = false;
        self.assist.querying = false;
        self.assist.message.clear();
        let _ = self.worker.commands.send(GuiCommand::CancelAssistCheck);
    }
    pub(super) fn finish_assist_operation(
        &mut self,
        result: std::result::Result<AssistResult, String>,
    ) {
        self.assist.busy = false;
        self.assist.querying = false;
        match result {
            Ok(AssistResult::Ready {
                id,
                code,
                request,
                publisher_id,
            }) => {
                self.assist.message.clear();
                let pending = PendingAssist {
                    id,
                    request,
                    focus: true,
                    publisher_id,
                };
                self.launch_assist(pending, code);
            }
            Ok(AssistResult::NeedsCode { id, publisher_id }) => {
                self.assist.message.clear();
                self.assist.code.clear();
                self.assist.password_prompt = Some(PendingAssist {
                    id,
                    request: None,
                    focus: true,
                    publisher_id,
                });
            }
            Ok(AssistResult::Updated(message)) => {
                self.assist.message = message.into();
                self.assist.message_until = Some(Instant::now() + Duration::from_secs(3));
                self.request_assist_refresh();
            }
            Err(message) => {
                self.assist.fail(message);
            }
        }
    }
    pub(super) fn launch_assist(&mut self, pending: PendingAssist, code: String) {
        if self.logout_pending || self.mutation_pending {
            return;
        }
        let mut request = match AssistRequest::new(&pending.id, code) {
            Ok(request) => request,
            Err(error) => {
                self.assist.fail(error.to_string());
                return;
            }
        };
        request.pending = pending.request;
        let saved = self.assist.lists.as_ref().and_then(|lists| {
            lists
                .favorites
                .iter()
                .chain(&lists.recent)
                .find(|d| d.connect_id == pending.id)
        });
        let alias = saved
            .map(|d| d.title().to_owned())
            .unwrap_or_else(|| crate::session::controller::assist_target_label(&pending.id));
        let publisher_id = pending.publisher_id;
        request.expected_publisher_id = publisher_id.clone();
        self.spawn_viewer(alias, publisher_id, Some(request), false);
    }
}
