//! Commands and events crossing the UI/worker boundary.
use super::assist::{AssistOperation, AssistResult};
use super::login_form::LoginMethod;
use super::{StartupStage, catalog, power};
use crate::account::api::{DeviceInfo, DeviceList};
use crate::account::login::{self, LoginProgress};
use crate::media::ConnectionMediaOptions;
use crate::session::presence::PresenceState;

pub(super) enum GuiCommand {
    /// Save the server chosen on the login page, before any account.
    UseServer {
        server: crate::account::server::ServerConfig,
    },
    HostAssist {
        generation: u64,
        action: crate::features::host::assist::Action,
    },
    RefreshHostAudioDevices {
        generation: u64,
    },
    SaveHostSettings {
        generation: u64,
    },
    View {
        generation: u64,
        key: super::viewing::Key,
        alias: String,
        device_id: Option<String>,
        assist: Option<crate::account::assist::AssistRequest>,
        /// A host reached directly, without the server (§12).
        direct: Option<crate::session::controller::DirectRequest>,
        options: ConnectionMediaOptions,
        background: Option<crate::application::wallpaper::Source>,
    },
    Ports {
        generation: u64,
        device: DeviceInfo,
        options: ConnectionMediaOptions,
    },
    Files {
        generation: u64,
        device: DeviceInfo,
        options: ConnectionMediaOptions,
    },
    Diagnostics {
        generation: u64,
        device: DeviceInfo,
        options: ConnectionMediaOptions,
    },
    Refresh,
    Login {
        generation: u64,
        attempt: u64,
        server: crate::account::server::ServerConfig,
        credentials: login::Credentials,
    },
    CancelAttempt(u64),
    CancelLogin(u64),
    Logout,
    PrepareExit,
    /// Try the server again now.
    Reconnect,
    Shutdown,
    Mutate {
        generation: u64,
        change: DeviceMutation,
    },
    Detail(String),
    RefreshAssist,
    CancelAssistCheck,
    Friends {
        generation: u64,
        operation: super::friends::FriendOperation,
    },
    Records {
        generation: u64,
        operation: super::records::RecordOperation,
    },
    Notices {
        generation: u64,
        operation: super::notices::NoticeOperation,
    },
    AssistOperation {
        generation: u64,
        sequence: u64,
        operation: AssistOperation,
    },
}

pub(super) enum DeviceMutation {
    Rename {
        id: String,
        alias: String,
    },
    Remove {
        id: String,
    },
    Power {
        device: DeviceInfo,
        action: crate::account::power::PowerAction,
    },
}

pub(super) enum MutationOutcome {
    Changed {
        message: String,
        change: crate::account::device_change::DeviceChange,
    },
    Power(Box<power::AcceptedPower>),
}

pub(super) enum GuiEvent {
    ServerSaved(std::result::Result<(), String>),
    HostAssistFailed {
        generation: u64,
        error: String,
    },
    ExitReady(std::result::Result<(), String>),
    Host(u64, crate::features::host::Handle),
    Viewer(
        u64,
        super::viewing::Key,
        String,
        Option<String>,
        std::result::Result<crate::session::controller::windows::ViewerHandle, String>,
    ),
    PowerDispatched(u64, String, crate::account::power::PowerAction),
    Startup(u64, StartupStage),
    Presence(PresenceState),
    AssistLists(
        u64,
        std::result::Result<crate::account::assist::SavedLists, String>,
    ),
    AssistOperation(u64, u64, std::result::Result<AssistResult, String>),
    Friends(u64, std::result::Result<super::friends::FriendResult, String>),
    Records(u64, std::result::Result<super::records::RecordResult, String>),
    Notices(u64, std::result::Result<super::notices::NoticeResult, String>),
    Devices(u64, DeviceList),
    /// The last device list, while the server cannot be reached at startup.
    /// The saved device list (or this device alone) and the saved
    /// account name, while the server has not answered yet.
    CachedDevices(u64, DeviceList, String),
    /// Whether the server was out of reach at the last attempt.
    Reachability(u64, bool),
    /// The server's name, for the status bar.
    ServerName(u64, String),
    Catalog(u64, std::result::Result<catalog::Catalog, String>, String),
    MutationFinished(u64, std::result::Result<MutationOutcome, String>),
    Detail(
        u64,
        String,
        std::result::Result<crate::account::api::DeviceDetail, String>,
    ),
    Working(String),
    Warning(String),
    Error(String),
    SessionUnavailable(String),
    SignedOut,
    AccountEnded(String),
    LoginProgress(LoginMethod, u64, u64, LoginProgress),
    LoginFinished(LoginMethod, u64, u64, anyhow::Result<()>),
    LoggedOut(crate::account::client::LogoutOutcome),
}

impl DeviceMutation {
    pub(super) fn close_target(&self) -> Option<&str> {
        match self {
            Self::Remove { id } => Some(id),
            Self::Power { device, action }
                if *action != crate::account::power::PowerAction::Wake =>
            {
                Some(&device.device_id)
            }
            _ => None,
        }
    }
}
