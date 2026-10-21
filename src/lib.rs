//! Farwing Client: self-hosted remote desktop and streaming, Windows application composition.

#[cfg(not(windows))]
compile_error!("Farwing currently supports Windows only");

pub mod product;

pub const APP_NAME: &str = product::PRODUCT_NAME;
pub(crate) const VIEWER_TITLE_PREFIX: &str = "Farwing — ";
// `concat!` cannot read constants; keep the viewer title prefix in sync here.
const _: () = assert!(starts_with(VIEWER_TITLE_PREFIX, APP_NAME));
const fn starts_with(text: &str, prefix: &str) -> bool {
    let (text, prefix) = (text.as_bytes(), prefix.as_bytes());
    if text.len() < prefix.len() {
        return false;
    }
    let mut i = 0;
    while i < prefix.len() {
        if text[i] != prefix[i] {
            return false;
        }
        i += 1;
    }
    true
}

pub mod account;
pub mod application;
pub mod diagnostics;
pub mod features;
pub mod i18n;
pub mod media;
pub mod protocol;
pub mod session;
pub mod transport;

pub mod plugins;
mod ui;

mod platform;
