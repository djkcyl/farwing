use super::*;
use crate::account::api::AssistJoined;
use crate::i18n::t;
use crate::account::assist::{SavedDevice, SavedKind, SavedLists, normalize_connect_id};
use farwing_protocol::api::{AssistPending, FavoriteItem};

impl AuthenticatedClient {
    /// Ask a host for assistance with the controller half of the PAKE.
    pub(crate) async fn assist_join(&self, id: &str, pake: Vec<String>) -> Result<AssistPending> {
        self.request(|api| async move { api.assist_join(id, pake).await })
            .await
    }
    /// Prove the code (or nothing, in confirmation mode) and wait for the
    /// host's decision.
    pub(crate) async fn assist_accept(
        &self,
        request_id: &str,
        key_confirm: String,
    ) -> Result<AssistJoined> {
        self.request(|api| async move { api.assist_accept(request_id, key_confirm).await })
            .await
    }
    /// A saved code the host did not accept is forgotten.
    pub(crate) async fn reject_assist_code(&self, id: &str, code: &str) -> Result<()> {
        let store = crate::account::auth::AssistCodeStore::new(self.session.user_id())?;
        let id = id.to_owned();
        let code = code.to_owned();
        tokio::task::spawn_blocking(move || store.reject_code(&id, &code))
            .await
            .context(t().session.code_clear_interrupted)?
    }
    /// A fresh ticket for an assistance session after its socket was lost.
    pub(crate) async fn assist_refetch(&self, session_id: &str) -> Result<RealtimeTicket> {
        self.request(|api| async move { api.refresh_session_ticket(session_id).await })
            .await
    }
    pub(crate) async fn assist_cancel(&self, request_id: &str) -> Result<()> {
        self.request(|api| async move { api.assist_cancel(request_id).await })
            .await
    }
    pub(crate) async fn assist_saved(&self, kind: SavedKind) -> Result<Vec<SavedDevice>> {
        Ok(self
            .request(|api| async move { api.assist_saved(kind).await })
            .await?
            .into_iter()
            .map(SavedDevice::from)
            .collect())
    }
    pub(crate) async fn assist_lists(&self) -> Result<SavedLists> {
        let (recent, favorites) = tokio::try_join!(
            self.assist_saved(SavedKind::Recent),
            self.assist_saved(SavedKind::Favorites)
        )?;
        let store = crate::account::auth::AssistCodeStore::new(self.session.user_id())?;
        tokio::task::spawn_blocking(move || {
            let mut lists = SavedLists {
                recent,
                favorites,
                code_error: None,
            };
            for device in lists.recent.iter_mut().chain(&mut lists.favorites) {
                match store.load(&device.connect_id, Some(&device.publisher_device_id)) {
                    Ok(Some(saved)) => device.saved_code = saved.code,
                    Ok(None) => {}
                    Err(_) => {
                        lists.code_error = Some(t().session.saved_codes_unreadable.into())
                    }
                }
            }
            lists
        })
        .await
        .context(t().session.code_read_interrupted)
    }

    pub(crate) async fn resolve_assist_code(
        &self,
        id: String,
        code: String,
        publisher_id: Option<String>,
    ) -> Result<(String, Option<String>)> {
        crate::account::assist::validate_connect_code(&code)?;
        if !code.is_empty() {
            return Ok((code, publisher_id));
        }
        let store = crate::account::auth::AssistCodeStore::new(self.session.user_id())?;
        tokio::task::spawn_blocking(move || match store.load(&id, publisher_id.as_deref())? {
            Some(saved) => Ok((saved.code, Some(saved.publisher_id))),
            None => Ok((String::new(), publisher_id)),
        })
        .await
        .context(t().session.code_read_interrupted)?
    }

    pub(crate) async fn remember_assist_code(
        &self,
        id: String,
        publisher_id: String,
        code: String,
    ) -> Result<()> {
        if !self.is_active() {
            bail!("{}", t().session.account_session_ended);
        }
        let store = crate::account::auth::AssistCodeStore::new(self.session.user_id())?;
        let ended = self.ended();
        tokio::task::spawn_blocking(move || {
            if ended.is_cancelled() {
                bail!("{}", t().session.account_session_ended);
            }
            store.save(&id, &publisher_id, code)
        })
        .await
        .context(t().session.code_save_interrupted)?
    }

    pub(crate) async fn save_assist_favorite(
        &self,
        id: &str,
        remark: &str,
        code: Option<String>,
    ) -> Result<()> {
        if let Some(code) = &code {
            crate::account::assist::validate_connect_code(code)?;
        }
        let saved = self.upsert_assist_favorite(id, remark).await?;
        if let Some(code) = code {
            self.remember_assist_code(saved.connect_id, saved.publisher_device_id, code)
                .await
                .context(t().session.favorite_code_not_saved)?;
        }
        Ok(())
    }

    async fn upsert_assist_favorite(&self, id: &str, remark: &str) -> Result<SavedDevice> {
        let id = normalize_connect_id(id)?;
        let remark = remark.trim();
        if remark.chars().any(char::is_control) {
            bail!("{}", t().session.note_control_characters);
        }
        let current = self.assist_saved(SavedKind::Favorites).await?;
        let time = current
            .iter()
            .find(|d| d.connect_id == id)
            .and_then(|d| d.favorited_at)
            .unwrap_or_else(|| chrono::Utc::now().timestamp());
        let item = FavoriteItem {
            connect_id: id.clone(),
            remark: remark.to_owned(),
            favorited_at: time,
        };
        let result = self
            .request(|api| async move { api.assist_save_favorite(item).await })
            .await;
        let desired =
            |d: &SavedDevice| d.connect_id == id && (remark.is_empty() || d.remark == remark);
        match result {
            Ok(reply) => {
                if let Some(saved) = reply.into_iter().map(SavedDevice::from).find(desired) {
                    return Ok(saved);
                }
            }
            Err(error) if super::is_server_failure(&error) => return Err(error),
            _ => {}
        }
        if let Some(saved) = self
            .assist_saved(SavedKind::Favorites)
            .await?
            .into_iter()
            .find(desired)
        {
            return Ok(saved);
        }
        bail!("{}", t().session.favorite_unconfirmed)
    }
    pub(crate) async fn delete_assist_saved(
        &self,
        kind: SavedKind,
        publisher_id: &str,
    ) -> Result<()> {
        crate::account::api::validate_device_id(publisher_id)?;
        let current = self.assist_saved(kind).await?;
        if !current
            .iter()
            .any(|d| d.publisher_device_id == publisher_id)
        {
            return Ok(());
        }
        let removed: Vec<_> = current
            .into_iter()
            .filter(|d| d.publisher_device_id == publisher_id)
            .collect();
        let result = self
            .request(|api| async move { api.assist_delete_saved(kind, publisher_id).await })
            .await;
        if let Err(error) = result {
            if super::is_server_failure(&error) {
                return Err(error);
            }
            if self
                .assist_saved(kind)
                .await?
                .iter()
                .any(|d| d.publisher_device_id == publisher_id)
            {
                bail!("{}", t().session.delete_unconfirmed);
            }
        }
        self.remove_unused_assist_codes(removed).await
    }
    pub(crate) async fn clear_assist_recent(&self) -> Result<()> {
        let previous = self.assist_saved(SavedKind::Recent).await?;
        let result = self
            .request(|api| async move { api.assist_clear_recent().await })
            .await;
        if let Err(error) = result {
            if super::is_server_failure(&error) {
                return Err(error);
            }
            if !self.assist_saved(SavedKind::Recent).await?.is_empty() {
                bail!("{}", t().session.clear_unconfirmed);
            }
        }
        self.remove_unused_assist_codes(previous).await
    }

    async fn remove_unused_assist_codes(&self, candidates: Vec<SavedDevice>) -> Result<()> {
        if candidates.is_empty() {
            return Ok(());
        }
        let (recent, favorites) = tokio::try_join!(
            self.assist_saved(SavedKind::Recent),
            self.assist_saved(SavedKind::Favorites),
        )
        .context(t().session.removed_code_clear_unconfirmed)?;
        let store = crate::account::auth::AssistCodeStore::new(self.session.user_id())?;
        tokio::task::spawn_blocking(move || {
            for device in candidates {
                if !recent
                    .iter()
                    .chain(&favorites)
                    .any(|d| d.connect_id == device.connect_id)
                {
                    store.remove_device(&device)?;
                }
            }
            Ok(())
        })
        .await
        .context(t().session.code_cleanup_interrupted)?
    }
}
