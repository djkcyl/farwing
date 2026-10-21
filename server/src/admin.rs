//! Admin CLI commands. They operate directly on the SQLite database (WAL,
//! busy timeout) and record events the running server applies to live
//! connections.

use crate::config::Config;
use crate::crypto;
use crate::db::{self, open_connection};
use crate::util::format_ts;
use anyhow::{Context, Result, bail};
use rusqlite::Connection;
use std::io::BufRead;

pub const MIN_PASSWORD_LEN: usize = 8;
pub const MAX_PASSWORD_LEN: usize = 1024;
pub const MAX_USERNAME_LEN: usize = 64;

fn open(cfg: &Config) -> Result<Connection> {
    let path = cfg.db_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    open_connection(&path)
}

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

/// Read a password from stdin (one line) or prompt twice on the terminal.
pub fn read_password(from_stdin: bool) -> Result<String> {
    let pw = if from_stdin {
        let mut line = String::new();
        std::io::stdin()
            .lock()
            .read_line(&mut line)
            .context("reading password from stdin")?;
        line.trim_end_matches(['\r', '\n']).to_owned()
    } else {
        let a = rpassword::prompt_password("Password: ").context("reading password")?;
        let b = rpassword::prompt_password("Repeat password: ").context("reading password")?;
        if a != b {
            bail!("passwords do not match");
        }
        a
    };
    check_password(&pw)?;
    Ok(pw)
}

fn user_by_name(c: &Connection, username: &str) -> Result<db::UserRow> {
    db::get_user_by_name(c, username)?.with_context(|| format!("no such user: {username}"))
}

pub fn user_create(
    cfg: &Config,
    username: &str,
    nickname: Option<&str>,
    password: &str,
) -> Result<String> {
    validate_username(username)?;
    check_password(password)?;
    let c = open(cfg)?;
    if db::get_user_by_name(&c, username)?.is_some() {
        bail!("user {username} already exists");
    }
    let hash = crypto::hash_password(password)?;
    let id = crypto::new_uuid();
    db::create_user(
        &c,
        &id,
        username,
        &db::clip(nickname.unwrap_or(""), 64),
        &hash,
    )?;
    Ok(id)
}

pub fn user_list(cfg: &Config) -> Result<()> {
    let c = open(cfg)?;
    let users = crate::webdb::list_accounts(&c)?;
    println!(
        "{:<24} {:<20} {:<36} {:<8} {:<6} {:>7}  CREATED",
        "USERNAME", "NICKNAME", "USER ID", "STATUS", "ROLE", "DEVICES"
    );
    for u in users {
        println!(
            "{:<24} {:<20} {:<36} {:<8} {:<6} {:>7}  {}",
            u.username,
            u.nickname,
            u.id,
            if u.disabled { "disabled" } else { "active" },
            u.role,
            u.devices,
            format_ts(u.created_at)
        );
    }
    Ok(())
}

/// Make an account an administrator, or an ordinary account. The last
/// administrator stays one.
pub fn user_set_role(cfg: &Config, username: &str, admin: bool) -> Result<()> {
    let c = open(cfg)?;
    let u = user_by_name(&c, username)?;
    let current = crate::webdb::role(&c, &u.id)?;
    if !admin && current == crate::webdb::ROLE_ADMIN && crate::webdb::admin_count(&c)? <= 1 {
        bail!("{username} is the last administrator");
    }
    crate::webdb::set_role(
        &c,
        &u.id,
        if admin {
            crate::webdb::ROLE_ADMIN
        } else {
            crate::webdb::ROLE_USER
        },
    )?;
    Ok(())
}

/// Turn off two-step verification (the authenticator was lost).
pub fn user_reset_totp(cfg: &Config, username: &str) -> Result<()> {
    let c = open(cfg)?;
    let u = user_by_name(&c, username)?;
    crate::webdb::clear_totp(&c, &u.id)?;
    println!("two-step verification of {username} turned off");
    Ok(())
}

pub fn user_passwd(cfg: &Config, username: &str, password: &str) -> Result<()> {
    check_password(password)?;
    let c = open(cfg)?;
    let u = user_by_name(&c, username)?;
    let hash = crypto::hash_password(password)?;
    db::set_user_password(&c, &u.id, &hash)?;
    let n = db::revoke_tokens_for_user(&c, &u.id, "password_changed")?;
    db::insert_event(&c, db::EVENT_REVOKE_USER, Some(&u.id), None)?;
    println!("password changed; {n} login(s) revoked");
    Ok(())
}

pub fn user_set_disabled(cfg: &Config, username: &str, disabled: bool) -> Result<()> {
    let c = open(cfg)?;
    let u = user_by_name(&c, username)?;
    db::set_user_disabled(&c, &u.id, disabled)?;
    if disabled {
        let n = db::revoke_tokens_for_user(&c, &u.id, "user_disabled")?;
        db::insert_event(&c, db::EVENT_REVOKE_USER, Some(&u.id), None)?;
        println!("user {username} disabled; {n} login(s) revoked");
    } else {
        println!("user {username} enabled");
    }
    Ok(())
}

pub fn user_delete(cfg: &Config, username: &str) -> Result<()> {
    let mut c = open(cfg)?;
    let u = user_by_name(&c, username)?;
    let devices = db::delete_user(&mut c, &u.id)?;
    db::insert_event(&c, db::EVENT_REVOKE_USER, Some(&u.id), None)?;
    println!(
        "user {username} deleted; {} device(s) unbound",
        devices.len()
    );
    Ok(())
}

pub fn device_list(cfg: &Config, username: Option<&str>) -> Result<()> {
    let c = open(cfg)?;
    let devices = match username {
        Some(name) => {
            let u = user_by_name(&c, name)?;
            db::list_user_devices(&c, &u.id)?
        }
        None => db::list_all_devices(&c)?,
    };
    let users: std::collections::HashMap<String, String> = db::list_users(&c)?
        .into_iter()
        .map(|u| (u.id, u.username))
        .collect();
    println!(
        "{:<16} {:<24} {:<16} {:<8} {:<12} {:<9} {:<18} {:<23} REGISTERED",
        "DEVICE ID", "ALIAS", "ACCOUNT", "PLATFORM", "VERSION", "CONNECT", "POLICY", "LAST SEEN"
    );
    for d in devices {
        let owner = d
            .user_id
            .as_ref()
            .map(|u| users.get(u).cloned().unwrap_or_else(|| u.clone()))
            .unwrap_or_else(|| "-".to_owned());
        println!(
            "{:<16} {:<24} {:<16} {:<8} {:<12} {:<9} {:<18} {:<23} {}",
            d.id,
            d.alias.chars().take(24).collect::<String>(),
            owner,
            d.platform.as_str(),
            d.version,
            d.connect_id.clone().unwrap_or_else(|| "-".to_owned()),
            policy_summary(&d),
            format_ts(d.last_seen_at),
            format_ts(d.created_at)
        );
    }
    Ok(())
}

/// Compact form of what the owner allows, e.g. `control,files max=8`.
fn policy_summary(d: &db::DeviceRow) -> String {
    let p = &d.policy;
    let flags: Vec<&str> = [
        (p.control, "control"),
        (p.files, "files"),
        (p.ports, "ports"),
        (p.power, "power"),
        (p.wol, "wol"),
        (p.assist, "assist"),
    ]
    .into_iter()
    .filter_map(|(on, name)| on.then_some(name))
    .collect();
    let flags = if flags.is_empty() {
        "-".to_owned()
    } else {
        flags.join(",")
    };
    format!("{flags} max={}", d.max_participants())
}

/// Friends of an account with the grants the account gives each of them.
pub fn friend_list(cfg: &Config, username: &str) -> Result<()> {
    let c = open(cfg)?;
    let u = user_by_name(&c, username)?;
    let friends = db::list_friends(&c, &u.id)?;
    let grants = db::grants_by_owner(&c, &u.id)?;
    let devices: std::collections::HashMap<String, String> = db::list_user_devices(&c, &u.id)?
        .into_iter()
        .map(|d| (d.id, d.alias))
        .collect();
    println!(
        "{:<24} {:<20} {:<36} {:<23} GRANTED BY {username}",
        "USERNAME", "NICKNAME", "USER ID", "SINCE"
    );
    for (f, since) in friends {
        let g = grants.get(&f.id).cloned().unwrap_or_default();
        let mut parts = vec![format!(
            "default: {}",
            if g.default.is_empty() {
                "-".to_owned()
            } else {
                g.default.join(",")
            }
        )];
        for (device, list) in &g.devices {
            let name = devices.get(device).map(String::as_str).unwrap_or(device);
            parts.push(format!(
                "{name}: {}",
                if list.is_empty() {
                    "-".to_owned()
                } else {
                    list.join(",")
                }
            ));
        }
        println!(
            "{:<24} {:<20} {:<36} {:<23} {}",
            f.username,
            f.nickname,
            f.id,
            format_ts(since),
            parts.join("; ")
        );
    }
    Ok(())
}

pub fn device_remove(cfg: &Config, device_id: &str) -> Result<()> {
    let c = open(cfg)?;
    let d =
        db::get_device(&c, device_id)?.with_context(|| format!("no such device: {device_id}"))?;
    let n = db::revoke_tokens_for_device(&c, &d.id, "removed_by_admin")?;
    db::set_device_user(&c, &d.id, None)?;
    db::insert_event(
        &c,
        db::EVENT_REVOKE_DEVICE,
        d.user_id.as_deref(),
        Some(&d.id),
    )?;
    println!("device {device_id} unbound; {n} login(s) revoked");
    Ok(())
}

pub fn session_list(cfg: &Config) -> Result<()> {
    let c = open(cfg)?;
    let sessions = db::list_open_sessions(&c)?;
    let alias = |id: &str| -> String {
        db::get_device(&c, id)
            .ok()
            .flatten()
            .map(|d| d.alias)
            .unwrap_or_default()
    };
    let users: std::collections::HashMap<String, String> = db::list_users(&c)?
        .into_iter()
        .map(|u| (u.id, u.username))
        .collect();
    println!(
        "{:<36} {:<8} {:<8} {:<9} {:<34} {:<34} {:<16} CREATED",
        "SESSION ID", "RELATION", "PURPOSE", "STATE", "HOST", "CONTROLLER", "ACCOUNT"
    );
    for s in sessions {
        println!(
            "{:<36} {:<8} {:<8} {:<9} {:<34} {:<34} {:<16} {}",
            s.id,
            s.relation,
            s.purpose,
            s.state,
            format!("{} ({})", s.host_device_id, alias(&s.host_device_id)),
            format!(
                "{} ({})",
                s.controller_device_id,
                alias(&s.controller_device_id)
            ),
            users
                .get(&s.controller_user_id)
                .cloned()
                .unwrap_or_else(|| s.controller_user_id.clone()),
            format_ts(s.created_at)
        );
    }
    Ok(())
}
