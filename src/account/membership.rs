//! Account-scoped revocations from the authenticated presence channel.
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum MemberTier {
    Free,
    Vip,
    GlobalVip,
}

impl MemberTier {
    pub fn label(self) -> &'static str {
        match self {
            Self::Free => "免费版",
            Self::Vip => "VIP",
            Self::GlobalVip => "全球 VIP",
        }
    }
}

#[derive(Clone, Copy, Serialize, Deserialize)]
pub(crate) struct Profile {
    pub tier: Option<MemberTier>,
    observed_at: i64,
}

/// Both the resident owner and its GUI read the same monotonic counter
/// (Windows QPC, `CLOCK_MONOTONIC` elsewhere; both are system-wide for one
/// boot). It only orders local room opens and push delivery, never
/// subscription dates, so the unit does not matter.
#[cfg(windows)]
pub(super) fn local_counter() -> i64 {
    let mut value = 0;
    unsafe { windows::Win32::System::Performance::QueryPerformanceCounter(&mut value) }
        .expect("Windows performance counter");
    value
}

#[cfg(not(windows))]
pub(super) fn local_counter() -> i64 {
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: writes into a local timespec; CLOCK_MONOTONIC always exists.
    let result = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) };
    assert_eq!(result, 0, "CLOCK_MONOTONIC");
    now.tv_sec * 1_000_000_000 + now.tv_nsec
}

#[derive(Clone, Copy, Deserialize, Serialize)]
pub(crate) struct Expiration {
    expires_at: i64,
    received_at: i64,
}

#[derive(Default)]
pub(crate) struct Membership {
    pub(crate) connecting: tokio::sync::Mutex<()>,
    pub(crate) rights: tokio::sync::Mutex<Option<(u64, crate::account::api::Entitlements)>>,
    pub(crate) rights_updates:
        watch::Sender<Option<std::sync::Arc<crate::account::api::Entitlements>>>,
    revision: std::sync::atomic::AtomicU64,
    minimum_version: std::sync::atomic::AtomicU64,
    last: watch::Sender<Option<Expiration>>,
    profile: watch::Sender<Option<Profile>>,
    pub refresh_profile: tokio::sync::Notify,
}

impl Membership {
    pub fn invalidate_rights(&self) {
        self.revision
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        self.refresh_profile.notify_one();
    }
    pub fn rights_revision(&self) -> u64 {
        self.revision.load(std::sync::atomic::Ordering::Acquire)
    }
    pub fn minimum_version(&self) -> u64 {
        self.minimum_version
            .load(std::sync::atomic::Ordering::Acquire)
    }
    pub fn profile(&self) -> watch::Receiver<Option<Profile>> {
        self.profile.subscribe()
    }

    pub fn read_profile(&self, info: &serde_json::Value, account: &str, observed_at: i64) {
        if info.get("user_id").and_then(|v| v.as_str()) != Some(account) {
            return;
        }
        let tier = match info["member_type"].as_i64() {
            Some(0) => Some(MemberTier::Free),
            Some(1) => Some(MemberTier::Vip),
            Some(2) => Some(MemberTier::GlobalVip),
            _ => None,
        };
        self.observe_profile(Profile { tier, observed_at });
    }

    pub fn observe_profile(&self, profile: Profile) {
        self.profile.send_if_modified(|last| {
            if last.is_some_and(|old| old.observed_at >= profile.observed_at) {
                return false;
            }
            *last = Some(profile);
            true
        });
    }

    pub fn account_push(&self, push: &serde_json::Value) {
        if matches!(
            push["type"].as_str(),
            Some("member_info_changed" | "entitlement_updated")
        ) && push["data"].is_object()
        {
            if push["type"] == "entitlement_updated" {
                match push["data"]["update_type"].as_str() {
                    Some("normal") => {
                        let Some(version) = push["data"]["version"].as_u64() else {
                            return;
                        };
                        let current = self.rights_updates.borrow().as_ref().map(|t| t.version);
                        if current.is_some_and(|seen| version <= seen)
                            || (self.minimum_version() > 0 && version <= self.minimum_version())
                        {
                            return;
                        }
                        self.minimum_version
                            .fetch_max(version, std::sync::atomic::Ordering::AcqRel);
                    }
                    Some("force") => self
                        .minimum_version
                        .store(0, std::sync::atomic::Ordering::Release),
                    _ => return,
                }
            }
            self.invalidate_rights();
            self.observe_profile(Profile {
                tier: None,
                observed_at: local_counter(),
            });
            self.refresh_profile.notify_one();
            return;
        }
        if push["type"] != "member_expiration_kickout" {
            return;
        }
        let data = &push["data"];
        let Some(expires_at) = data["expire_at"].as_i64().filter(|t| *t > 0) else {
            return;
        };
        if data["before_member_type"].as_i64() == Some(2)
            && matches!(data["after_member_type"].as_i64(), Some(0..=2))
        {
            self.observe(Expiration {
                expires_at,
                received_at: local_counter(),
            });
            self.invalidate_rights();
            self.observe_profile(Profile {
                tier: None,
                observed_at: local_counter(),
            });
            self.refresh_profile.notify_one();
        }
    }

    // Read by the Windows resident service to hand the GUI its latest
    // expiration; Linux has no resident service.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn snapshot(&self) -> Option<Expiration> {
        *self.last.borrow()
    }

    pub fn observe(&self, event: Expiration) {
        self.last.send_if_modified(|last| {
            if last.is_some_and(|previous| previous.expires_at >= event.expires_at) {
                return false;
            }
            *last = Some(event);
            true
        });
    }

    pub fn cross_border_room(&self) -> RoomExpiration {
        RoomExpiration {
            opened_at: local_counter(),
            events: self.last.subscribe(),
        }
    }
}

pub(crate) struct RoomExpiration {
    opened_at: i64,
    events: watch::Receiver<Option<Expiration>>,
}

impl RoomExpiration {
    pub async fn revoked(room: &mut Option<Self>) -> anyhow::Error {
        if let Some(room) = room
            && room
                .events
                .wait_for(|last| last.is_some_and(|event| event.received_at >= room.opened_at))
                .await
                .is_ok()
        {
            return anyhow::anyhow!("全球 VIP 已到期，跨境远控连接已结束");
        }
        // Local rooms and account shutdown use the ordinary session lifecycle.
        std::future::pending().await
    }

    pub async fn during<T>(
        room: &mut Option<Self>,
        work: impl Future<Output = anyhow::Result<T>>,
    ) -> anyhow::Result<T> {
        tokio::select! {
            biased;
            error = Self::revoked(room) => Err(error),
            result = work => result,
        }
    }
}
