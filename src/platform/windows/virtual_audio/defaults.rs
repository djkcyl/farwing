//! Default endpoints belong to the interactive user, not the SYSTEM resident.
//! A durable lease restores only defaults that still point at our own device.
use super::super::{
    components::files,
    host_service::{pipe::Handle, process, vault},
};
use anyhow::{Context, Result, ensure};
use std::{marker::PhantomData, path::PathBuf, rc::Rc};
use windows::{
    Win32::{
        Foundation::{ERROR_NOT_FOUND, HANDLE, HLOCAL, LocalFree, PROPERTYKEY},
        Media::Audio::*,
        Security::{
            Authorization::ConvertSidToStringSidW, GetTokenInformation, ImpersonateLoggedOnUser,
            RevertToSelf, TOKEN_USER, TokenUser,
        },
        System::{
            Com::{
                StructuredStorage::{PropVariantClear, PropVariantToString},
                *,
            },
            RemoteDesktop::WTSQueryUserToken,
            Threading::GetCurrentProcessId,
        },
    },
    core::{GUID, HRESULT, IUnknown, IUnknown_Vtbl, Interface, PCWSTR},
};

windows::core::imp::define_interface!(Policy, PolicyVtbl, 0xf8679f50_850a_41cf_9c72_430f290290c8);
#[repr(C)]
pub struct PolicyVtbl {
    base: IUnknown_Vtbl,
    // IPolicyConfig's ten methods preceding SetDefaultEndpoint are unused.
    unused: [usize; 10],
    set_default: unsafe extern "system" fn(*mut core::ffi::c_void, PCWSTR, ERole) -> HRESULT,
}
windows::core::imp::interface_hierarchy!(Policy, IUnknown);
const POLICY: GUID = GUID::from_u128(0x870af99c_171d_4f9e_af0d_e63df40c2bc9);
const MATCHING_DEVICE: PROPERTYKEY = PROPERTYKEY {
    fmtid: GUID::from_u128(0xa8b865dd_2e3d_4094_ad97_e593a70c75d6),
    pid: 8,
};
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}
fn id(device: &IMMDevice) -> Result<String> {
    let raw = unsafe { device.GetId()? };
    let result = unsafe { raw.to_string() };
    unsafe { CoTaskMemFree(Some(raw.0.cast())) };
    Ok(result?)
}
fn ours(device: &IMMDevice) -> Result<bool> {
    unsafe {
        let store = device.OpenPropertyStore(STGM_READ)?;
        let mut value = store.GetValue(&MATCHING_DEVICE)?;
        let mut text = [0u16; 256];
        let result = PropVariantToString(&value, &mut text);
        let _ = PropVariantClear(&mut value);
        result?;
        Ok(String::from_utf16_lossy(
            &text[..text.iter().position(|c| *c == 0).unwrap_or(text.len())],
        )
        .eq_ignore_ascii_case(r"ROOT\FARWING_AUDIO"))
    }
}
pub(super) struct Scope {
    pub(super) impersonated: bool,
    pub(super) apartment: bool,
}
impl Drop for Scope {
    fn drop(&mut self) {
        unsafe {
            if self.apartment {
                CoUninitialize();
            }
            if self.impersonated {
                let _ = RevertToSelf();
            }
        }
    }
}
pub(super) struct User {
    token: Option<Handle>,
    sid: String,
    // COM and impersonation scopes must stay on their owning worker thread.
    _thread: PhantomData<Rc<()>>,
}
impl User {
    pub fn current() -> Result<Self> {
        let mut sid = vault::sid(unsafe { GetCurrentProcessId() })?;
        let system = sid == "S-1-5-18";
        let token = if system {
            let mut raw = HANDLE::default();
            unsafe { WTSQueryUserToken(process::active_session(), &mut raw) }
                .context("当前没有可设置默认麦克风的登录用户")?;
            let token = Handle(raw);
            let mut needed = 0;
            unsafe {
                let _ = GetTokenInformation(raw, TokenUser, None, 0, &mut needed);
            }
            ensure!(needed > 0 && needed < 65536, "音频用户身份无效");
            let mut buffer = vec![0usize; (needed as usize).div_ceil(size_of::<usize>())];
            unsafe {
                GetTokenInformation(
                    raw,
                    TokenUser,
                    Some(buffer.as_mut_ptr().cast()),
                    needed,
                    &mut needed,
                )?;
                let user = &*buffer.as_ptr().cast::<TOKEN_USER>();
                let mut text = windows::core::PWSTR::null();
                ConvertSidToStringSidW(user.User.Sid, &mut text)?;
                let value = text.to_string();
                LocalFree(Some(HLOCAL(text.0.cast())));
                sid = value?;
            }
            Some(token)
        } else {
            None
        };
        Ok(Self {
            token,
            sid,
            _thread: PhantomData,
        })
    }
    pub(super) fn with<T>(
        &self,
        f: impl FnOnce(IMMDeviceEnumerator, Policy) -> Result<T>,
    ) -> Result<T> {
        let mut scope = Scope {
            impersonated: false,
            apartment: false,
        };
        if let Some(token) = &self.token {
            unsafe { ImpersonateLoggedOnUser(token.0)? };
            scope.impersonated = true;
        }
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if hr.0 != 0x80010106u32 as i32 {
            hr.ok()?;
            scope.apartment = true;
        }
        let enumerator: IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };
        let policy: Policy = unsafe { CoCreateInstance(&POLICY, None, CLSCTX_ALL)? };
        if scope.impersonated {
            for object in [enumerator.cast::<IUnknown>()?, policy.cast::<IUnknown>()?] {
                let result = unsafe {
                    CoSetProxyBlanket(
                        &object,
                        u32::MAX,
                        u32::MAX,
                        None,
                        RPC_C_AUTHN_LEVEL_DEFAULT,
                        RPC_C_IMP_LEVEL_IMPERSONATE,
                        None,
                        EOAC_DYNAMIC_CLOAKING,
                    )
                };
                // In-process implementations have no COM proxy. Their calls
                // execute inside the same bounded impersonation scope.
                if let Err(error) = result {
                    if error.code().0 != 0x80004002u32 as i32 {
                        return Err(error.into());
                    }
                }
            }
        }
        f(enumerator, policy)
    }
}
#[derive(Debug, PartialEq, serde::Serialize, serde::Deserialize)]
struct Role {
    flow: i32,
    role: i32,
    original: String,
    #[serde(default)]
    target: Option<String>,
}
#[derive(serde::Serialize, serde::Deserialize)]
struct Record {
    roles: Vec<Role>,
}
impl Record {
    fn load(path: &std::path::Path) -> Result<Self> {
        use std::io::Read;
        let mut bytes = Vec::new();
        std::fs::File::open(path)?
            .take(32768)
            .read_to_end(&mut bytes)?;
        ensure!(bytes.len() < 32768, "音频恢复记录过大");
        let record: Self = serde_json::from_slice(&bytes)?;
        ensure!(
            record.roles.len() <= 6
                && record.roles.iter().all(|r| (0..=1).contains(&r.flow)
                    && (0..=2).contains(&r.role)
                    && r.original.len() < 4096),
            "音频恢复记录无效"
        );
        Ok(record)
    }
}
pub(crate) struct Defaults {
    user: Rc<User>,
    record: Record,
    journal: Option<PathBuf>,
    watch: Option<super::defaults_watch::Watch>,
}
fn current(e: &IMMDeviceEnumerator, flow: EDataFlow, role: ERole) -> Result<Option<IMMDevice>> {
    match unsafe { e.GetDefaultAudioEndpoint(flow, role) } {
        Ok(device) => Ok(Some(device)),
        Err(error) if error.code() == ERROR_NOT_FOUND.to_hresult() => Ok(None),
        Err(error) => Err(error.into()),
    }
}
impl Defaults {
    pub fn recover_defaults() -> Result<()> {
        // Recovery must not require an audio endpoint when no lease exists.
        Self::recover("speakers")?;
        Self::recover("microphone")
    }
    pub(crate) fn ensure_recovered() -> Result<()> {
        Self::recover_defaults()?;
        Self::recover("install")?;
        let directory = vault::root()?
            .parent()
            .context("音频恢复目录无效")?
            .join("audio-defaults");
        files::reject_reparse(&directory)?;
        if !directory.exists() {
            return Ok(());
        }
        for entry in std::fs::read_dir(directory)? {
            let path = entry?.path();
            files::reject_reparse(&path)?;
            if path
                .extension()
                .is_some_and(|extension| extension == "json")
            {
                ensure!(
                    Record::load(&path)?.roles.is_empty(),
                    "其他Windows用户仍有音频设备待恢复，请完成恢复后再清除数据"
                );
            }
        }
        Ok(())
    }
    fn recover(purpose: &str) -> Result<()> {
        let user = Rc::new(User::current()?);
        let path = vault::root()?
            .parent()
            .context("音频恢复目录无效")?
            .join("audio-defaults")
            .join(format!("{purpose}-{}.json", user.sid));
        files::reject_reparse(path.parent().unwrap())?;
        files::reject_reparse(&path)?;
        if !path.exists() {
            return Ok(());
        }
        let record = Record::load(&path)?;
        Self {
            user,
            record,
            journal: Some(path),
            watch: None,
        }
        .restore()
    }
    pub fn snapshot() -> Result<Self> {
        Self::capture(None)
    }
    pub(crate) fn wait_for_install(&self) -> Result<()> {
        // PnP readiness precedes AudioEndpointBuilder publication and default
        // selection. Restore defaults only after that asynchronous phase settles.
        self.user.with(|e, _| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            let mut previous = None;
            let mut settled = std::time::Instant::now();
            loop {
                let mut state = Vec::new();
                let mut ready = true;
                for flow in [eRender, eCapture] {
                    let devices = unsafe { e.EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE)? };
                    let mut own = Vec::new();
                    for index in 0..unsafe { devices.GetCount()? } {
                        let device = unsafe { devices.Item(index)? };
                        if ours(&device).unwrap_or(false) {
                            own.push(id(&device)?);
                        }
                    }
                    ready &= own.len() == 1;
                    state.extend(own);
                    for role in [eConsole, eMultimedia, eCommunications] {
                        state.push(
                            current(&e, flow, role)?
                                .as_ref()
                                .map(id)
                                .transpose()?
                                .unwrap_or_default(),
                        );
                    }
                }
                if !ready || previous.as_ref() != Some(&state) {
                    settled = std::time::Instant::now();
                    previous = Some(state);
                }
                if ready && settled.elapsed() >= std::time::Duration::from_millis(250) {
                    return Ok(());
                }
                ensure!(
                    std::time::Instant::now() < deadline,
                    "虚拟音频端点未完成初始化，请检查Windows音频服务"
                );
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
        })
    }
    fn capture(selection: Option<(bool, bool)>) -> Result<Self> {
        let user = Rc::new(User::current()?);
        let directory = vault::root()?
            .parent()
            .context("音频恢复目录无效")?
            .join("audio-defaults");
        files::reject_reparse(directory.parent().unwrap())?;
        files::reject_reparse(&directory)?;
        std::fs::create_dir_all(&directory)?;
        files::secure_directory(&directory)?;
        let purpose = match selection {
            Some((true, false)) => "speakers",
            Some(_) => "microphone",
            None => "install",
        };
        let journal = directory.join(format!("{purpose}-{}.json", user.sid));
        files::reject_reparse(&journal)?;
        if journal.exists() {
            let record = Record::load(&journal)?;
            let mut previous = Self {
                user,
                record,
                journal: Some(journal),
                watch: None,
            };
            previous.restore()?;
            return Self::capture(selection);
        }
        let roles = user.with(|e, _| {
            let mut roles = Vec::new();
            for flow in [eRender, eCapture] {
                for role in [eConsole, eMultimedia, eCommunications] {
                    if selection.is_some_and(|(speakers, microphone)| {
                        if flow == eRender {
                            !speakers
                        } else {
                            !microphone
                        }
                    }) {
                        continue;
                    }
                    if let Some(device) = current(&e, flow, role)? {
                        roles.push(Role {
                            flow: flow.0,
                            role: role.0,
                            original: id(&device)?,
                            target: None,
                        });
                    }
                }
            }
            Ok(roles)
        })?;
        if let Some((speakers, microphone)) = selection {
            ensure!(
                roles.len() == 3 * (usize::from(speakers) + usize::from(microphone)),
                "当前用户没有完整的默认音频设备，无法安全临时切换"
            );
        }
        let record = Record { roles };
        let temporary = journal.with_extension("tmp");
        files::reject_reparse(&temporary)?;
        {
            use std::io::Write;
            let mut file = std::fs::File::create(&temporary)?;
            file.write_all(&serde_json::to_vec(&record)?)?;
            file.sync_all()?;
        }
        std::fs::rename(&temporary, &journal)?;
        Ok(Self {
            user,
            record,
            journal: Some(journal),
            watch: None,
        })
    }
    pub fn for_connection(speakers: bool, microphone: bool) -> Result<Option<Self>> {
        if !speakers && !microphone {
            return Ok(None);
        }
        let mut lease = Self::capture(Some((speakers, microphone)))?;
        let user = lease.user.clone();
        let targets = user.with(|e, _| {
            let mut targets = Vec::new();
            for (flow, selected) in [(eRender, speakers), (eCapture, microphone)] {
                if !selected {
                    continue;
                }
                let devices = unsafe { e.EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE)? };
                let mut target = None;
                for index in 0..unsafe { devices.GetCount()? } {
                    let device = unsafe { devices.Item(index)? };
                    if ours(&device).unwrap_or(false) {
                        ensure!(target.is_none(), "本程序虚拟音频端点不唯一");
                        target = Some(id(&device)?);
                    }
                }
                let target = target.context("未找到本程序虚拟音频设备")?;
                targets.push((flow, target));
            }
            Ok(targets)
        })?;
        for (flow, target) in &targets {
            for saved in &mut lease.record.roles {
                if saved.flow == flow.0 {
                    saved.target = Some(target.clone());
                }
            }
        }
        // The journal is administrator-owned; never write it while scoped to
        // the ordinary interactive user's token.
        lease.save()?;
        user.with(|e, policy| {
            for (flow, target) in targets {
                for role in [eConsole, eMultimedia, eCommunications] {
                    if current(&e, flow, role)?
                        .as_ref()
                        .map(id)
                        .transpose()?
                        .as_deref()
                        == Some(target.as_str())
                    {
                        continue;
                    }
                    unsafe {
                        (policy.vtable().set_default)(
                            policy.as_raw(),
                            PCWSTR(wide(&target).as_ptr()),
                            role,
                        )
                        .ok()?
                    };
                }
            }
            Ok(())
        })?;
        lease.watch = Some(super::defaults_watch::Watch::new(lease.user.clone())?);
        Ok(Some(lease))
    }
    fn save(&self) -> Result<()> {
        if let Some(path) = &self.journal {
            use std::io::Write;
            let temporary = path.with_extension("tmp");
            files::reject_reparse(&temporary)?;
            let mut file = std::fs::File::create(&temporary)?;
            file.write_all(&serde_json::to_vec(&self.record)?)?;
            file.sync_all()?;
            drop(file);
            std::fs::rename(temporary, path)?;
        }
        Ok(())
    }
    fn maintain(&mut self) -> Result<()> {
        let Some(watch) = &self.watch else {
            return Ok(());
        };
        let (overflow, changes) = watch.drain();
        let before = self.record.roles.len();
        // Once another selection is observed, that role is no longer ours,
        // even if the user later selects the virtual device again.
        self.record.roles.retain(|saved| {
            !overflow
                && !changes.iter().any(|change| {
                    change.flow == saved.flow
                        && change.role == saved.role
                        && change.id != saved.target
                })
        });
        if before != self.record.roles.len() {
            self.save()?;
        }
        Ok(())
    }
    pub fn restore(&mut self) -> Result<()> {
        self.maintain()?;
        let result = self.user.with(|e, policy| {
            let mut error = None;
            for saved in &self.record.roles {
                let result = (|| -> Result<()> {
                    let Some(device) = current(&e, EDataFlow(saved.flow), ERole(saved.role))?
                    else {
                        return Ok(());
                    };
                    let current_id = id(&device)?;
                    if current_id == saved.original
                        || !ours(&device).unwrap_or(false)
                        || saved
                            .target
                            .as_ref()
                            .is_some_and(|target| *target != current_id)
                    {
                        return Ok(());
                    }
                    let Ok(original) =
                        (unsafe { e.GetDevice(PCWSTR(wide(&saved.original).as_ptr())) })
                    else {
                        return Ok(());
                    };
                    if unsafe { original.GetState()? } != DEVICE_STATE_ACTIVE {
                        return Ok(());
                    }
                    unsafe {
                        (policy.vtable().set_default)(
                            policy.as_raw(),
                            PCWSTR(wide(&saved.original).as_ptr()),
                            ERole(saved.role),
                        )
                        .ok()?
                    };
                    Ok(())
                })();
                if let Err(failure) = result {
                    error = Some(failure);
                }
            }
            if let Some(error) = error {
                Err(error)
            } else {
                Ok(())
            }
        });
        if result.is_ok() {
            if let Some(path) = &self.journal {
                std::fs::remove_file(path)?;
            }
            self.journal = None;
            self.record.roles.clear();
        }
        result
    }
}

impl Drop for Defaults {
    fn drop(&mut self) {
        if let Err(error) = self.restore() {
            tracing::warn!(%error,"audio defaults restoration incomplete");
        }
    }
}

#[derive(Default)]
pub(crate) struct Routing {
    speaker: Option<Defaults>,
    microphone: Option<Defaults>,
}
impl Routing {
    pub fn apply(&mut self, speakers: bool, microphone: bool) -> Result<()> {
        self.maintain()?;
        // Each direction owns its own snapshot. Changing one must not bounce
        // the other direction or reclaim a role the user changed manually.
        for (slot, wanted, render) in [
            (&mut self.speaker, speakers, true),
            (&mut self.microphone, microphone, false),
        ] {
            if wanted && slot.is_none() {
                *slot = Defaults::for_connection(render, !render)?;
            } else if !wanted {
                if let Some(lease) = slot {
                    lease.restore()?;
                }
                *slot = None;
            }
        }
        Ok(())
    }
    pub fn maintain(&mut self) -> Result<()> {
        for lease in [&mut self.speaker, &mut self.microphone]
            .into_iter()
            .flatten()
        {
            lease.maintain()?;
        }
        Ok(())
    }
}
