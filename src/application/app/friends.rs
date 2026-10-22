//! Friends, friend requests and what each friend may do on this account's
//! devices.
use super::*;
use crate::i18n::t;
use farwing_protocol::api::{Friend, FriendRequest, Grants, UserSummary};

#[derive(Default)]
pub(super) struct FriendsUi {
    pub data: Option<FriendsData>,
    pub loading: bool,
    pub busy: bool,
    pub add_username: String,
    /// Filters the friend list.
    pub search: String,
    pub message: Option<(String, Instant)>,
    pub error: Option<String>,
    pub editor: Option<GrantEditor>,
    pub confirm_remove: Option<UserSummary>,
}

#[derive(Clone, Default)]
pub(super) struct FriendsData {
    pub friends: Vec<Friend>,
    pub incoming: Vec<FriendRequest>,
    pub outgoing: Vec<FriendRequest>,
}

/// Editing what this account grants one friend.
pub(super) struct GrantEditor {
    pub user: UserSummary,
    /// `None` while loading.
    pub grants: Option<Grants>,
    pub original: Option<Grants>,
    /// A save is in flight; the editor closes once it succeeds.
    pub saving: bool,
}

pub(super) enum FriendOperation {
    Refresh,
    Add(String),
    Answer(String, bool),
    Cancel(String),
    Remove(String),
    LoadGrants(UserSummary),
    SaveGrants(String, Grants),
}

pub(super) enum FriendResult {
    Data(FriendsData),
    Grants(UserSummary, Grants),
    Done(String),
}

pub(super) async fn execute(
    client: Arc<AuthenticatedClient>,
    operation: FriendOperation,
) -> Result<FriendResult> {
    Ok(match operation {
        FriendOperation::Refresh => {
            let (friends, requests) =
                tokio::try_join!(client.friends(), client.friend_requests())?;
            FriendResult::Data(FriendsData {
                friends: friends.friends,
                incoming: requests.incoming,
                outgoing: requests.outgoing,
            })
        }
        FriendOperation::Add(username) => match client.send_friend_request(&username).await {
            Ok(farwing_protocol::api::FriendRequestOutcome::Sent { .. }) => {
                FriendResult::Done(t().app.friend_request_sent.into())
            }
            Ok(farwing_protocol::api::FriendRequestOutcome::Accepted { friend }) => {
                FriendResult::Done((t().app.now_friends)(&friend.user.display_name()))
            }
            Err(error) => {
                use crate::account::api::ErrorCode;
                match crate::account::api::failure_code(&error) {
                    Some(ErrorCode::UserNotFound) => anyhow::bail!(t().app.user_not_found),
                    Some(ErrorCode::AlreadyFriends) => anyhow::bail!(t().app.already_friends),
                    _ => return Err(error),
                }
            }
        },
        FriendOperation::Answer(id, accept) => {
            client.answer_friend_request(&id, accept).await?;
            FriendResult::Done(
                if accept {
                    t().app.friend_request_accepted
                } else {
                    t().app.friend_request_declined
                }
                .into(),
            )
        }
        FriendOperation::Cancel(id) => {
            client.cancel_friend_request(&id).await?;
            FriendResult::Done(t().app.friend_request_withdrawn.into())
        }
        FriendOperation::Remove(user_id) => {
            client.remove_friend(&user_id).await?;
            FriendResult::Done(t().app.friend_removed.into())
        }
        FriendOperation::LoadGrants(user) => {
            let grants = client.grants(&user.user_id).await?;
            FriendResult::Grants(user, grants)
        }
        FriendOperation::SaveGrants(user_id, grants) => {
            client.set_grants(&user_id, grants).await?;
            FriendResult::Done(t().app.permissions_saved.into())
        }
    })
}

/// Display label of a session capability.
pub(crate) fn cap_label(cap: &str) -> &'static str {
    use farwing_protocol::caps;
    let text = &t().app;
    match cap {
        caps::VIEW => text.cap_view,
        caps::CONTROL => text.cap_control,
        caps::CLIPBOARD => text.cap_clipboard,
        caps::FILES => text.cap_files,
        caps::PORTS => text.cap_ports,
        caps::POWER => text.cap_power,
        caps::AUDIO => text.cap_audio,
        caps::MICROPHONE => text.cap_microphone,
        caps::DISPLAY => text.cap_display,
        caps::DIAGNOSTICS => text.cap_diagnostics,
        caps::ANNOTATION => text.cap_annotation,
        _ => text.cap_unknown,
    }
}

impl DeviceCenterApp {
    pub(super) fn request_friends(&mut self, operation: FriendOperation) {
        if self.friends.busy && !matches!(operation, FriendOperation::Refresh) {
            return;
        }
        if self.logout_pending || self.login_running {
            return;
        }
        let refresh = matches!(operation, FriendOperation::Refresh);
        if refresh {
            self.friends.loading = true;
        } else {
            self.friends.busy = true;
        }
        self.friends.error = None;
        if self
            .worker
            .commands
            .send(GuiCommand::Friends {
                generation: self.login_generation,
                operation,
            })
            .is_err()
        {
            self.friends.loading = false;
            self.friends.busy = false;
            self.friends.error = Some(t().app.request_send_failed.into());
        }
    }

    pub(super) fn finish_friends(&mut self, result: std::result::Result<FriendResult, String>) {
        match result {
            Ok(FriendResult::Data(data)) => {
                self.friends.loading = false;
                self.friends.data = Some(data);
            }
            Ok(FriendResult::Grants(user, grants)) => {
                self.friends.busy = false;
                if let Some(editor) = &mut self.friends.editor
                    && editor.user.user_id == user.user_id
                {
                    editor.grants = Some(grants.clone());
                    editor.original = Some(grants);
                }
            }
            Ok(FriendResult::Done(message)) => {
                self.friends.busy = false;
                self.friends.message = Some((message, Instant::now() + Duration::from_secs(4)));
                if self.friends.editor.as_ref().is_some_and(|editor| editor.saving) {
                    self.friends.editor = None;
                }
                self.request_friends(FriendOperation::Refresh);
            }
            Err(error) => {
                self.friends.loading = false;
                self.friends.busy = false;
                self.friends.error = Some(error);
                if let Some(editor) = &mut self.friends.editor {
                    editor.saving = false;
                }
            }
        }
    }
}
