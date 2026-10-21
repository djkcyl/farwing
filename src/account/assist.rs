//! Connecting through the server to a device that is neither this
//! account's nor a friend's (`docs/access-model.md` §2.4), and the
//! server-synchronized saved devices. Such a host is addressed by its short
//! number (9 digits) or its device ID; it admits this device if it trusts
//! it, or as a visitor with its access code or its user's confirmation.
use anyhow::{Result, bail};
use std::fmt;

/// A short number: 9 digits, spaces and dashes ignored.
pub fn normalize_connect_id(value: &str) -> Result<String> {
    let id: String = value
        .chars()
        .filter(|c| !c.is_ascii_whitespace() && *c != '-')
        .collect();
    if id.len() != 9 || !id.bytes().all(|c| c.is_ascii_digit()) {
        bail!("{}", crate::i18n::t().session.short_number_required);
    }
    Ok(id)
}

/// A host reached through the server: its short number or its device ID.
pub fn normalize_target(value: &str) -> Result<String> {
    if let Ok(id) = normalize_connect_id(value) {
        return Ok(id);
    }
    crate::account::api::parse_device_id(value)
        .ok_or_else(|| anyhow::anyhow!("{}", crate::i18n::t().session.assist_target_required))
}

/// Whether a normalised target is a short number (else a device ID).
pub fn is_short_number(target: &str) -> bool {
    target.len() == 9 && target.bytes().all(|c| c.is_ascii_digit())
}

pub(crate) fn validate_connect_code(code: &str) -> Result<()> {
    if code.len() > 256 || code.chars().any(char::is_control) {
        bail!("{}", crate::i18n::t().session.code_format_invalid);
    }
    Ok(())
}

#[derive(Clone)]
pub struct AssistRequest {
    /// The short number or the device ID of the host.
    pub target: String,
    pub(crate) connect_code: String,
    /// A request the preflight already opened without a code (the host asks
    /// for confirmation only); the viewer continues it instead of joining.
    pub(crate) pending: Option<farwing_protocol::api::AssistPending>,
    pub(crate) expected_publisher_id: Option<String>,
}
impl AssistRequest {
    pub fn new(target: &str, connect_code: String) -> Result<Self> {
        let request = Self {
            target: normalize_target(target)?,
            connect_code,
            pending: None,
            expected_publisher_id: None,
        };
        request.validate()?;
        Ok(request)
    }
    pub fn validate(&self) -> Result<()> {
        normalize_target(&self.target)?;
        if let Some(id) = &self.expected_publisher_id {
            crate::account::api::validate_device_id(id)?;
        }
        validate_connect_code(&self.connect_code)
    }
    pub(crate) fn has_code(&self) -> bool {
        !self.connect_code.is_empty()
    }
}
impl fmt::Debug for AssistRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AssistRequest")
            .field("credentials", &"REDACTED")
            .finish()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SavedKind {
    Recent,
    Favorites,
}
impl SavedKind {
    pub(crate) fn path(self) -> &'static str {
        match self {
            Self::Recent => "recent",
            Self::Favorites => "favorites",
        }
    }
}

#[derive(Clone)]
pub(crate) struct SavedDevice {
    pub publisher_device_id: String,
    pub connect_id: String,
    pub remark: String,
    pub is_favorite: bool,
    pub last_connected_at: Option<i64>,
    pub favorited_at: Option<i64>,
    /// Locally saved code for this target; never sent to the server.
    pub saved_code: String,
    /// The device's key (its device ID is derived from it).
    pub public_key: String,
}

impl From<farwing_protocol::api::SavedDevice> for SavedDevice {
    fn from(value: farwing_protocol::api::SavedDevice) -> Self {
        Self {
            publisher_device_id: value.publisher_device_id,
            connect_id: value.connect_id,
            remark: value.remark,
            is_favorite: value.is_favorite,
            last_connected_at: value.last_connected_at,
            favorited_at: value.favorited_at,
            saved_code: String::new(),
            public_key: value.public_key,
        }
    }
}
impl SavedDevice {
    pub(crate) fn title(&self) -> &str {
        if self.remark.trim().is_empty() {
            &self.connect_id
        } else {
            &self.remark
        }
    }
    pub(crate) fn validate(&self) -> Result<()> {
        normalize_connect_id(&self.connect_id)?;
        crate::account::api::validate_device_id(&self.publisher_device_id)
    }
}
#[derive(Clone, Default)]
pub(crate) struct SavedLists {
    pub recent: Vec<SavedDevice>,
    pub favorites: Vec<SavedDevice>,
    pub code_error: Option<String>,
}
