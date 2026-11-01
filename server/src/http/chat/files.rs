//! Attachments, thumbnails, avatars, group icons and emoji images
//! (`docs/protocol/chat-design.md` §2.4, §12.3). Uploads of attachments
//! and every download go to `/media/...` with a token in the query, so a
//! browser can load them as images.

use super::{audit, id, mine_with, not_found};
use crate::chat::media;
use crate::chat::store::{self, AttachmentRow};
use crate::chat::{access, next_id, push};
use crate::crypto::token_hash;
use crate::db::now_s;
use crate::error::{ApiError, ApiResult};
use crate::http::auth::AccountAuth;
use crate::http::App;
use axum::Json;
use axum::body::Body;
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use farwing_protocol::ErrorCode;
use farwing_protocol::api::Empty;
use farwing_protocol::chat::{Attachment, AttachmentCreate, AttachmentUpload, Id, limits, perm};
use farwing_protocol::error::reason;
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

#[derive(Deserialize)]
pub struct TokenQuery {
    #[serde(default)]
    t: String,
    #[serde(default)]
    download: Option<String>,
}

fn unauthorized() -> ApiError {
    ApiError::new(ErrorCode::Unauthorized, "missing or expired media token")
}

fn media_user(app: &App, token: &str) -> ApiResult<String> {
    app.chat.media.verify(token, now_s()).ok_or_else(unauthorized)
}

/// A file name as stored: no path, no control characters, ≤ 255 characters.
fn file_name(name: &str) -> ApiResult<String> {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("").trim();
    if base.is_empty() || base == "." || base == ".." || base.chars().any(char::is_control) {
        return Err(ApiError::bad_request("a file name"));
    }
    Ok(base.chars().take(limits::MAX_FILE_NAME).collect())
}

fn mime(text: &str) -> String {
    let t = text.trim().to_ascii_lowercase();
    let valid = !t.is_empty()
        && t.len() <= 127
        && t.contains('/')
        && t.bytes().all(|b| b.is_ascii_alphanumeric() || b"/+-.;=_ ".contains(&b));
    if valid { t } else { "application/octet-stream".to_owned() }
}

/// `POST /chat/attachments {channel_id, name, mime, size}` (`attach`):
/// where to upload the bytes.
pub async fn create_attachment(
    State(app): State<App>,
    auth: AccountAuth,
    crate::http::JsonBody(req): crate::http::JsonBody<AttachmentCreate>,
) -> ApiResult<Json<AttachmentUpload>> {
    let name = file_name(&req.name)?;
    let cfg = app.cfg().chat.clone();
    let max = u64::from(cfg.max_attachment_mb) * 1024 * 1024;
    if req.size == 0 {
        return Err(ApiError::bad_request("an empty file"));
    }
    if req.size > max {
        return Err(ApiError::reason(ErrorCode::PayloadTooLarge, reason::CHAT_TOO_LARGE, "file too large")
            .with_detail("max", max));
    }
    app.limiter
        .take(&format!("chat-upload:{}", auth.user.id), 120, std::time::Duration::from_secs(3600))
        .map_err(ApiError::rate_limited)?;
    let token = crate::crypto::new_token("");
    let hash = token_hash(&token);
    let u = auth.user.id.clone();
    let channel = req.channel_id.0;
    let mime_type = mime(&req.mime);
    let size = req.size as i64;
    let group_quota = i64::from(cfg.group_quota_gb) << 30;
    let dm_quota = i64::from(cfg.dm_quota_gb) << 30;
    let id = app
        .db
        .call(move |c| {
            let Some(a) = access(c, &u, channel)?.filter(|a| a.channel.kind.has_messages()) else {
                return Ok(Err(not_found()));
            };
            if !a.can(perm::SEND | perm::ATTACH) {
                return Ok(Err(super::forbidden()));
            }
            let (used, quota) = match &a.ctx {
                Some(ctx) => (
                    store::group_usage(c, ctx.group.id)?,
                    ctx.group.quota_bytes.unwrap_or(group_quota),
                ),
                None => (store::dm_usage(c, &u)?, dm_quota),
            };
            if used + size > quota {
                return Ok(Err(ApiError::reason(ErrorCode::Forbidden, reason::CHAT_QUOTA, "attachment space is full")));
            }
            let row = AttachmentRow {
                id: next_id(),
                owner_id: u.clone(),
                channel_id: channel,
                group_id: a.group_id(),
                name,
                mime: mime_type,
                size,
                upload_hash: Some(hash),
                created_at: now_s(),
                ..Default::default()
            };
            store::insert_attachment(c, &row)?;
            Ok(Ok(row.id))
        })
        .await??;
    Ok(Json(AttachmentUpload {
        id: Id(id),
        upload: format!("/media/upload/{id}?t={token}"),
    }))
}

/// `PUT /media/upload/{id}?t=`: the bytes of an attachment, exactly as
/// many as announced.
pub async fn upload(
    State(app): State<App>,
    Path(aid): Path<String>,
    Query(q): Query<TokenQuery>,
    req: Request,
) -> ApiResult<Json<Attachment>> {
    let aid = id(&aid)?;
    let hash = token_hash(&q.t);
    let row = app
        .db
        .call(move |c| store::attachment(c, aid))
        .await?
        .filter(|a| !a.uploaded && a.upload_hash.as_deref() == Some(hash.as_str()))
        .ok_or_else(not_found)?;
    let data = app.cfg().data_dir.clone();
    let path = media::file(&data, row.group_id, row.id);
    if let Some(dir) = path.parent() {
        tokio::fs::create_dir_all(dir).await.map_err(|e| anyhow::anyhow!("attachment folder: {e}"))?;
    }
    let tmp = path.with_extension("part");
    let mut file = tokio::fs::File::create(&tmp)
        .await
        .map_err(|e| anyhow::anyhow!("attachment file: {e}"))?;
    let mut stream = req.into_body().into_data_stream();
    let mut hasher = Sha256::new();
    let mut total: i64 = 0;
    let failed = loop {
        match stream.next().await {
            Some(Ok(chunk)) => {
                total += chunk.len() as i64;
                if total > row.size {
                    break Some(ApiError::reason(ErrorCode::PayloadTooLarge, reason::CHAT_TOO_LARGE, "more bytes than announced")
                        .with_detail("max", row.size));
                }
                hasher.update(&chunk);
                if file.write_all(&chunk).await.is_err() {
                    break Some(ApiError::internal());
                }
            }
            Some(Err(_)) => break Some(ApiError::bad_request("upload interrupted")),
            None if total != row.size => break Some(ApiError::bad_request("fewer bytes than announced")),
            None => break None,
        }
    };
    let flushed = file.flush().await.is_ok();
    drop(file);
    if let Some(e) = failed.or_else(|| (!flushed).then(ApiError::internal)) {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(e);
    }
    tokio::fs::rename(&tmp, &path)
        .await
        .map_err(|e| anyhow::anyhow!("attachment file: {e}"))?;
    let sha = hex::encode(hasher.finalize());
    let (w, h, thumb) = if row.mime.starts_with("image/") {
        let p = path.clone();
        match tokio::task::spawn_blocking(move || media::thumbnail(&p)).await.ok().flatten() {
            Some((w, h, bytes)) => {
                let written = tokio::fs::write(media::thumb(&data, row.group_id, row.id), bytes).await.is_ok();
                (w, h, written)
            }
            None => (0, 0, false),
        }
    } else {
        (0, 0, false)
    };
    let s = sha.clone();
    app.db
        .call(move |c| store::finish_upload(c, aid, &s, w, h, thumb))
        .await?;
    let mut done = row.clone();
    done.width = w;
    done.height = h;
    done.thumb = thumb;
    done.uploaded = true;
    Ok(Json(done.wire()))
}

/// The attachment `id` if `user` may read it.
async fn readable(app: &App, user: &str, aid: i64) -> ApiResult<AttachmentRow> {
    let u = user.to_owned();
    app.db
        .call(move |c| {
            let Some(row) = store::attachment(c, aid)?.filter(|a| a.uploaded) else {
                return Ok(None);
            };
            let Some(a) = access(c, &u, row.channel_id)? else {
                return Ok(None);
            };
            let ok = match row.message_id {
                Some(m) => m >= a.floor,
                None => row.owner_id == u,
            };
            Ok(ok.then_some(row))
        })
        .await?
        .ok_or_else(not_found)
}

/// Answer with a file, honoring one `Range: bytes=a-b`.
async fn serve_file(path: PathBuf, headers: &HeaderMap, extra: Vec<(header::HeaderName, HeaderValue)>) -> ApiResult<Response> {
    let mut file = tokio::fs::File::open(&path).await.map_err(|_| not_found())?;
    let len = file.metadata().await.map_err(|_| not_found())?.len();
    let range = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("bytes="))
        .filter(|v| !v.contains(','))
        .and_then(|v| {
            let (a, b) = v.split_once('-')?;
            let (start, end) = match (a.trim(), b.trim()) {
                ("", suffix) => {
                    let n: u64 = suffix.parse().ok()?;
                    (len.saturating_sub(n), len.checked_sub(1)?)
                }
                (a, "") => (a.parse().ok()?, len.checked_sub(1)?),
                (a, b) => (a.parse().ok()?, b.parse::<u64>().ok()?.min(len.checked_sub(1)?)),
            };
            (start <= end && end < len).then_some((start, end))
        });
    let (status, start, count) = match range {
        Some((s, e)) => (StatusCode::PARTIAL_CONTENT, s, e - s + 1),
        None if headers.contains_key(header::RANGE) && len > 0 => {
            return Ok((
                StatusCode::RANGE_NOT_SATISFIABLE,
                [(header::CONTENT_RANGE, format!("bytes */{len}"))],
            )
                .into_response());
        }
        None => (StatusCode::OK, 0, len),
    };
    if start > 0 {
        file.seek(std::io::SeekFrom::Start(start)).await.map_err(|_| ApiError::internal())?;
    }
    let stream = futures_util::stream::unfold((file, count), |(mut file, left)| async move {
        if left == 0 {
            return None;
        }
        let mut buf = vec![0u8; left.min(64 * 1024) as usize];
        match file.read(&mut buf).await {
            Ok(0) => None,
            Ok(n) => {
                buf.truncate(n);
                Some((Ok::<_, std::io::Error>(bytes::Bytes::from(buf)), (file, left - n as u64)))
            }
            Err(e) => Some((Err(e), (file, 0))),
        }
    });
    let mut resp = Response::new(Body::from_stream(stream));
    *resp.status_mut() = status;
    let h = resp.headers_mut();
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(count));
    h.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    if status == StatusCode::PARTIAL_CONTENT
        && let Ok(v) = HeaderValue::from_str(&format!("bytes {start}-{}/{len}", start + count - 1))
    {
        h.insert(header::CONTENT_RANGE, v);
    }
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static("sandbox"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("private, max-age=31536000, immutable"));
    for (k, v) in extra {
        h.insert(k, v);
    }
    Ok(resp)
}

fn disposition(kind: &str, name: &str) -> HeaderValue {
    let ascii: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || ".-_ ".contains(c) { c } else { '_' })
        .collect();
    let encoded: String = name
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b".-_".contains(&b) {
                char::from(b).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect();
    HeaderValue::from_str(&format!("{kind}; filename=\"{ascii}\"; filename*=UTF-8''{encoded}"))
        .unwrap_or_else(|_| HeaderValue::from_static("attachment"))
}

/// `GET /media/attachments/{id}/{name}?t=[&download=1]`
pub async fn download(
    State(app): State<App>,
    Path((aid, _name)): Path<(String, String)>,
    Query(q): Query<TokenQuery>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let user = media_user(&app, &q.t)?;
    let row = readable(&app, &user, id(&aid)?).await?;
    // Only images the server decoded show inline, with their type.
    let image = row.width > 0 && q.download.is_none();
    let content_type = if image { row.mime.clone() } else { "application/octet-stream".to_owned() };
    let path = media::file(&app.cfg().data_dir, row.group_id, row.id);
    serve_file(
        path,
        &headers,
        vec![
            (
                header::CONTENT_TYPE,
                HeaderValue::from_str(&content_type).unwrap_or(HeaderValue::from_static("application/octet-stream")),
            ),
            (header::CONTENT_DISPOSITION, disposition(if image { "inline" } else { "attachment" }, &row.name)),
        ],
    )
    .await
}

/// `GET /media/thumbs/{id}?t=`
pub async fn thumb(
    State(app): State<App>,
    Path(aid): Path<String>,
    Query(q): Query<TokenQuery>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let user = media_user(&app, &q.t)?;
    let row = readable(&app, &user, id(&aid)?).await?;
    if !row.thumb {
        return Err(not_found());
    }
    serve_file(
        media::thumb(&app.cfg().data_dir, row.group_id, row.id),
        &headers,
        vec![(header::CONTENT_TYPE, HeaderValue::from_static("image/webp"))],
    )
    .await
}

/// `GET /media/avatars/{user}?v=&t=`
pub async fn avatar(
    State(app): State<App>,
    Path(user): Path<String>,
    Query(q): Query<TokenQuery>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    media_user(&app, &q.t)?;
    if !farwing_protocol::ids::is_uuid(&user) {
        return Err(not_found());
    }
    serve_file(
        media::avatar(&app.cfg().data_dir, &user),
        &headers,
        vec![(header::CONTENT_TYPE, HeaderValue::from_static("image/webp"))],
    )
    .await
}

/// `GET /media/icons/{group}?v=&t=`
pub async fn icon(
    State(app): State<App>,
    Path(group): Path<String>,
    Query(q): Query<TokenQuery>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    media_user(&app, &q.t)?;
    serve_file(
        media::icon(&app.cfg().data_dir, id(&group)?),
        &headers,
        vec![(header::CONTENT_TYPE, HeaderValue::from_static("image/webp"))],
    )
    .await
}

/// Store an avatar or icon from an uploaded image; its new version.
async fn store_square(req: Request, path: PathBuf) -> ApiResult<String> {
    let body = crate::http::read_body(req, limits::MAX_IMAGE_BYTES).await?;
    let square = tokio::task::spawn_blocking(move || media::square(&body))
        .await
        .ok()
        .flatten()
        .ok_or_else(|| ApiError::bad_request("an image (PNG, JPEG, GIF or WebP)"))?;
    if let Some(dir) = path.parent() {
        tokio::fs::create_dir_all(dir).await.map_err(|e| anyhow::anyhow!("image folder: {e}"))?;
    }
    let tmp = path.with_extension("tmp");
    tokio::fs::write(&tmp, square).await.map_err(|e| anyhow::anyhow!("image: {e}"))?;
    tokio::fs::rename(&tmp, &path).await.map_err(|e| anyhow::anyhow!("image: {e}"))?;
    Ok(media::version())
}

/// `PUT /me/avatar` (an image, ≤ 4 MiB)
pub async fn put_avatar(State(app): State<App>, auth: AccountAuth, req: Request) -> ApiResult<Json<serde_json::Value>> {
    let path = media::avatar(&app.cfg().data_dir, &auth.user.id);
    let version = store_square(req, path).await?;
    let (u, v) = (auth.user.id.clone(), version.clone());
    app.db.call(move |c| store::set_avatar(c, &u, &v)).await?;
    push::user_changed(&app, &auth.user.id).await;
    Ok(Json(json!({ "avatar": version })))
}

/// `DELETE /me/avatar`
pub async fn delete_avatar(State(app): State<App>, auth: AccountAuth) -> ApiResult<Json<Empty>> {
    let _ = tokio::fs::remove_file(media::avatar(&app.cfg().data_dir, &auth.user.id)).await;
    let u = auth.user.id.clone();
    app.db.call(move |c| store::set_avatar(c, &u, "")).await?;
    push::user_changed(&app, &auth.user.id).await;
    Ok(Json(Empty {}))
}

/// `PUT /chat/groups/{id}/icon` (`manage_group`; an image, ≤ 4 MiB)
pub async fn put_icon(
    State(app): State<App>,
    auth: AccountAuth,
    Path(gid): Path<String>,
    req: Request,
) -> ApiResult<Json<serde_json::Value>> {
    let group = id(&gid)?;
    let u = auth.user.id.clone();
    app.db
        .call(move |c| Ok(mine_with(c, &u, group, perm::MANAGE_GROUP)?.map(|_| ())))
        .await??;
    let version = store_square(req, media::icon(&app.cfg().data_dir, group)).await?;
    let (u, v) = (auth.user.id.clone(), version.clone());
    app.db
        .call(move |c| {
            if let Some(mut g) = store::group(c, group)? {
                g.icon = v;
                store::update_group(c, &g)?;
                audit(c, group, &u, "group_icon", "", json!({}))?;
            }
            Ok(())
        })
        .await?;
    push::group_state(&app, group, None).await;
    Ok(Json(json!({ "icon": version })))
}

/// `DELETE /chat/groups/{id}/icon` (`manage_group`)
pub async fn delete_icon(State(app): State<App>, auth: AccountAuth, Path(gid): Path<String>) -> ApiResult<Json<Empty>> {
    let group = id(&gid)?;
    let u = auth.user.id.clone();
    app.db
        .call(move |c| {
            let (ctx, _) = match mine_with(c, &u, group, perm::MANAGE_GROUP)? {
                Ok(v) => v,
                Err(e) => return Ok(Err(e)),
            };
            let mut g = ctx.group;
            g.icon = String::new();
            store::update_group(c, &g)?;
            audit(c, group, &u, "group_icon", "", json!({ "removed": true }))?;
            Ok(Ok(()))
        })
        .await??;
    let _ = tokio::fs::remove_file(media::icon(&app.cfg().data_dir, group)).await;
    push::group_state(&app, group, None).await;
    Ok(Json(Empty {}))
}

/// `GET /emoji/{file}`: the emoji images and their index, built into the
/// server (`crate::http::web_static::emoji`).
pub async fn emoji(Path(file): Path<String>, headers: HeaderMap) -> Response {
    crate::http::web_static::emoji(&file, &headers)
}
