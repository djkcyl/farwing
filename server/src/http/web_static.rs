//! The web console's files, embedded at build time (`build.rs`), served at
//! `/` with a single-page-application fallback and strict security headers
//! (`docs/web-console.md` §3).

use super::App;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};

mod assets {
    include!(concat!(env!("OUT_DIR"), "/web_assets.rs"));
}

fn mime(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "json" => "application/json",
        "webmanifest" => "application/manifest+json",
        "woff2" => "font/woff2",
        "wasm" => "application/wasm",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

fn find(path: &str) -> Option<&'static [u8]> {
    assets::ASSETS
        .iter()
        .find(|(p, _)| *p == path)
        .map(|(_, bytes)| *bytes)
}

/// The precompressed copy (`scripts/compress.mjs`) the browser accepts:
/// `(bytes, Content-Encoding)`.
fn encoded(path: &str, headers: &HeaderMap) -> Option<(&'static [u8], &'static str)> {
    let accepted = headers
        .get(header::ACCEPT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let accepts = |name: &str| {
        accepted.split(',').any(|item| {
            let mut parts = item.split(';');
            parts.next().is_some_and(|n| n.trim().eq_ignore_ascii_case(name))
                && !parts.any(|q| q.trim().replace(' ', "") == "q=0")
        })
    };
    [("br", ".br"), ("gzip", ".gz")]
        .into_iter()
        .filter(|(name, _)| accepts(name))
        .find_map(|(name, suffix)| find(&format!("{path}{suffix}")).map(|bytes| (bytes, name)))
}

/// Content security policy: everything from this origin only; media and
/// screenshots use `blob:` URLs, the viewer connects back over WSS.
const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; \
    img-src 'self' data: blob:; media-src 'self' blob:; connect-src 'self' wss: ws:; \
    font-src 'self'; object-src 'none'; base-uri 'self'; form-action 'self'; frame-ancestors 'none'";

fn with_headers(mut response: Response, cache: &'static str) -> Response {
    let h = response.headers_mut();
    h.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(CSP));
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    h.insert(
        "permissions-policy",
        // The microphone (viewer, voice), the camera (voice channels) and the
        // clipboard; nothing for embedded content.
        HeaderValue::from_static(
            "camera=(self), geolocation=(), microphone=(self), clipboard-read=(self), clipboard-write=(self)",
        ),
    );
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    response
}

/// The emoji pack: `EMOJ`, a count, then `(name length u16, name, offset
/// u32, length u32)` entries, then the files (`emoji/tool`).
fn emoji_index() -> &'static std::collections::HashMap<&'static str, &'static [u8]> {
    static INDEX: std::sync::OnceLock<std::collections::HashMap<&'static str, &'static [u8]>> =
        std::sync::OnceLock::new();
    INDEX.get_or_init(|| {
        let pack = assets::EMOJI;
        let mut out = std::collections::HashMap::new();
        if pack.len() < 8 || &pack[..4] != b"EMOJ" {
            return out;
        }
        let u32_at = |i: usize| pack.get(i..i + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize);
        let Some(count) = u32_at(4) else { return out };
        let mut at = 8;
        for _ in 0..count {
            let Some(len) = pack.get(at..at + 2).map(|b| u16::from_le_bytes([b[0], b[1]]) as usize) else {
                break;
            };
            let Some(name) = pack.get(at + 2..at + 2 + len).and_then(|b| std::str::from_utf8(b).ok()) else {
                break;
            };
            at += 2 + len;
            let (Some(offset), Some(size)) = (u32_at(at), u32_at(at + 4)) else { break };
            at += 8;
            if let Some(bytes) = pack.get(offset..offset + size) {
                out.insert(name, bytes);
            }
        }
        out
    })
}

/// `GET /emoji/{file}`: an emoji image (`1f600.png`, code points joined by
/// `-`) or the index of names and keywords (`index.json`).
pub fn emoji(file: &str, _headers: &HeaderMap) -> Response {
    let Some(bytes) = emoji_index().get(file).copied() else {
        return crate::error::ApiError::not_found().into_response();
    };
    let mut response = ([(header::CONTENT_TYPE, mime(file))], Body::from(bytes)).into_response();
    response.headers_mut().insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("public, max-age=604800"));
    response
}

/// Shown to whoever builds the server without the web console (a developer).
const NOT_BUILT: &str = "<!doctype html><html lang=en><meta charset=utf-8><title>Farwing</title>\
<body style=\"font-family:sans-serif;background:#16181d;color:#e6e8ee;padding:48px\">\
<h1>Farwing Server</h1><p>The web console is not built. Run <code>npm ci &amp;&amp; npm run build</code> \
in server/web, then build the server again (or use the build-web script). The client API works \
without it.</p></body></html>";

/// Fallback for every path the API does not handle.
pub async fn serve(State(app): State<App>, method: Method, uri: Uri, headers: HeaderMap) -> Response {
    let path = uri.path();
    if path.starts_with("/api/") || !(method == Method::GET || method == Method::HEAD) {
        return crate::error::ApiError::not_found().into_response();
    }
    if !app.cfg().web.enabled {
        return (StatusCode::NOT_FOUND, "web console disabled").into_response();
    }
    // Hashed build output never changes; everything else is revalidated.
    if let Some(bytes) = find(path).filter(|_| path != "/index.html") {
        let cache = if path.starts_with("/assets/") {
            "public, max-age=31536000, immutable"
        } else {
            "no-cache"
        };
        let mut response = match encoded(path, &headers) {
            Some((compressed, encoding)) => (
                [
                    (header::CONTENT_TYPE, mime(path)),
                    (header::CONTENT_ENCODING, encoding),
                ],
                Body::from(compressed),
            )
                .into_response(),
            None => ([(header::CONTENT_TYPE, mime(path))], Body::from(bytes)).into_response(),
        };
        response
            .headers_mut()
            .insert(header::VARY, HeaderValue::from_static("accept-encoding"));
        return with_headers(response, cache);
    }
    // A client-side route: the application's page, with its base set to the
    // public URL's path (a reverse proxy may serve the server under it).
    let page = match find("/index.html") {
        Some(page) => with_base(page, &app.path_prefix()),
        None => NOT_BUILT.as_bytes().to_vec(),
    };
    let response = (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        Body::from(page),
    )
        .into_response();
    with_headers(response, "no-cache")
}

/// `<base href="/">` of the built page → `<base href="{prefix}/">`.
fn with_base(page: &[u8], prefix: &str) -> Vec<u8> {
    const BASE: &str = "<base href=\"/\"";
    let Ok(text) = std::str::from_utf8(page) else {
        return page.to_vec();
    };
    if prefix.is_empty() || !text.contains(BASE) {
        return page.to_vec();
    }
    let escaped: String = prefix
        .chars()
        .map(|c| match c {
            '"' => "%22".to_owned(),
            '<' => "%3C".to_owned(),
            '>' => "%3E".to_owned(),
            c => c.to_string(),
        })
        .collect();
    text.replacen(BASE, &format!("<base href=\"{escaped}/\""), 1)
        .into_bytes()
}

#[cfg(test)]
mod tests {
    use axum::http::{HeaderMap, HeaderValue, header};

    #[test]
    fn accept_encoding_is_parsed() {
        let mut h = HeaderMap::new();
        // No precompressed file of this name: nothing to serve.
        h.insert(header::ACCEPT_ENCODING, HeaderValue::from_static("br, gzip"));
        assert!(super::encoded("/missing.js", &h).is_none());
        let Some((path, _)) = super::assets::ASSETS
            .iter()
            .find(|(p, _)| p.ends_with(".js.br"))
            .map(|(p, b)| (p.trim_end_matches(".br"), b))
        else {
            return; // Web console not built.
        };
        assert_eq!(super::encoded(path, &h).map(|e| e.1), Some("br"));
        h.insert(header::ACCEPT_ENCODING, HeaderValue::from_static("gzip, deflate"));
        assert_eq!(super::encoded(path, &h).map(|e| e.1), Some("gzip"));
        h.insert(header::ACCEPT_ENCODING, HeaderValue::from_static("br;q=0, gzip;q=0"));
        assert!(super::encoded(path, &h).is_none());
        assert!(super::encoded(path, &HeaderMap::new()).is_none());
    }

    #[test]
    fn base_follows_the_public_path() {
        let page = b"<head><base href=\"/\" /></head>";
        assert_eq!(super::with_base(page, ""), page.to_vec());
        assert_eq!(
            String::from_utf8(super::with_base(page, "/farwing")).unwrap(),
            "<head><base href=\"/farwing/\" /></head>"
        );
    }
}
