//! Chat files (`docs/protocol/chat-design.md` §2.4): where attachments,
//! thumbnails, avatars and group icons live, the tokens that let browsers
//! load them, and the image work (sizes, thumbnails, square avatars).

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::path::{Path, PathBuf};

/// Lifetime of a media token.
pub const TOKEN_TTL_S: i64 = 24 * 3600;
/// Longest side of a thumbnail.
const THUMB_SIDE: u32 = 400;
/// Side of an avatar or group icon.
const SQUARE_SIDE: u32 = 256;
/// Pixels decoded at most (a decompression bomb is refused).
const MAX_PIXELS: u64 = 50_000_000;

/// The key of media tokens (kept in the database's `meta`).
pub struct MediaKey(pub [u8; 32]);

impl MediaKey {
    fn mac(&self, text: &str) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.0).expect("any key length");
        mac.update(text.as_bytes());
        URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
    }

    /// A token for `user` until `expires` (Unix seconds): `exp.user.mac`.
    pub fn token(&self, user: &str, expires: i64) -> String {
        let body = format!("{expires}.{user}");
        let mac = self.mac(&body);
        format!("{body}.{mac}")
    }

    /// The user of a valid token.
    pub fn verify(&self, token: &str, now: i64) -> Option<String> {
        let (body, mac) = token.rsplit_once('.')?;
        let (exp, user) = body.split_once('.')?;
        let exp: i64 = exp.parse().ok()?;
        if exp < now || !farwing_protocol::ids::is_uuid(user) {
            return None;
        }
        let expected = self.mac(body);
        use subtle::ConstantTimeEq;
        bool::from(expected.as_bytes().ct_eq(mac.as_bytes())).then(|| user.to_owned())
    }
}

/// The media key, made once per database.
pub fn load_key(c: &mut rusqlite::Connection) -> rusqlite::Result<[u8; 32]> {
    use rusqlite::OptionalExtension;
    let stored: Option<String> = c
        .query_row("SELECT value FROM meta WHERE key = 'chat_media_key'", [], |r| r.get(0))
        .optional()?;
    if let Some(key) = stored
        .and_then(|h| hex::decode(h).ok())
        .and_then(|b| <[u8; 32]>::try_from(b).ok())
    {
        return Ok(key);
    }
    let key: [u8; 32] = crate::crypto::random_array();
    c.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES ('chat_media_key', ?1)",
        [hex::encode(key)],
    )?;
    Ok(key)
}

/// Folder of a group's attachments (`dm` for direct messages).
pub fn folder(data: &Path, group: Option<i64>) -> PathBuf {
    data.join("attachments").join(match group {
        Some(g) => g.to_string(),
        None => "dm".to_owned(),
    })
}

pub fn file(data: &Path, group: Option<i64>, id: i64) -> PathBuf {
    folder(data, group).join(id.to_string())
}

pub fn thumb(data: &Path, group: Option<i64>, id: i64) -> PathBuf {
    folder(data, group).join(format!("{id}.thumb.webp"))
}

pub fn avatar(data: &Path, user: &str) -> PathBuf {
    data.join("avatars").join(format!("{user}.webp"))
}

pub fn icon(data: &Path, group: i64) -> PathBuf {
    data.join("group-icons").join(format!("{group}.webp"))
}

/// Remove an attachment's file and thumbnail.
pub fn remove(data: &Path, group: Option<i64>, id: i64) {
    let _ = std::fs::remove_file(file(data, group, id));
    let _ = std::fs::remove_file(thumb(data, group, id));
}

/// Decode an image of an accepted format (PNG, JPEG, GIF's first frame,
/// WebP) within the pixel limit.
fn decode(bytes: &[u8]) -> Option<image::DynamicImage> {
    let reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    if !matches!(
        reader.format(),
        Some(image::ImageFormat::Png | image::ImageFormat::Jpeg | image::ImageFormat::Gif | image::ImageFormat::WebP)
    ) {
        return None;
    }
    let (w, h) = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()?;
    if u64::from(w) * u64::from(h) > MAX_PIXELS || w == 0 || h == 0 {
        return None;
    }
    let mut reader = reader;
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(MAX_PIXELS * 4 + (64 << 20));
    reader.limits(limits);
    reader.decode().ok()
}

fn webp(img: &image::RgbaImage, quality: f32) -> Vec<u8> {
    webp::Encoder::from_rgba(img.as_raw(), img.width(), img.height())
        .encode(quality)
        .to_vec()
}

/// An image's size and its thumbnail (WebP, longest side ≤ 400); none for
/// what is not an image of an accepted format.
pub fn thumbnail(path: &Path) -> Option<(u32, u32, Vec<u8>)> {
    let bytes = std::fs::read(path).ok()?;
    let img = decode(&bytes)?;
    let (w, h) = (img.width(), img.height());
    let small = if w > THUMB_SIDE || h > THUMB_SIDE {
        img.thumbnail(THUMB_SIDE, THUMB_SIDE)
    } else {
        img
    };
    Some((w, h, webp(&small.to_rgba8(), 78.0)))
}

/// An avatar or icon: the middle square, 256 pixels, WebP.
pub fn square(bytes: &[u8]) -> Option<Vec<u8>> {
    let img = decode(bytes)?;
    let side = img.width().min(img.height());
    let x = (img.width() - side) / 2;
    let y = (img.height() - side) / 2;
    let cropped = img.crop_imm(x, y, side, side);
    let out = cropped.resize_exact(SQUARE_SIDE, SQUARE_SIDE, image::imageops::FilterType::Lanczos3);
    Some(webp(&out.to_rgba8(), 85.0))
}

/// A short random version tag for an avatar or icon.
pub fn version() -> String {
    URL_SAFE_NO_PAD.encode(crate::crypto::random_array::<6>())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_verify_until_they_expire() {
        let key = MediaKey([7; 32]);
        let user = "0f8fad5b-d9cb-469f-a165-70867728950e";
        let t = key.token(user, 1_000);
        assert_eq!(key.verify(&t, 999).as_deref(), Some(user));
        assert_eq!(key.verify(&t, 1_001), None);
        assert_eq!(MediaKey([8; 32]).verify(&t, 999), None);
        let forged = t.replace(user, "1f8fad5b-d9cb-469f-a165-70867728950e");
        assert_eq!(key.verify(&forged, 999), None);
    }

    #[test]
    fn images_get_thumbnails_and_squares() {
        let img = image::RgbaImage::from_pixel(1200, 600, image::Rgba([10, 200, 30, 255]));
        let mut png = Vec::new();
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a");
        std::fs::write(&path, &png).unwrap();
        let (w, h, thumb) = thumbnail(&path).unwrap();
        assert_eq!((w, h), (1200, 600));
        let t = image::load_from_memory(&thumb).unwrap();
        assert_eq!((t.width(), t.height()), (400, 200));
        let sq = image::load_from_memory(&square(&png).unwrap()).unwrap();
        assert_eq!((sq.width(), sq.height()), (256, 256));
        std::fs::write(&path, b"not an image").unwrap();
        assert!(thumbnail(&path).is_none());
    }
}
