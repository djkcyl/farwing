//! Friends, friend requests and what each friend may do on this account's
//! devices.
use super::*;
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
                FriendResult::Done("好友申请已发送，等待对方接受".into())
            }
            Ok(farwing_protocol::api::FriendRequestOutcome::Accepted { friend }) => {
                FriendResult::Done(format!("已与 {} 成为好友", friend.user.display_name()))
            }
            Err(error) => {
                use crate::account::api::ErrorCode;
                match crate::account::api::failure_code(&error) {
                    Some(ErrorCode::UserNotFound) => anyhow::bail!("没有找到该用户名"),
                    Some(ErrorCode::AlreadyFriends) => anyhow::bail!("你们已经是好友"),
                    _ => return Err(error),
                }
            }
        },
        FriendOperation::Answer(id, accept) => {
            client.answer_friend_request(&id, accept).await?;
            FriendResult::Done(if accept { "已接受好友申请" } else { "已拒绝好友申请" }.into())
        }
        FriendOperation::Cancel(id) => {
            client.cancel_friend_request(&id).await?;
            FriendResult::Done("已撤回好友申请".into())
        }
        FriendOperation::Remove(user_id) => {
            client.remove_friend(&user_id).await?;
            FriendResult::Done("已解除好友关系，双方的授权已撤销".into())
        }
        FriendOperation::LoadGrants(user) => {
            let grants = client.grants(&user.user_id).await?;
            FriendResult::Grants(user, grants)
        }
        FriendOperation::SaveGrants(user_id, grants) => {
            client.set_grants(&user_id, grants).await?;
            FriendResult::Done("权限已保存，正在进行的连接会立即按新权限生效".into())
        }
    })
}

/// Display label of a session capability.
pub(super) fn cap_label(cap: &str) -> &'static str {
    use farwing_protocol::caps;
    match cap {
        caps::VIEW => "观看画面",
        caps::CONTROL => "键鼠控制",
        caps::CLIPBOARD => "剪贴板",
        caps::FILES => "文件传输",
        caps::PORTS => "端口转发",
        caps::POWER => "关机重启与唤醒",
        caps::AUDIO => "接收声音",
        caps::MICROPHONE => "麦克风",
        caps::DISPLAY => "显示设置",
        caps::DIAGNOSTICS => "诊断包",
        caps::ANNOTATION => "批注",
        _ => "未知能力",
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
            self.friends.error = Some("设备后台服务不可用".into());
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
