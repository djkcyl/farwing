//! Per-profile records of direct connections and trusted devices
//! (`docs/access-model.md` §2.2), in the data directory of the process that
//! uses them:
//! - `trusted_devices.json`: devices that may connect to this one without
//!   an access code, and what they may do (host side; read by the hosting
//!   process);
//! - `known_hosts.json`: devices this one connected to: last address, short
//!   number, and whether they trust this device (controller side);
//! - `own_devices.json`: the cached device list of the logged-in account;
//! - `friend_devices.json`: the cached friend devices that may control this
//!   one, and their grants (host side).
//!
//! Every record is keyed by the device ID, which is derived from the device
//! key; a record whose ID is not its key's is dropped.
//!
//! With the background service installed the host runs in its resident
//! process, so the host-side records live in that process's own (SYSTEM)
//! profile and the user's GUI and CLI reach them through the resident pipe
//! (`super::managed`); the controller-side records stay with the user.
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

static LOCK: Mutex<()> = Mutex::new(());

fn dir() -> Result<PathBuf> {
    crate::product::local_data_dir().context("LOCALAPPDATA is required for direct connection records")
}

fn file(name: &str) -> Result<PathBuf> {
    Ok(dir()?.join(name))
}

fn read<T: DeserializeOwned + Default>(path: &Path) -> Result<T> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .with_context(|| format!("{} 已损坏", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(error) => Err(error).with_context(|| format!("读取 {} 失败", path.display())),
    }
}

/// The trusted devices and known hosts as last read, for a moment: pages
/// draw them every frame. Every write here drops them.
static TRUSTED_READ: Mutex<Option<(std::time::Instant, Vec<TrustedDevice>)>> = Mutex::new(None);
static KNOWN_READ: Mutex<Option<(std::time::Instant, Vec<KnownHost>)>> = Mutex::new(None);
const READ_FOR: std::time::Duration = std::time::Duration::from_secs(1);

fn cached<T: Clone>(slot: &Mutex<Option<(std::time::Instant, T)>>) -> Option<T> {
    slot.lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .filter(|(at, _)| at.elapsed() < READ_FOR)
        .map(|(_, value)| value.clone())
}

fn keep<T>(slot: &Mutex<Option<(std::time::Instant, T)>>, value: T) {
    *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some((std::time::Instant::now(), value));
}

fn forget_reads() {
    *TRUSTED_READ.lock().unwrap_or_else(|e| e.into_inner()) = None;
    *KNOWN_READ.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

fn write<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    forget_reads();
    let parent = path.parent().context("记录目录无效")?;
    std::fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("record"),
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::write(&temporary, serde_json::to_vec_pretty(value)?)?;
    std::fs::rename(&temporary, path).with_context(|| {
        let _ = std::fs::remove_file(&temporary);
        format!("保存 {} 失败", path.display())
    })
}

fn guard() -> std::sync::MutexGuard<'static, ()> {
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// The device ID of `public_key`, if it is a valid device key.
pub(crate) fn device_id_of(public_key: &str) -> Option<String> {
    farwing_protocol::crypto::local_device_id(public_key)
}

// ------------------------------------------------------- trusted devices

/// A device that may connect to this one without an access code
/// (`docs/access-model.md` §2.2), over the server or directly.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct TrustedDevice {
    pub device_id: String,
    pub public_key: String,
    pub name: String,
    /// What the device may do here (always includes viewing).
    #[serde(default)]
    pub grants: Vec<String>,
    #[serde(default)]
    pub trusted_at: i64,
    #[serde(default)]
    pub last_seen: i64,
    /// Both users agreed to pair: it trusts this device too.
    #[serde(default)]
    pub paired: bool,
}

#[derive(Default, Serialize, Deserialize)]
struct Trusted {
    #[serde(default)]
    devices: Vec<TrustedDevice>,
}

const TRUSTED: &str = "trusted_devices.json";
const KNOWN_HOSTS: &str = "known_hosts.json";

/// Whether `device_id` is the ID of `public_key`.
fn keyed(device_id: &str, public_key: &str) -> bool {
    device_id_of(public_key).as_deref() == Some(device_id)
}

fn load_trusted_in(dir: &Path) -> Result<(PathBuf, Trusted)> {
    let path = dir.join(TRUSTED);
    let mut trusted: Trusted = read(&path)?;
    trusted
        .devices
        .retain(|device| keyed(&device.device_id, &device.public_key));
    Ok((path, trusted))
}

fn save_trusted_in(dir: &Path, device: TrustedDevice) -> Result<()> {
    anyhow::ensure!(
        keyed(&device.device_id, &device.public_key),
        "设备 ID 与公钥不符"
    );
    let (path, mut trusted) = load_trusted_in(dir)?;
    trusted.devices.retain(|d| d.device_id != device.device_id);
    trusted.devices.push(device);
    write(&path, &trusted)
}

fn remove_trusted_in(dir: &Path, device_id: &str) -> Result<usize> {
    let (path, mut trusted) = load_trusted_in(dir)?;
    let before = trusted.devices.len();
    trusted.devices.retain(|d| d.device_id != device_id);
    let removed = before - trusted.devices.len();
    if removed > 0 {
        write(&path, &trusted)?;
    }
    Ok(removed)
}

fn set_trusted_grants_in(dir: &Path, device_id: &str, grants: &[String]) -> Result<bool> {
    let (path, mut trusted) = load_trusted_in(dir)?;
    let mut changed = false;
    for device in trusted.devices.iter_mut().filter(|d| d.device_id == device_id) {
        device.grants = with_view(grants);
        changed = true;
    }
    if changed {
        write(&path, &trusted)?;
    }
    Ok(changed)
}

/// `grants` with viewing, normalised.
pub(crate) fn with_view(grants: &[String]) -> Vec<String> {
    let mut grants = grants.to_vec();
    grants.push(farwing_protocol::caps::VIEW.to_owned());
    farwing_protocol::caps::normalize(&grants)
}

/// The devices this one trusts.
pub(crate) fn trusted() -> Vec<TrustedDevice> {
    if let Some(devices) = cached(&TRUSTED_READ) {
        return devices;
    }
    let _guard = guard();
    let devices = dir()
        .and_then(|dir| load_trusted_in(&dir))
        .map(|(_, t)| t.devices)
        .unwrap_or_else(|error| {
            tracing::warn!(error = %format!("{error:#}"), "trusted devices unavailable");
            Vec::new()
        });
    keep(&TRUSTED_READ, devices.clone());
    devices
}

/// Whether this device trusts any other (read at most every few seconds).
pub(crate) fn has_trusted() -> bool {
    static CACHE: Mutex<Option<(std::time::Instant, bool)>> = Mutex::new(None);
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((at, value)) = *cache
        && at.elapsed() < std::time::Duration::from_secs(3)
    {
        return value;
    }
    let value = !trusted().is_empty();
    *cache = Some((std::time::Instant::now(), value));
    value
}

/// The trusted device holding `public_key`.
pub(crate) fn find_trusted(public_key: &str) -> Option<TrustedDevice> {
    let id = device_id_of(public_key)?;
    trusted().into_iter().find(|d| d.device_id == id)
}

/// Trust a device (or replace what it may do).
pub(crate) fn save_trusted(device: TrustedDevice) -> Result<()> {
    let _guard = guard();
    save_trusted_in(&dir()?, device)
}

/// Replace the whole list (the background service's, on uninstallation).
pub(crate) fn replace_trusted(devices: Vec<TrustedDevice>) -> Result<()> {
    let _guard = guard();
    let (path, mut trusted) = load_trusted_in(&dir()?)?;
    trusted.devices = devices
        .into_iter()
        .filter(|device| keyed(&device.device_id, &device.public_key))
        .collect();
    write(&path, &trusted)
}

/// Stop trusting `device_id`. Returns how many records were removed.
pub(crate) fn remove_trusted(device_id: &str) -> Result<usize> {
    let _guard = guard();
    remove_trusted_in(&dir()?, device_id)
}

/// Replace what a trusted device may do.
pub(crate) fn set_trusted_grants(device_id: &str, grants: Vec<String>) -> Result<bool> {
    let _guard = guard();
    set_trusted_grants_in(&dir()?, device_id, &grants)
}

/// Record a connection of a trusted device.
pub(crate) fn touch_trusted(public_key: &str) {
    let Some(id) = device_id_of(public_key) else {
        return;
    };
    let _guard = guard();
    let result = (|| -> Result<()> {
        let (path, mut trusted) = load_trusted_in(&dir()?)?;
        let Some(device) = trusted.devices.iter_mut().find(|d| d.device_id == id) else {
            return Ok(());
        };
        device.last_seen = now();
        write(&path, &trusted)
    })();
    if let Err(error) = result {
        tracing::warn!(error = %format!("{error:#}"), "trusted device record not updated");
    }
}

// ------------------------------------------------------------ known hosts

/// A device this one connected to (controller side).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct KnownHost {
    pub device_id: String,
    pub public_key: String,
    pub name: String,
    /// The direct address that reached it last.
    #[serde(default)]
    pub address: String,
    /// Its short number on the server, when it was reached that way.
    #[serde(default)]
    pub connect_id: String,
    /// It accepted this device without an access code last time (an own
    /// device, a friend's, or one that trusts this device).
    #[serde(default)]
    pub trusted: bool,
    #[serde(default)]
    pub last_seen: i64,
}

#[derive(Default, Serialize, Deserialize)]
struct Known {
    #[serde(default)]
    hosts: Vec<KnownHost>,
}

fn load_known_in(dir: &Path) -> Result<(PathBuf, Known)> {
    let path = dir.join(KNOWN_HOSTS);
    let mut known: Known = read(&path)?;
    known.hosts.retain(|host| keyed(&host.device_id, &host.public_key));
    Ok((path, known))
}

/// The devices this one connected to, most recent first.
pub(crate) fn known_hosts() -> Vec<KnownHost> {
    if let Some(hosts) = cached(&KNOWN_READ) {
        return hosts;
    }
    let _guard = guard();
    let mut hosts = dir()
        .and_then(|dir| load_known_in(&dir))
        .map(|(_, k)| k.hosts)
        .unwrap_or_else(|error| {
            tracing::warn!(error = %format!("{error:#}"), "known hosts unavailable");
            Vec::new()
        });
    hosts.sort_by(|a, b| b.last_seen.cmp(&a.last_seen));
    keep(&KNOWN_READ, hosts.clone());
    hosts
}

/// The known host with this ID.
pub(crate) fn known_host(device_id: &str) -> Option<KnownHost> {
    known_hosts().into_iter().find(|h| h.device_id == device_id)
}

/// Remember a successful connection to a host. Empty fields keep what was
/// known before.
pub(crate) fn remember_host(host: KnownHost) {
    if device_id_of(&host.public_key).as_deref() != Some(host.device_id.as_str()) {
        return;
    }
    let _guard = guard();
    let result = (|| -> Result<()> {
        let (path, mut known) = load_known_in(&dir()?)?;
        let mut host = host;
        if let Some(previous) = known.hosts.iter().find(|h| h.device_id == host.device_id) {
            if host.address.is_empty() {
                host.address = previous.address.clone();
            }
            if host.connect_id.is_empty() {
                host.connect_id = previous.connect_id.clone();
            }
            if host.name.is_empty() {
                host.name = previous.name.clone();
            }
        }
        host.last_seen = now();
        known.hosts.retain(|h| h.device_id != host.device_id);
        known.hosts.push(host);
        // Keep the list short: the most recent 200.
        if known.hosts.len() > 200 {
            known.hosts.sort_by(|a, b| b.last_seen.cmp(&a.last_seen));
            known.hosts.truncate(200);
        }
        write(&path, &known)
    })();
    if let Err(error) = result {
        tracing::warn!(error = %format!("{error:#}"), "known host not saved");
    }
}

/// What a known host said when this device reached it: whether it admits
/// this device by its key. Unknown hosts are not added.
pub(crate) fn note_host_trust(device_id: &str, trusted: bool) {
    let _guard = guard();
    let result = (|| -> Result<()> {
        let (path, mut known) = load_known_in(&dir()?)?;
        let Some(host) = known.hosts.iter_mut().find(|h| h.device_id == device_id) else {
            return Ok(());
        };
        if host.trusted == trusted {
            return Ok(());
        }
        host.trusted = trusted;
        write(&path, &known)
    })();
    if let Err(error) = result {
        tracing::warn!(error = %format!("{error:#}"), "known host not updated");
    }
}

/// Forget a host. Returns whether it was known.
pub(crate) fn forget_host(device_id: &str) -> Result<bool> {
    let _guard = guard();
    let (path, mut known) = load_known_in(&dir()?)?;
    let before = known.hosts.len();
    known.hosts.retain(|h| h.device_id != device_id);
    let removed = known.hosts.len() != before;
    if removed {
        write(&path, &known)?;
    }
    Ok(removed)
}

// ------------------------------------------------------------- own devices

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct OwnDevice {
    pub device_id: String,
    pub alias: String,
    pub public_key: String,
}

#[derive(Default, Serialize, Deserialize)]
struct OwnDevices {
    #[serde(default)]
    user_id: String,
    #[serde(default)]
    updated_at: i64,
    #[serde(default)]
    devices: Vec<OwnDevice>,
}

/// Cache the account's devices (including this one) after a device list.
/// Each process that lists devices keeps its own copy: the hosting process
/// for `own` authorisation, the user's GUI for recognising LAN hosts.
pub(crate) fn save_own_devices(user_id: &str, list: &crate::account::api::DeviceList) {
    let devices = std::iter::once(&list.current_device)
        .chain(&list.devices)
        .filter_map(|d| {
            Some(OwnDevice {
                device_id: device_id_of(&d.public_key)?,
                alias: d.alias.clone(),
                public_key: d.public_key.clone(),
            })
        })
        .collect::<Vec<_>>();
    let _guard = guard();
    let result = file("own_devices.json").and_then(|path| {
        let previous: OwnDevices = read(&path).unwrap_or_default();
        if previous.user_id == user_id
            && previous.devices.len() == devices.len()
            && previous
                .devices
                .iter()
                .zip(&devices)
                .all(|(a, b)| a.device_id == b.device_id && a.public_key == b.public_key && a.alias == b.alias)
            && now() - previous.updated_at < 3600
        {
            return Ok(());
        }
        write(
            &path,
            &OwnDevices {
                user_id: user_id.to_owned(),
                updated_at: now(),
                devices,
            },
        )
    });
    if let Err(error) = result {
        tracing::warn!(error = %format!("{error:#}"), "own device cache not saved");
    }
}

const DEVICE_LIST: &str = "device_list.json";

#[derive(Serialize, Deserialize)]
struct CachedList {
    user_id: String,
    list: crate::account::api::DeviceList,
}

/// What was last written to [`DEVICE_LIST`] (dropped with the file).
static DEVICE_LIST_WRITTEN: Mutex<Option<Vec<u8>>> = Mutex::new(None);

/// Keep the account's device list, shown at startup while the server cannot
/// be reached. Without what will have changed by then (who is online and
/// connected), so it is written only when the devices themselves change.
pub(crate) fn save_device_list(user_id: &str, list: &crate::account::api::DeviceList) {
    let mut list = list.clone();
    for device in std::iter::once(&mut list.current_device)
        .chain(&mut list.devices)
        .chain(&mut list.shared)
    {
        device.status = "offline".into();
        device.participants_info.clear();
    }
    let cached = CachedList {
        user_id: user_id.to_owned(),
        list,
    };
    let Ok(bytes) = serde_json::to_vec(&cached) else {
        return;
    };
    let mut written = DEVICE_LIST_WRITTEN.lock().unwrap_or_else(|e| e.into_inner());
    if written.as_deref() == Some(bytes.as_slice()) {
        return;
    }
    let _guard = guard();
    match file(DEVICE_LIST).and_then(|path| write(&path, &cached)) {
        Ok(()) => *written = Some(bytes),
        Err(error) => tracing::warn!(error = %format!("{error:#}"), "device list cache not saved"),
    }
}

/// The last device list of `user_id`, if this profile kept one.
pub(crate) fn device_list(user_id: &str) -> Option<crate::account::api::DeviceList> {
    let _guard = guard();
    file(DEVICE_LIST)
        .and_then(|path| read::<Option<CachedList>>(&path))
        .ok()
        .flatten()
        .filter(|cache| !user_id.is_empty() && cache.user_id == user_id)
        .map(|cache| cache.list)
}

/// The cached devices of `user_id`; empty for another or no account.
pub(crate) fn own_devices(user_id: &str) -> Vec<OwnDevice> {
    if user_id.is_empty() {
        return Vec::new();
    }
    let _guard = guard();
    file("own_devices.json")
        .and_then(|path| read::<OwnDevices>(&path))
        .ok()
        .filter(|cache| cache.user_id == user_id)
        .map(|cache| {
            cache
                .devices
                .into_iter()
                .filter(|d| keyed(&d.device_id, &d.public_key))
                .collect()
        })
        .unwrap_or_default()
}

/// The cached own device with this key.
pub(crate) fn own_device(user_id: &str, public_key: &str) -> Option<OwnDevice> {
    own_devices(user_id)
        .into_iter()
        .find(|d| d.public_key == public_key)
}

/// The account of the cache, if any (a controller without a live login
/// still recognises its account's hosts).
pub(crate) fn own_devices_account() -> Option<String> {
    let _guard = guard();
    file("own_devices.json")
        .and_then(|path| read::<OwnDevices>(&path))
        .ok()
        .map(|cache| cache.user_id)
        .filter(|id| !id.is_empty())
}

/// Forget the account caches (logout).
pub(crate) fn clear_own_devices() {
    *DEVICE_LIST_WRITTEN.lock().unwrap_or_else(|e| e.into_inner()) = None;
    let _guard = guard();
    for name in ["own_devices.json", FRIEND_DEVICES, DEVICE_LIST] {
        if let Ok(path) = file(name) {
            let _ = std::fs::remove_file(path);
        }
    }
}

// ---------------------------------------------------------- friend devices

const FRIEND_DEVICES: &str = "friend_devices.json";

/// A friend's device that may control this one, as the server last said
/// (`docs/access-model.md` §2.2: friends over direct connections).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct FriendDevice {
    pub device_id: String,
    pub public_key: String,
    pub alias: String,
    pub user_id: String,
    pub nickname: String,
    /// What the friend may do on this device.
    pub grants: Vec<String>,
}

#[derive(Default, Serialize, Deserialize)]
struct FriendDevices {
    #[serde(default)]
    user_id: String,
    #[serde(default)]
    host_device_id: String,
    #[serde(default)]
    updated_at: i64,
    #[serde(default)]
    devices: Vec<FriendDevice>,
}

/// How long a cached friend grant stays usable without the server.
const FRIEND_CACHE_TTL_S: i64 = 7 * 24 * 3600;

/// Cache who among the account's friends may control this device.
pub(crate) fn save_friend_devices(user_id: &str, host_device_id: &str, devices: Vec<FriendDevice>) {
    let devices = devices
        .into_iter()
        .filter(|d| keyed(&d.device_id, &d.public_key))
        .filter_map(|mut d| {
            d.grants = farwing_protocol::caps::normalize(&d.grants);
            (!d.grants.is_empty()).then_some(d)
        })
        .collect::<Vec<_>>();
    let _guard = guard();
    let result = file(FRIEND_DEVICES).and_then(|path| {
        write(
            &path,
            &FriendDevices {
                user_id: user_id.to_owned(),
                host_device_id: host_device_id.to_owned(),
                updated_at: now(),
                devices,
            },
        )
    });
    if let Err(error) = result {
        tracing::warn!(error = %format!("{error:#}"), "friend device cache not saved");
    }
}

/// The cached friend devices that may control `host_device_id` of
/// `user_id`; empty when the cache is for another account or too old.
pub(crate) fn friend_devices(user_id: &str, host_device_id: &str) -> Vec<FriendDevice> {
    if user_id.is_empty() {
        return Vec::new();
    }
    let _guard = guard();
    file(FRIEND_DEVICES)
        .and_then(|path| read::<FriendDevices>(&path))
        .ok()
        .filter(|cache| {
            cache.user_id == user_id
                && cache.host_device_id == host_device_id
                && now() - cache.updated_at < FRIEND_CACHE_TTL_S
        })
        .map(|cache| cache.devices)
        .unwrap_or_default()
}

/// The cached friend device holding `public_key`.
pub(crate) fn friend_device(
    user_id: &str,
    host_device_id: &str,
    public_key: &str,
) -> Option<FriendDevice> {
    friend_devices(user_id, host_device_id)
        .into_iter()
        .find(|d| d.public_key == public_key)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(n: u8) -> String {
        farwing_protocol::crypto::b64(&[n; 32])
    }

    fn trusted_device(n: u8) -> TrustedDevice {
        TrustedDevice {
            device_id: device_id_of(&key(n)).unwrap(),
            public_key: key(n),
            name: format!("PC{n}"),
            grants: vec!["view".into()],
            trusted_at: 1,
            last_seen: 0,
            paired: false,
        }
    }

    fn scratch() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("farwing-store-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn trusted_devices_are_keyed_by_the_derived_id() {
        let dir = scratch();
        let mut stale = trusted_device(1);
        stale.device_id = "stale0000000000x".into();
        assert!(save_trusted_in(&dir, stale).is_err());
        save_trusted_in(&dir, trusted_device(1)).unwrap();
        let devices = load_trusted_in(&dir).unwrap().1.devices;
        assert_eq!(devices[0].device_id, device_id_of(&key(1)).unwrap());
        let id = devices[0].device_id.clone();
        assert!(set_trusted_grants_in(&dir, &id, &["control".into(), "bogus".into()]).unwrap());
        assert_eq!(
            load_trusted_in(&dir).unwrap().1.devices[0].grants,
            vec!["view".to_owned(), "control".to_owned()]
        );
        assert_eq!(remove_trusted_in(&dir, &id).unwrap(), 1);
        assert!(load_trusted_in(&dir).unwrap().1.devices.is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }
}
