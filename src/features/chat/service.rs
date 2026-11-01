//! The device center's chat (`docs/protocol/chat-design.md` §6, §12.1): the
//! chat connection with its snapshot and pushes, the requests the page
//! makes, images (emoji, avatars, icons, thumbnails) as textures, and the
//! local cache. Everything runs on the worker's runtime; the page reads an
//! immutable view each frame.
use super::cache::Cache;
use super::state::{Answer, ChatState, Connection, Entry, Notice};
use crate::account::client::AuthenticatedClient;
use anyhow::{Context, Result};
use farwing_protocol::chat::{
    Attachment, AttachmentUpload, Dm, GroupState, Id, MediaToken, Message, MessagePage, Prefs, Snapshot,
    Status,
};
use farwing_protocol::realtime::{Envelope, Hello, Ping, types as rt};
use futures_util::{SinkExt, StreamExt};
use reqwest::Method;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{Notify, mpsc};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_util::sync::CancellationToken;

const PAGE: u32 = 50;
const IDLE_AFTER: Duration = Duration::from_secs(10 * 60);
const TYPING_EVERY: Duration = Duration::from_secs(4);
/// Largest image fetched for showing (thumbnails, avatars, full images).
const MAX_IMAGE: usize = 32 * 1024 * 1024;

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// A file being attached in the composer.
#[derive(Clone, Debug)]
pub(crate) struct Upload {
    pub key: u64,
    pub name: String,
    pub done: Option<Attachment>,
    pub error: Option<String>,
}

/// An image to show.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum ImageKey {
    Emoji(String),
    Avatar(String, String),
    Icon(Id, String),
    Thumb(Id),
    Full(Id, String),
}

enum Slot {
    Loading,
    Ready(egui::TextureHandle),
    Failed,
}

struct Inner {
    state: Mutex<Arc<ChatState>>,
    client: Arc<AuthenticatedClient>,
    runtime: tokio::runtime::Handle,
    ctx: egui::Context,
    stop: CancellationToken,
    take_over: Notify,
    outgoing: mpsc::UnboundedSender<Envelope>,
    cache: Option<Cache>,
    images: Mutex<HashMap<ImageKey, Slot>>,
    notices: Mutex<Vec<Notice>>,
    uploads: Mutex<HashMap<Id, Vec<Upload>>>,
    hidden: AtomicBool,
    idle: AtomicBool,
    last_input: Mutex<Instant>,
    typing_sent: Mutex<HashMap<Id, Instant>>,
    /// Groups whose member list is on its way (pages ask every frame).
    members_loading: Mutex<std::collections::HashSet<Id>>,
    counter: AtomicU64,
}

#[derive(Clone)]
pub(crate) struct ChatService {
    inner: Arc<Inner>,
}

impl ChatService {
    /// Start chat for the signed-in account.
    pub(crate) fn start(client: Arc<AuthenticatedClient>, runtime: tokio::runtime::Handle, ctx: egui::Context) -> Self {
        let (outgoing, rx) = mpsc::unbounded_channel();
        let cache = client.server_config().and_then(|server| {
            crate::product::local_data_dir()
                .ok()
                .map(|dir| Cache::new(&dir, &server.url("/"), client.user_id()))
        });
        let inner = Arc::new(Inner {
            state: Mutex::new(Arc::new(ChatState::default())),
            client,
            runtime: runtime.clone(),
            ctx,
            stop: CancellationToken::new(),
            take_over: Notify::new(),
            outgoing,
            cache,
            images: Mutex::new(HashMap::new()),
            notices: Mutex::new(Vec::new()),
            uploads: Mutex::new(HashMap::new()),
            hidden: AtomicBool::new(false),
            idle: AtomicBool::new(false),
            last_input: Mutex::new(Instant::now()),
            typing_sent: Mutex::new(HashMap::new()),
            members_loading: Mutex::new(std::collections::HashSet::new()),
            counter: AtomicU64::new(1),
        });
        runtime.spawn(run(Arc::clone(&inner), rx));
        runtime.spawn(tick(Arc::clone(&inner)));
        runtime.spawn({
            let inner = Arc::clone(&inner);
            async move {
                if let Ok(bytes) = inner.client.media_get("/emoji/index.json", 4 << 20).await
                    && let Ok(list) = serde_json::from_slice::<Vec<super::emoji::Entry>>(&bytes)
                {
                    update(&inner, |s| s.emoji = Arc::new(list));
                }
            }
        });
        Self { inner }
    }

    pub(crate) fn stop(&self) {
        self.inner.stop.cancel();
    }

    /// The server's address (invite links).
    pub(crate) fn server_base(&self) -> String {
        self.inner
            .client
            .server_config()
            .map(|s| s.url("/").trim_end_matches('/').to_owned())
            .unwrap_or_default()
    }

    /// The state as of now (cheap: shared until it changes).
    pub(crate) fn view(&self) -> Arc<ChatState> {
        Arc::clone(&lock(&self.inner.state))
    }

    pub(crate) fn take_notices(&self) -> Vec<Notice> {
        std::mem::take(&mut *lock(&self.inner.notices))
    }

    pub(crate) fn take_errors(&self) -> Vec<String> {
        let mut out = Vec::new();
        update(&self.inner, |s| out = std::mem::take(&mut s.errors));
        out
    }

    pub(crate) fn uploads(&self, channel: Id) -> Vec<Upload> {
        lock(&self.inner.uploads).get(&channel).cloned().unwrap_or_default()
    }

    pub(crate) fn remove_upload(&self, channel: Id, key: u64) {
        if let Some(list) = lock(&self.inner.uploads).get_mut(&channel) {
            list.retain(|u| u.key != key);
        }
    }

    /// The window is minimized or behind others (notifications then show
    /// for the open channel too).
    pub(crate) fn set_hidden(&self, hidden: bool) {
        self.inner.hidden.store(hidden, Ordering::Relaxed);
    }

    /// The user did something: not idle.
    pub(crate) fn activity(&self) {
        *lock(&self.inner.last_input) = Instant::now();
        if self.inner.idle.swap(false, Ordering::Relaxed) {
            let _ = self.inner.outgoing.send(Envelope::new("chat.idle", json!({"idle": false})));
        }
    }

    pub(crate) fn take_over(&self) {
        self.inner.take_over.notify_one();
    }

    pub(crate) fn set_focus(&self, channel: Option<Id>) {
        if self.view().focus != channel {
            update(&self.inner, |s| s.focus = channel);
        }
    }

    pub(crate) fn jump(&self, channel: Id, message: Id) {
        update(&self.inner, |s| s.jump = Some((channel, message, Instant::now())));
        self.load_around(channel, message);
    }

    fn spawn<F>(&self, f: impl FnOnce(Arc<Inner>) -> F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        self.inner.runtime.spawn(f(Arc::clone(&self.inner)));
    }

    // -------------------------------------------------------------- messages

    pub(crate) fn load_latest(&self, channel: Id) {
        let view = self.view();
        if view.messages.get(&channel).is_some_and(|b| b.loading) {
            return;
        }
        let cached = if view.messages.contains_key(&channel) {
            None
        } else {
            self.inner.cache.as_ref().and_then(|c| c.load(channel))
        };
        update(&self.inner, |s| {
            let b = s.messages.entry(channel).or_default();
            if let Some(list) = cached {
                b.list = list.into_iter().map(entry).collect();
                b.cached = true;
            }
            b.loading = true;
        });
        self.spawn(move |inner| async move {
            let path = format!("/api/v2/chat/channels/{channel}/messages?limit={PAGE}");
            match get::<MessagePage>(&inner, &path).await {
                Ok(page) => {
                    if let Some(cache) = &inner.cache {
                        cache.store(channel, &page.messages);
                    }
                    let full = page.messages.len() == PAGE as usize;
                    update(&inner, |s| {
                        let b = s.messages.entry(channel).or_default();
                        let pending: Vec<Entry> = b.list.drain(..).filter(|e| e.sending || e.failed.is_some()).collect();
                        b.list = page.messages.into_iter().map(entry).collect();
                        b.list.extend(pending);
                        b.older = full;
                        b.newer = false;
                        b.loading = false;
                        b.cached = false;
                    });
                }
                Err(e) => update(&inner, |s| {
                    if let Some(b) = s.messages.get_mut(&channel) {
                        b.loading = false;
                    }
                    s.errors.push(e);
                }),
            }
        });
    }

    pub(crate) fn load_older(&self, channel: Id) {
        let view = self.view();
        let Some(b) = view.messages.get(&channel) else { return };
        let Some(first) = b.list.iter().find(|e| !e.sending).map(|e| e.message.id) else { return };
        if b.loading || !b.older || b.cached {
            return;
        }
        update(&self.inner, |s| s.messages.entry(channel).or_default().loading = true);
        self.spawn(move |inner| async move {
            let path = format!("/api/v2/chat/channels/{channel}/messages?before={first}&limit={PAGE}");
            let result = get::<MessagePage>(&inner, &path).await;
            update(&inner, |s| {
                let b = s.messages.entry(channel).or_default();
                b.loading = false;
                match result {
                    Ok(page) => {
                        b.older = page.messages.len() == PAGE as usize;
                        let mut list: Vec<Entry> = page.messages.into_iter().map(entry).collect();
                        list.append(&mut b.list);
                        b.list = list;
                    }
                    Err(e) => s.errors.push(e),
                }
            });
        });
    }

    pub(crate) fn load_newer(&self, channel: Id) {
        let view = self.view();
        let Some(b) = view.messages.get(&channel) else { return };
        let Some(last) = b.list.iter().rev().find(|e| !e.sending).map(|e| e.message.id) else { return };
        if b.loading || !b.newer {
            return;
        }
        update(&self.inner, |s| s.messages.entry(channel).or_default().loading = true);
        self.spawn(move |inner| async move {
            let path = format!("/api/v2/chat/channels/{channel}/messages?after={last}&limit={PAGE}");
            let result = get::<MessagePage>(&inner, &path).await;
            update(&inner, |s| {
                let b = s.messages.entry(channel).or_default();
                b.loading = false;
                match result {
                    Ok(page) => {
                        b.newer = page.messages.len() == PAGE as usize;
                        b.list.extend(page.messages.into_iter().map(entry));
                    }
                    Err(e) => s.errors.push(e),
                }
            });
        });
    }

    pub(crate) fn load_around(&self, channel: Id, id: Id) {
        if self.view().messages.get(&channel).is_some_and(|b| b.list.iter().any(|e| e.message.id == id)) {
            return;
        }
        update(&self.inner, |s| s.messages.entry(channel).or_default().loading = true);
        self.spawn(move |inner| async move {
            let path = format!("/api/v2/chat/channels/{channel}/messages?around={id}&limit={PAGE}");
            let result = get::<MessagePage>(&inner, &path).await;
            update(&inner, |s| {
                let last = s.channel(channel).and_then(|(c, _, _)| c.last_message_id);
                let b = s.messages.entry(channel).or_default();
                b.loading = false;
                match result {
                    Ok(page) => {
                        let newest = page.messages.last().map(|m| m.id);
                        b.newer = matches!((newest, last), (Some(n), Some(l)) if n < l);
                        b.older = true;
                        b.cached = false;
                        b.list = page.messages.into_iter().map(entry).collect();
                    }
                    Err(e) => s.errors.push(e),
                }
            });
        });
    }

    /// Send a message: shown at once, confirmed by the server's answer.
    pub(crate) fn send(&self, channel: Id, content: String, reply: Option<Message>, mention_reply: bool, attachments: Vec<Id>) {
        let Some(me) = self.view().snap.as_ref().map(|s| s.me.clone()) else { return };
        let n = self.inner.counter.fetch_add(1, Ordering::Relaxed);
        let nonce = format!("n{}{n}", farwing_protocol::chat::Id::EPOCH_MS ^ chrono::Utc::now().timestamp_millis());
        let local = Message {
            id: Id(i64::MAX - 1_000_000 + n as i64),
            channel_id: channel,
            author: me,
            content: content.clone(),
            reply: reply.as_ref().map(|r| farwing_protocol::chat::Reply {
                id: r.id,
                author: r.author.clone(),
                content: r.content.chars().take(120).collect(),
                deleted: false,
                attachments: 0,
            }),
            created_at: chrono::Utc::now().timestamp_millis(),
            nonce: nonce.clone(),
            ..Default::default()
        };
        update(&self.inner, |s| {
            s.messages.entry(channel).or_default().list.push(Entry {
                message: local,
                sending: true,
                failed: None,
            })
        });
        let body = json!({
            "content": content,
            "reply_to": reply.map(|r| r.id),
            "mention_reply": mention_reply,
            "attachments": attachments,
            "nonce": nonce,
        });
        self.spawn(move |inner| async move {
            let path = format!("/api/v2/chat/channels/{channel}/messages");
            match call::<Message>(&inner, Method::POST, &path, Some(body)).await {
                Ok(mut sent) => {
                    sent.nonce = nonce.clone();
                    let hidden = inner.hidden.load(Ordering::Relaxed);
                    update(&inner, |s| {
                        s.add_message(sent, hidden);
                    });
                }
                Err(e) => update(&inner, |s| {
                    if let Some(b) = s.messages.get_mut(&channel) {
                        for x in &mut b.list {
                            if x.sending && x.message.nonce == nonce {
                                x.sending = false;
                                x.failed = Some(e.clone());
                            }
                        }
                    }
                }),
            }
        });
    }

    pub(crate) fn resend(&self, channel: Id, failed: &Entry) {
        let nonce = failed.message.nonce.clone();
        update(&self.inner, |s| {
            if let Some(b) = s.messages.get_mut(&channel) {
                b.list.retain(|x| x.message.nonce != nonce || !x.failed.is_some());
            }
        });
        self.send(channel, failed.message.content.clone(), None, false, Vec::new());
    }

    pub(crate) fn discard(&self, channel: Id, failed: &Entry) {
        let nonce = failed.message.nonce.clone();
        update(&self.inner, |s| {
            if let Some(b) = s.messages.get_mut(&channel) {
                b.list.retain(|x| x.message.nonce != nonce || x.failed.is_none());
            }
        });
    }

    pub(crate) fn edit(&self, message: Id, content: String) {
        self.fire(Method::PATCH, format!("/api/v2/chat/messages/{message}"), Some(json!({"content": content})));
    }

    pub(crate) fn delete(&self, message: Id) {
        self.fire(Method::DELETE, format!("/api/v2/chat/messages/{message}"), None);
    }

    pub(crate) fn pin(&self, message: Id, pinned: bool) {
        let method = if pinned { Method::PUT } else { Method::DELETE };
        self.fire(method, format!("/api/v2/chat/messages/{message}/pin"), pinned.then(|| json!({})));
    }

    /// A request whose effect arrives as a push; failures are shown.
    pub(crate) fn fire(&self, method: Method, path: String, body: Option<Value>) {
        self.spawn(move |inner| async move {
            if let Err(e) = call::<Value>(&inner, method, &path, body).await {
                update(&inner, |s| s.errors.push(e));
            }
        });
    }

    /// A request whose answer the page waits for, under `key`.
    pub(crate) fn ask(&self, key: &str, method: Method, path: String, body: Option<Value>) {
        let key = key.to_owned();
        update(&self.inner, |s| {
            s.answers.insert(key.clone(), Answer::Waiting);
        });
        self.spawn(move |inner| async move {
            let result = call::<Value>(&inner, method, &path, body).await;
            update(&inner, |s| {
                // A group or DM in the answer is the newest state of it.
                if let Ok(v) = &result {
                    absorb(s, v);
                }
                s.answers.insert(
                    key,
                    match result {
                        Ok(v) => Answer::Done(v),
                        Err(e) => Answer::Failed(e),
                    },
                );
            });
        });
    }

    pub(crate) fn forget(&self, key: &str) {
        if self.view().answers.contains_key(key) {
            update(&self.inner, |s| {
                s.answers.remove(key);
            });
        }
    }

    /// Read up to the newest message of `channel`.
    pub(crate) fn mark_read(&self, channel: Id) {
        let view = self.view();
        let Some((c, _, _)) = view.channel(channel) else { return };
        let Some(last) = c.last_message_id else { return };
        let read = view.reads.get(&channel);
        if read.is_some_and(|r| r.last_read_id >= last && r.mentions == 0) {
            return;
        }
        update(&self.inner, |s| {
            s.reads.insert(
                channel,
                farwing_protocol::chat::ReadState {
                    channel_id: channel,
                    last_read_id: last,
                    mentions: 0,
                },
            );
        });
        self.spawn(move |inner| async move {
            let _ = call::<Value>(
                &inner,
                Method::POST,
                &format!("/api/v2/chat/channels/{channel}/read"),
                Some(json!({"message_id": last})),
            )
            .await;
        });
    }

    pub(crate) fn typing(&self, channel: Id) {
        let mut sent = lock(&self.inner.typing_sent);
        if sent.get(&channel).is_some_and(|t| t.elapsed() < TYPING_EVERY) {
            return;
        }
        sent.insert(channel, Instant::now());
        let _ = self.inner.outgoing.send(Envelope::new("chat.typing", json!({"channel_id": channel})));
    }

    pub(crate) fn load_members(&self, group: Id) {
        if !lock(&self.inner.members_loading).insert(group) {
            return;
        }
        self.spawn(move |inner| async move {
            let all = load_member_pages(&inner, group).await;
            lock(&inner.members_loading).remove(&group);
            if let Some(all) = all {
                update(&inner, |s| {
                    s.members.insert(group, all);
                });
            }
        });
    }

    pub(crate) fn set_status(&self, status: Status) {
        update(&self.inner, |s| {
            if let Some(snap) = &mut s.snap {
                snap.status = status;
            }
        });
        self.fire(Method::PUT, "/api/v2/me/status".into(), Some(json!({"status": status})));
    }

    pub(crate) fn save_prefs(&self, prefs: Prefs) {
        update(&self.inner, |s| {
            if let Some(snap) = &mut s.snap {
                snap.prefs = prefs.clone();
            }
        });
        self.fire(Method::PUT, "/api/v2/chat/prefs".into(), Some(serde_json::to_value(&prefs).unwrap_or_default()));
    }

    /// An avatar or group icon from an image file's bytes.
    pub(crate) fn upload_square(&self, path: String, bytes: Vec<u8>) {
        self.spawn(move |inner| async move {
            if let Err(e) = inner
                .client
                .chat_bytes(Method::PUT, &path, bytes::Bytes::from(bytes))
                .await
                .map_err(|e| error_text(&e))
            {
                update(&inner, |s| s.errors.push(e));
            }
        });
    }

    /// Attach files (name, bytes) to the composer of `channel`.
    pub(crate) fn attach(&self, channel: Id, files: Vec<(String, Vec<u8>)>) {
        let limit = self.view().snap.as_ref().map_or(10, |s| s.limits.max_attachments as usize);
        let max = self.view().snap.as_ref().map_or(u64::MAX, |s| s.limits.max_attachment_bytes);
        for (name, bytes) in files {
            {
                let uploads = lock(&self.inner.uploads);
                if uploads.get(&channel).map_or(0, Vec::len) >= limit {
                    update(&self.inner, |s| s.errors.push((crate::i18n::t().chat.too_many_files)(limit)));
                    break;
                }
            }
            if bytes.len() as u64 > max {
                update(&self.inner, |s| s.errors.push((crate::i18n::t().chat.too_large)(&name)));
                continue;
            }
            let key = self.inner.counter.fetch_add(1, Ordering::Relaxed);
            lock(&self.inner.uploads).entry(channel).or_default().push(Upload {
                key,
                name: name.clone(),
                done: None,
                error: None,
            });
            self.spawn(move |inner| async move {
                let result = upload(&inner, channel, &name, bytes).await;
                if let Some(u) = lock(&inner.uploads).get_mut(&channel).and_then(|l| l.iter_mut().find(|u| u.key == key)) {
                    match result {
                        Ok(a) => u.done = Some(a),
                        Err(e) => u.error = Some(e),
                    }
                }
                inner.ctx.request_repaint();
            });
        }
    }

    pub(crate) fn take_uploads(&self, channel: Id) -> Option<Vec<Id>> {
        let mut uploads = lock(&self.inner.uploads);
        let list = uploads.get(&channel)?;
        if list.iter().any(|u| u.done.is_none() && u.error.is_none()) {
            return None;
        }
        let ids = list.iter().filter_map(|u| u.done.as_ref().map(|a| a.id)).collect();
        uploads.remove(&channel);
        Some(ids)
    }

    /// Save an attachment to the Downloads folder.
    pub(crate) fn download(&self, a: &Attachment) {
        let token = self.view().token.clone();
        let (id, name) = (a.id, a.name.clone());
        self.spawn(move |inner| async move {
            let Some(token) = token else { return };
            let dir = downloads_dir();
            let dest = unique(&dir, &name);
            let path = format!("/media/attachments/{id}/{}?download=1&t={}", urlencode(&name), urlencode(&token.token));
            let message = match inner.client.media_download(&path, &dest).await {
                Ok(()) => (crate::i18n::t().chat.saved_to)(&dest.display().to_string()),
                Err(e) => error_text(&e),
            };
            update(&inner, |s| s.errors.push(message));
        });
    }

    /// The texture of an image, starting to fetch it if needed.
    pub(crate) fn image(&self, key: &ImageKey) -> Option<egui::TextureHandle> {
        let mut images = lock(&self.inner.images);
        match images.get(key) {
            Some(Slot::Ready(t)) => return Some(t.clone()),
            Some(_) => return None,
            None => {}
        }
        let token = self.view().token.as_ref().map(|t| t.token.clone());
        let path = match key {
            ImageKey::Emoji(k) => format!("/emoji/{k}.png"),
            other => {
                let token = urlencode(&token?);
                match other {
                    ImageKey::Avatar(user, v) => format!("/media/avatars/{user}?v={v}&t={token}"),
                    ImageKey::Icon(group, v) => format!("/media/icons/{group}?v={v}&t={token}"),
                    ImageKey::Thumb(id) => format!("/media/thumbs/{id}?t={token}"),
                    ImageKey::Full(id, name) => format!("/media/attachments/{id}/{}?t={token}", urlencode(name)),
                    ImageKey::Emoji(_) => unreachable!(),
                }
            }
        };
        images.insert(key.clone(), Slot::Loading);
        drop(images);
        let key = key.clone();
        self.spawn(move |inner| async move {
            let slot = match fetch_image(&inner, &key, &path).await {
                Some(t) => Slot::Ready(t),
                None => Slot::Failed,
            };
            lock(&inner.images).insert(key, slot);
            inner.ctx.request_repaint();
        });
        None
    }
}

fn entry(m: Message) -> Entry {
    Entry {
        message: m,
        sending: false,
        failed: None,
    }
}

/// A group or direct message in a call's answer updates the state at once.
fn absorb(s: &mut ChatState, v: &Value) {
    if let Ok(g) = serde_json::from_value::<GroupState>(v.clone())
        && g.group.id.0 != 0
    {
        let _ = s.apply(farwing_protocol::chat::types::GROUP, v.clone(), false);
    } else if let Ok(dm) = serde_json::from_value::<Dm>(v.clone())
        && dm.channel.id.0 != 0
    {
        let _ = s.apply(farwing_protocol::chat::types::DM, v.clone(), false);
    } else if let Some(joined) = v.get("joined").filter(|j| !j.is_null()) {
        let _ = s.apply(farwing_protocol::chat::types::GROUP, joined.clone(), false);
    }
}

fn update(inner: &Inner, f: impl FnOnce(&mut ChatState)) {
    {
        let mut guard = lock(&inner.state);
        f(Arc::make_mut(&mut guard));
    }
    inner.ctx.request_repaint();
}

/// A failure in the interface's words (a server's failure is translated by
/// its code and reason, never its message).
fn error_text(e: &anyhow::Error) -> String {
    match e.downcast_ref::<crate::account::api::ApiFailure>() {
        Some(failure) => failure.to_string(),
        None => e.to_string(),
    }
}

async fn call<T: serde::de::DeserializeOwned>(inner: &Inner, method: Method, path: &str, body: Option<Value>) -> Result<T, String> {
    let v = inner.client.chat(method, path, body).await.map_err(|e| error_text(&e))?;
    serde_json::from_value(v).map_err(|e| e.to_string())
}

async fn get<T: serde::de::DeserializeOwned>(inner: &Inner, path: &str) -> Result<T, String> {
    call(inner, Method::GET, path, None).await
}

async fn upload(inner: &Inner, channel: Id, name: &str, bytes: Vec<u8>) -> Result<Attachment, String> {
    let mime = mime_of(name);
    let slot: AttachmentUpload = call(
        inner,
        Method::POST,
        "/api/v2/chat/attachments",
        Some(json!({"channel_id": channel, "name": name, "mime": mime, "size": bytes.len()})),
    )
    .await?;
    let v = inner
        .client
        .media_put(&slot.upload, bytes::Bytes::from(bytes))
        .await
        .map_err(|e| error_text(&e))?;
    serde_json::from_value(v).map_err(|e| e.to_string())
}

pub(crate) fn mime_of(name: &str) -> &'static str {
    match name.rsplit('.').next().unwrap_or("").to_ascii_lowercase().as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "txt" | "log" | "md" => "text/plain",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        _ => "application/octet-stream",
    }
}

async fn fetch_image(inner: &Inner, key: &ImageKey, path: &str) -> Option<egui::TextureHandle> {
    // Emoji images are public and kept on disk.
    let disk = match key {
        ImageKey::Emoji(k) => crate::product::local_data_dir().ok().map(|d| d.join("cache").join("emoji").join(format!("{k}.png"))),
        _ => None,
    };
    let bytes = match disk.as_ref().and_then(|p| std::fs::read(p).ok()) {
        Some(b) => b,
        None => {
            let b = inner.client.media_get(path, MAX_IMAGE).await.ok()?.to_vec();
            if let Some(p) = &disk {
                let _ = std::fs::create_dir_all(p.parent()?);
                let _ = std::fs::write(p, &b);
            }
            b
        }
    };
    let decoded = tokio::task::spawn_blocking(move || image::load_from_memory(&bytes).ok().map(|i| i.to_rgba8()))
        .await
        .ok()??;
    let size = [decoded.width() as usize, decoded.height() as usize];
    let image = egui::ColorImage::from_rgba_unmultiplied(size, decoded.as_raw());
    Some(inner.ctx.load_texture(format!("chat-{key:?}"), image, egui::TextureOptions::LINEAR))
}

fn downloads_dir() -> std::path::PathBuf {
    std::env::var_os("USERPROFILE")
        .map(|p| std::path::PathBuf::from(p).join("Downloads"))
        .filter(|p| p.is_dir())
        .unwrap_or_else(std::env::temp_dir)
}

fn unique(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
    let clean: String = name.chars().map(|c| if "\\/:*?\"<>|".contains(c) || c.is_control() { '_' } else { c }).collect();
    let first = dir.join(&clean);
    if !first.exists() {
        return first;
    }
    let (stem, ext) = match clean.rsplit_once('.') {
        Some((s, e)) => (s.to_owned(), format!(".{e}")),
        None => (clean.clone(), String::new()),
    };
    (2..1000)
        .map(|i| dir.join(format!("{stem} ({i}){ext}")))
        .find(|p| !p.exists())
        .unwrap_or(first)
}

pub(crate) fn urlencode(text: &str) -> String {
    text.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
                char::from(b).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// Idle reports, typing expiry, the media token.
async fn tick(inner: Arc<Inner>) {
    let mut every = tokio::time::interval(Duration::from_secs(2));
    let mut token_at: Option<Instant> = None;
    loop {
        tokio::select! {
            _ = inner.stop.cancelled() => return,
            _ = every.tick() => {}
        }
        let idle = lock(&inner.last_input).elapsed() > IDLE_AFTER;
        if idle && !inner.idle.swap(true, Ordering::Relaxed) {
            let _ = inner.outgoing.send(Envelope::new("chat.idle", json!({"idle": true})));
        }
        let view = Arc::clone(&lock(&inner.state));
        if !view.typing.is_empty() {
            update(&inner, |s| s.expire_typing());
        }
        let stale = token_at.is_none_or(|t| t.elapsed() > Duration::from_secs(12 * 3600));
        if view.connection == Connection::Ready && stale {
            if let Ok(token) = inner.client.chat(Method::POST, "/api/v2/chat/media-token", None).await
                && let Ok(token) = serde_json::from_value::<MediaToken>(token)
            {
                token_at = Some(Instant::now());
                update(&inner, |s| s.token = Some(token));
            }
        }
    }
}

enum Ended {
    Superseded,
    Retry,
}

/// The chat connection, reconnecting until stopped.
async fn run(inner: Arc<Inner>, mut outgoing: mpsc::UnboundedReceiver<Envelope>) {
    let mut delay = Duration::from_secs(1);
    loop {
        if inner.stop.is_cancelled() {
            return;
        }
        update(&inner, |s| {
            s.connection = if s.snap.is_some() { Connection::Offline } else { Connection::Connecting }
        });
        match connect(&inner, &mut outgoing, &mut delay).await {
            Ok(Ended::Superseded) => {
                update(&inner, |s| s.connection = Connection::Elsewhere);
                tokio::select! {
                    _ = inner.stop.cancelled() => return,
                    _ = inner.take_over.notified() => continue,
                }
            }
            Ok(Ended::Retry) => {}
            Err(e) => tracing::debug!(error = %format!("{e:#}"), "chat connection failed"),
        }
        tokio::select! {
            _ = inner.stop.cancelled() => return,
            _ = tokio::time::sleep(delay) => {}
        }
        delay = (delay * 2).min(Duration::from_secs(30));
    }
}

async fn connect(inner: &Arc<Inner>, outgoing: &mut mpsc::UnboundedReceiver<Envelope>, delay: &mut Duration) -> Result<Ended> {
    let ticket = inner.client.chat(Method::POST, "/api/v2/chat/ticket", None).await?;
    let code = ticket["ticket"].as_str().context("chat ticket")?.to_owned();
    let endpoint = ticket["endpoints"][0].as_str().context("chat endpoint")?.to_owned();
    let server = inner.client.server_config().context("no server")?;
    let tls = server.tls(false)?;
    let request = crate::transport::signal::realtime_request(&endpoint, None, &code)?;
    let socket = crate::transport::signal::connect_socket(request, tls).await?;
    let (mut sink, mut stream) = socket.split();
    let text = |m: WsMessage| match m {
        WsMessage::Text(t) => Some(t.to_string()),
        _ => None,
    };
    let hello: Hello = loop {
        let frame = tokio::time::timeout(Duration::from_secs(10), stream.next()).await?.context("closed")??;
        if let Some(t) = text(frame)
            && let Ok(env) = Envelope::decode(&t)
            && env.kind == rt::HELLO
        {
            break env.body()?;
        }
    };
    let idle = inner.idle.load(Ordering::Relaxed);
    let request = Envelope::new("chat.hello", json!({"idle": idle})).with_id("h");
    sink.send(WsMessage::Text(request.encode().into())).await?;
    let snap: Snapshot = loop {
        let frame = tokio::time::timeout(Duration::from_secs(15), stream.next()).await?.context("closed")??;
        if let Some(t) = text(frame)
            && let Ok(env) = Envelope::decode(&t)
            && env.kind == rt::ACK
            && env.re.as_deref() == Some("h")
        {
            break env.body()?;
        }
    };
    *delay = Duration::from_secs(1);
    update(inner, |s| {
        s.presences = snap.presences.iter().map(|p| (p.user_id.clone(), p.status)).collect();
        s.presences.insert(snap.me.user_id.clone(), snap.status);
        s.reads = snap.read_states.iter().map(|r| (r.channel_id, *r)).collect();
        // Loaded channels may have missed messages: they load again.
        s.messages.retain(|_, b| b.cached);
        s.members.clear();
        s.snap = Some(snap);
        s.connection = Connection::Ready;
    });
    tracing::info!("chat connected");
    let limit = Duration::from_millis(u64::from(hello.heartbeat_ms) + u64::from(hello.idle_timeout_ms)).max(Duration::from_secs(10));
    loop {
        tokio::select! {
            _ = inner.stop.cancelled() => {
                let _ = sink.send(WsMessage::Text(Envelope::new(rt::BYE, json!({})).encode().into())).await;
                let _ = sink.close().await;
                return Ok(Ended::Retry);
            }
            env = outgoing.recv() => {
                let Some(env) = env else { return Ok(Ended::Retry) };
                sink.send(WsMessage::Text(env.encode().into())).await?;
            }
            frame = tokio::time::timeout(limit, stream.next()) => {
                let frame = match frame {
                    Ok(Some(Ok(f))) => f,
                    Ok(Some(Err(e))) => return Err(e.into()),
                    Ok(None) | Err(_) => return Ok(Ended::Retry),
                };
                match frame {
                    WsMessage::Text(t) => {
                        let Ok(env) = Envelope::decode(&t) else { continue };
                        match env.kind.as_str() {
                            rt::PING => {
                                let t = env.data.get("t").and_then(Value::as_i64).unwrap_or(0);
                                sink.send(WsMessage::Text(Envelope::new(rt::PONG, Ping { t }).encode().into())).await?;
                            }
                            rt::PONG | rt::ACK | rt::ERROR => {}
                            kind => {
                                let hidden = inner.hidden.load(Ordering::Relaxed);
                                let mut notice = None;
                                update(inner, |s| notice = s.apply(kind, env.data.clone(), hidden));
                                if let Some(n) = notice {
                                    lock(&inner.notices).push(n);
                                }
                            }
                        }
                    }
                    WsMessage::Close(frame) => {
                        let code = frame.map(|f| u16::from(f.code));
                        tracing::info!(?code, "chat connection closed");
                        return Ok(match code {
                            Some(4001) => Ended::Superseded,
                            _ => Ended::Retry,
                        });
                    }
                    _ => {}
                }
            }
        }
    }
}

/// Every page of a group's members.
async fn load_member_pages(inner: &Arc<Inner>, group: Id) -> Option<Vec<farwing_protocol::chat::Member>> {
    let mut all = Vec::new();
    let mut after = String::new();
    loop {
        let path = format!("/api/v2/chat/groups/{group}/members?limit=1000&after={after}");
        let page = get::<farwing_protocol::chat::MemberPage>(inner, &path).await.ok()?;
        let n = page.members.len();
        after = page.members.last().map(|m| m.user.user_id.clone()).unwrap_or_default();
        all.extend(page.members);
        if n < 1000 {
            return Some(all);
        }
    }
}
