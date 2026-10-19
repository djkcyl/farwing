//! Peer platform of the host input policy (from the connect options' client type).
use serde::{Deserialize, Serialize};

/// Client type of a Windows controller.
pub(crate) const WINDOWS_CLIENT_TYPE: i32 = 3;

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub(crate) enum PeerPlatform {
    Windows,
    Mac,
    Android,
    Ios,
    Unknown,
}
impl PeerPlatform {
    /// Connect client type of a platform (host input policy).
    pub fn client_type(platform: farwing_protocol::Platform) -> i32 {
        match platform {
            farwing_protocol::Platform::Ios => 1,
            farwing_protocol::Platform::Android => 2,
            farwing_protocol::Platform::Macos => 4,
            _ => WINDOWS_CLIENT_TYPE,
        }
    }
    pub fn from_connect_client_type(value: i32) -> Self {
        match value {
            1 => Self::Ios,
            2 => Self::Android,
            3 => Self::Windows,
            4 => Self::Mac,
            _ => Self::Unknown,
        }
    }
    pub fn account_code(self) -> Option<i32> {
        match self {
            Self::Windows => Some(1),
            Self::Android => Some(2),
            Self::Ios => Some(3),
            Self::Mac => Some(4),
            Self::Unknown => None,
        }
    }
}
