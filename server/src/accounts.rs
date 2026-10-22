//! Account rules shared by setup, the administration API and registration.

use crate::crypto;
use crate::db;
use anyhow::{Result, bail};
use rusqlite::Connection;

pub const MIN_PASSWORD_LEN: usize = 8;
pub const MAX_PASSWORD_LEN: usize = 1024;
pub const MAX_USERNAME_LEN: usize = 64;

pub fn validate_username(name: &str) -> Result<()> {
    if name.is_empty() || name.chars().count() > MAX_USERNAME_LEN {
        bail!("username must be 1-{MAX_USERNAME_LEN} characters");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '@'))
    {
        bail!("username may only contain letters, digits and . _ - @");
    }
    Ok(())
}

pub fn check_password(pw: &str) -> Result<()> {
    if pw.chars().count() < MIN_PASSWORD_LEN {
        bail!("password must be at least {MIN_PASSWORD_LEN} characters");
    }
    if pw.chars().count() > MAX_PASSWORD_LEN {
        bail!("password is too long");
    }
    Ok(())
}

/// Create an account; its ID.
pub fn create_user(
    c: &Connection,
    username: &str,
    nickname: Option<&str>,
    password: &str,
) -> Result<String> {
    validate_username(username)?;
    check_password(password)?;
    if db::get_user_by_name(c, username)?.is_some() {
        bail!("user {username} already exists");
    }
    let hash = crypto::hash_password(password)?;
    let id = crypto::new_uuid();
    db::create_user(
        c,
        &id,
        username,
        &db::clip(nickname.unwrap_or(""), 64),
        &hash,
    )?;
    Ok(id)
}
