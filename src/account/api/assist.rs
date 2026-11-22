use super::*;
use crate::account::assist::{
    ControlMode, FavoriteItem, JoinReply, SavedKind, SavedReply, normalize_connect_id,
};

const MODE: Contract = Contract {
    path: "/api/v2/room/share/control_mode",
    method: Method::Post,
    json_content_type: true,
    ..DEVICE_LIST
};
const JOIN: Contract = Contract {
    retry: RetryPolicy::ConnectOnly,
    path: "/api/v2/room/join/share/by_code",
    ..MODE
};
const CONFIRM: Contract = Contract {
    retry: RetryPolicy::ConnectOnly,
    path: "/api/v2/room/join/share/by_confirmation",
    timeout: Duration::from_secs(65),
    ..MODE
};
const CANCEL: Contract = Contract {
    retry: RetryPolicy::ConnectOnly,
    path: "/api/v2/room/share/cancel_remote_assist",
    json_content_type: false,
    ..MODE
};
const REFETCH: Contract = Contract {
    retry: RetryPolicy::ConnectOnly,
    path: "/api/v1/room/join/refetch",
    ..MODE
};
const SAVED: Contract = Contract {
    path: "/api/v1/device/share/saved",
    json_content_type: true,
    ..DEVICE_LIST
};
const FAVORITE: Contract = Contract {
    retry: RetryPolicy::ConnectOnly,
    path: "/api/v1/device/share/saved/favorites",
    method: Method::Post,
    ..SAVED
};
const DELETE: Contract = Contract {
    retry: RetryPolicy::ConnectOnly,
    method: Method::Delete,
    ..SAVED
};

pub(super) fn is_join(contract: Contract) -> bool {
    contract == JOIN || contract == CONFIRM
}
pub(super) fn is_sensitive(contract: Contract) -> bool {
    is_join(contract) || contract == REFETCH
}

impl NrdApi {
    pub(crate) async fn assist_mode(&self, id: &str) -> Result<ApiEnvelope<ControlMode>> {
        let id = normalize_connect_id(id)?;
        self.post_json(MODE, MODE.path, &serde_json::json!({"connect_id":id}))
            .await
    }
    pub(crate) async fn assist_join(&self, id: &str, code: &str) -> Result<ApiEnvelope<JoinReply>> {
        let id = normalize_connect_id(id)?;
        let mut response: ApiEnvelope<JoinReply> = self
            .post_json(
                JOIN,
                JOIN.path,
                &serde_json::json!({"connect_id":id,"connect_code":code}),
            )
            .await?;
        if !code.is_empty() {
            response.msg = response.msg.replace(code, "***");
        }
        self.configure_self_hosted_assist(&mut response)?;
        Ok(response)
    }
    pub(crate) async fn assist_confirm(
        &self,
        id: &str,
        control_id: &str,
    ) -> Result<ApiEnvelope<JoinReply>> {
        let id = normalize_connect_id(id)?;
        let mut response = self.post_json(
            CONFIRM,
            CONFIRM.path,
            &serde_json::json!({"connect_id":id,"control_id":control_id}),
        )
        .await?;
        self.configure_self_hosted_assist(&mut response)?;
        Ok(response)
    }
    pub(crate) async fn assist_refetch(&self, token: &str) -> Result<ApiEnvelope<JoinReply>> {
        let mut response: ApiEnvelope<JoinReply> = self
            .post_json(REFETCH, REFETCH.path, &serde_json::json!({"token":token}))
            .await?;
        if !token.is_empty() {
            response.msg = response.msg.replace(token, "***");
        }
        self.configure_self_hosted_assist(&mut response)?;
        Ok(response)
    }

    fn configure_self_hosted_assist(&self, response: &mut ApiEnvelope<JoinReply>) -> Result<()> {
        if let (Backend::SelfHosted(api), Some(reply)) = (&self.backend, &mut response.data)
            && !reply.token.is_empty()
        {
            reply.signaling_list = vec![api.signaling_url()?];
        }
        Ok(())
    }
    pub(crate) async fn assist_cancel(&self, id: &str) -> Result<ApiEnvelope<serde_json::Value>> {
        let id = normalize_connect_id(id)?;
        let response = self
            .post_json(CANCEL, CANCEL.path, &serde_json::json!({"connect_id":id}))
            .await?;
        Ok(common_response(response))
    }
    pub(crate) async fn assist_saved(&self, kind: SavedKind) -> Result<ApiEnvelope<SavedReply>> {
        self.send(
            SAVED,
            &format!("{}/{}", SAVED.path, kind.path()),
            Vec::new(),
        )
        .await
    }
    pub(crate) async fn assist_save_favorite(
        &self,
        item: &FavoriteItem<'_>,
    ) -> Result<ApiEnvelope<SavedReply>> {
        normalize_connect_id(item.connect_id)?;
        self.post_json(
            FAVORITE,
            FAVORITE.path,
            &serde_json::json!({"favorites":[item],"skip_invalid":true}),
        )
        .await
    }
    pub(crate) async fn assist_delete_saved(
        &self,
        kind: SavedKind,
        publisher_id: &str,
    ) -> Result<ApiEnvelope<serde_json::Value>> {
        validate_device_id(publisher_id)?;
        let response = self
            .send(
                DELETE,
                &format!("{}/{}/{}", SAVED.path, kind.path(), publisher_id),
                Vec::new(),
            )
            .await?;
        Ok(common_response(response))
    }
    pub(crate) async fn assist_clear_recent(&self) -> Result<ApiEnvelope<serde_json::Value>> {
        let response = self
            .send(DELETE, &format!("{}/recent", SAVED.path), Vec::new())
            .await?;
        Ok(common_response(response))
    }
}

fn common_response(mut response: ApiEnvelope<serde_json::Value>) -> ApiEnvelope<serde_json::Value> {
    if response.code == 0 && response.data.is_none() {
        response.data = Some(serde_json::Value::Null);
    }
    response
}
