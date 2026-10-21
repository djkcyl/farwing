//! Source OLE objects stay on one STA; remote providers are thread-safe COM objects.
//! COM callbacks may pump the calling thread's messages;
//! no RefCell borrow or application mutex is held across a call into OLE/network.
use super::formats::{self, Format};
use super::*;
use std::{
    cell::{Cell, RefCell},
    io::{Read, Seek, SeekFrom},
    mem::ManuallyDrop,
    os::windows::{
        fs::{MetadataExt, OpenOptionsExt},
        io::AsRawHandle,
    },
    path::{Path, PathBuf},
    sync::{
        OnceLock,
        mpsc::{Receiver, SyncSender, sync_channel},
    },
};
use windows::{
    Win32::{
        Foundation::*,
        Graphics::Gdi::*,
        System::{Com::*, DataExchange::*, LibraryLoader::GetModuleHandleW, Memory::*, Ole::*},
        UI::{Shell::*, WindowsAndMessaging::*},
    },
    core::{BOOL, HRESULT, Interface, Ref, implement},
};
#[link(name = "ole32")]
unsafe extern "system" {
    #[link_name = "OleIsCurrentClipboard"]
    fn ole_is_current(object: *mut std::ffi::c_void) -> HRESULT;
}
fn is_current(object: &IDataObject) -> bool {
    unsafe { ole_is_current(object.as_raw()) == S_OK }
}
fn clipboard_retry<T>(
    mut action: impl FnMut() -> windows::core::Result<T>,
) -> windows::core::Result<T> {
    for attempt in 0..10 {
        match action() {
            Err(error) if error.code() == HRESULT(0x800401D0u32 as i32) && attempt < 9 => {
                std::thread::sleep(Duration::from_millis(10))
            }
            result => return result,
        }
    }
    unreachable!()
}

// Shared by all profiles: no instance may echo another one's remote copy.
const REMOTE_MARKER: &str = "Farwing.Clipboard.Remote";
const WAKE: u32 = WM_APP + 73;
pub(super) enum Command {
    Activate(Weak<Inner>),
    Remove(u64),
    /// The peer's offer: epoch, offer number, formats, and whether it
    /// answers our `Pull` (applied even without automatic sync).
    Offer(Weak<Inner>, u64, u32, Vec<wire::Format>, bool),
    /// A peer request: epoch, request ID, body.
    Request(Weak<Inner>, u64, u32, Body),
    /// The peer stopped reading the files of our offer (session, offer).
    Release(u64, u32),
    PublishFiles(
        Weak<Inner>,
        u64,
        PreparedFiles,
        tokio::sync::oneshot::Sender<Result<(), String>>,
    ),
}
struct Worker {
    sender: SyncSender<Command>,
    hwnd: isize,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}
static WORKER: OnceLock<std::result::Result<Worker, String>> = OnceLock::new();
thread_local! {static STATE:RefCell<Option<State>>=const{RefCell::new(None)};static WRITING:Cell<bool>=const{Cell::new(false)};}
struct Writing(bool);
impl Writing {
    fn new() -> Self {
        Self(WRITING.with(|w| w.replace(true)))
    }
}
impl Drop for Writing {
    fn drop(&mut self) {
        WRITING.with(|w| w.set(self.0));
    }
}
struct State {
    receiver: Receiver<Command>,
    sessions: HashMap<u64, Weak<Inner>>,
    /// Our current offer per session: source, formats, offer number.
    published: HashMap<u64, (Arc<LocalSource>, Vec<Format>, u32)>,
    /// File lists the peer is reading, by session and offer.
    tasks: HashMap<(u64, u32), Arc<LocalFiles>>,
    owner: Option<(IDataObject, Arc<RemoteOffer>)>,
    sequence: u32,
}
pub(super) fn start() -> Result<()> {
    WORKER
        .get_or_init(|| {
            let (sender, receiver) = sync_channel(64);
            let (ready, wait) = sync_channel(1);
            let thread = std::thread::Builder::new()
                .name("farwing-clipboard-sta".into())
                .spawn(move || unsafe {
                    let result = (|| -> windows::core::Result<HWND> {
                        OleInitialize(None)?;
                        let module = GetModuleHandleW(None)?;
                        let class = windows::core::w!("Farwing.Clipboard.STA");
                        let wc = WNDCLASSW {
                            lpfnWndProc: Some(window_proc),
                            hInstance: module.into(),
                            lpszClassName: class,
                            ..Default::default()
                        };
                        if RegisterClassW(&wc) == 0 {
                            return Err(windows::core::Error::from_thread());
                        }
                        STATE.with(|s| {
                            *s.borrow_mut() = Some(State {
                                receiver,
                                sessions: HashMap::new(),
                                published: HashMap::new(),
                                tasks: HashMap::new(),
                                owner: None,
                                sequence: 0,
                            })
                        });
                        let hwnd = CreateWindowExW(
                            WINDOW_EX_STYLE(0),
                            class,
                            windows::core::w!(""),
                            WINDOW_STYLE(0),
                            0,
                            0,
                            0,
                            0,
                            Some(HWND_MESSAGE),
                            None,
                            Some(module.into()),
                            None,
                        )?;
                        AddClipboardFormatListener(hwnd)?;
                        Ok(hwnd)
                    })();
                    match result {
                        Ok(hwnd) => {
                            let _ = ready.send(Ok(hwnd.0 as isize));
                            let mut msg = MSG::default();
                            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                                let _ = TranslateMessage(&msg);
                                DispatchMessageW(&msg);
                            }
                            let _ = RemoveClipboardFormatListener(hwnd);
                            let _ = DestroyWindow(hwnd);
                        }
                        Err(e) => {
                            let _ = ready.send(Err(e.to_string()));
                        }
                    }
                    STATE.with(|s| {
                        s.borrow_mut().take();
                    });
                    OleUninitialize();
                })
                .map_err(|e| e.to_string())?;
            let hwnd = wait.recv().map_err(|e| e.to_string())??;
            Ok(Worker {
                sender,
                hwnd,
                thread: Mutex::new(Some(thread)),
            })
        })
        .as_ref()
        .map(|_| ())
        .map_err(|e| anyhow!(e.clone()))
}
pub(super) fn post(command: Command) -> Result<()> {
    let Some(worker) = WORKER.get() else {
        return if matches!(command, Command::Remove(_)) {
            Ok(())
        } else {
            Err(anyhow!("剪贴板服务尚未启动"))
        };
    };
    let worker = worker.as_ref().map_err(|e| anyhow!(e.clone()))?;
    worker
        .sender
        .try_send(command)
        .map_err(|_| anyhow!("剪贴板工作队列繁忙"))?;
    unsafe {
        PostMessageW(Some(HWND(worker.hwnd as _)), WAKE, WPARAM(0), LPARAM(0))?;
    }
    Ok(())
}
pub(super) fn pump() {
    unsafe {
        let mut msg = MSG::default();
        for _ in 0..64 {
            if !PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                break;
            }
            if msg.message == WM_QUIT {
                PostQuitMessage(msg.wParam.0 as i32);
                break;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}
pub(super) fn shutdown() {
    if let Some(Ok(worker)) = WORKER.get() {
        let _ =
            unsafe { PostMessageW(Some(HWND(worker.hwnd as _)), WM_CLOSE, WPARAM(0), LPARAM(0)) };
        if let Some(thread) = lock(&worker.thread).take() {
            let _ = thread.join();
        }
    }
}
unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match msg {
        WM_CLOSE => {
            for s in sessions() {
                s.active.store(false, Ordering::Release);
                s.cancel_pending();
            }
            retire_owner(true);
            unsafe { PostQuitMessage(0) };
            true
        }
        WM_CLIPBOARDUPDATE => {
            if let Err(e) = local_change() {
                set_error_all(e.to_string());
            }
            true
        }
        WAKE => {
            for _ in 0..64 {
                let command =
                    STATE.with(|s| s.borrow().as_ref().and_then(|s| s.receiver.try_recv().ok()));
                let Some(command) = command else {
                    break;
                };
                process(command);
            }
            true
        }
        _ => false,
    }));
    if matches!(result, Ok(true)) {
        LRESULT(0)
    } else {
        unsafe { DefWindowProcW(hwnd, msg, w, l) }
    }
}
fn sessions() -> Vec<Arc<Inner>> {
    STATE.with(|s| {
        s.borrow()
            .as_ref()
            .map(|s| {
                s.sessions
                    .values()
                    .filter_map(Weak::upgrade)
                    .filter(|s| s.valid(s.epoch.load(Ordering::Acquire)))
                    .collect()
            })
            .unwrap_or_default()
    })
}
fn set_error_all(error: String) {
    for s in sessions().into_iter().filter(|s| s.file_offers.is_none()) {
        s.fail(error.clone());
    }
}
fn retire_owner(clear: bool) {
    let owner = STATE.with(|s| s.borrow_mut().as_mut().and_then(|s| s.owner.take()));
    if let Some((object, offer)) = owner {
        offer.cancel();
        offer.session.cancel_pending();
        if clear && is_current(&object) {
            let _writing = Writing::new();
            if let Err(error) = retain_rendered(&offer) {
                offer.session.fail(error.to_string());
            }
        }
    }
}
fn retain_rendered(offer: &RemoteOffer) -> Result<()> {
    let hwnd = WORKER
        .get()
        .and_then(|w| w.as_ref().ok())
        .map(|w| HWND(w.hwnd as _))
        .ok_or_else(|| anyhow!("剪贴板窗口已关闭"))?;
    let cache = lock(&offer.cached).clone();
    let mut opened = false;
    for _ in 0..10 {
        if unsafe { OpenClipboard(Some(hwnd)) }.is_ok() {
            opened = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    ensure!(opened, "剪贴板正在被其他程序占用");
    let result = (|| -> Result<()> {
        unsafe {
            EmptyClipboard()?;
        }
        // Preserve the local-only origin marker while retaining rendered data.
        // Another Farwing process must not rebroadcast this cleanup as a new local copy.
        let marker = memory_medium(&[1])?;
        if unsafe {
            SetClipboardData(
                formats::register(REMOTE_MARKER),
                Some(HANDLE(marker.u.hGlobal.0)),
            )
        }
        .is_err()
        {
            let mut marker = marker;
            unsafe {
                ReleaseStgMedium(&mut marker);
            }
        }
        for (id, bytes) in cache {
            if matches!(id, 14 | 0x8e) {
                unsafe {
                    let h = SetEnhMetaFileBits(&bytes);
                    if !h.is_invalid() && SetClipboardData(id, Some(HANDLE(h.0))).is_err() {
                        let _ = DeleteEnhMetaFile(Some(h));
                    }
                }
            } else {
                let mut medium = memory_medium(&bytes)?;
                if unsafe { SetClipboardData(id, Some(HANDLE(medium.u.hGlobal.0))) }.is_err() {
                    unsafe {
                        ReleaseStgMedium(&mut medium);
                    }
                }
            }
        }
        Ok(())
    })();
    let _ = unsafe { CloseClipboard() };
    result
}
fn process(command: Command) {
    match command {
        Command::PublishFiles(weak, epoch, files, reply) => {
            let result = (|| -> Result<()> {
                let s = weak
                    .upgrade()
                    .filter(|s| s.valid(epoch) && s.file_allowed())
                    .ok_or_else(|| anyhow!("文件发布已取消"))?;
                let ids = vec![
                    (
                        formats::register("FileGroupDescriptorW"),
                        "FileGroupDescriptorW".into(),
                    ),
                    (formats::register("FileContents"), "FileContents".into()),
                ];
                let links = formats::outgoing(&ids, s.platform.load(Ordering::Acquire), true);
                let source = Arc::new(LocalSource {
                    object: None,
                    files: true,
                    prepared: Some(Arc::new(LocalFiles {
                        items: files.items,
                        object: None,
                    })),
                });
                let offer = s.offer(epoch, links.iter().map(|v| v.wire.clone()).collect())?;
                STATE.with(|slot| {
                    if let Some(state) = slot.borrow_mut().as_mut() {
                        state.published.insert(s.id, (source, links, offer));
                    }
                });
                Ok(())
            })();
            let _ = reply.send(result.map_err(|e| e.to_string()));
        }
        Command::Activate(weak) => {
            if let Some(s) = weak.upgrade() {
                STATE.with(|v| {
                    if let Some(v) = v.borrow_mut().as_mut() {
                        v.sessions.insert(s.id, Arc::downgrade(&s));
                    }
                });
            }
        }
        Command::Remove(id) => {
            let clear = STATE.with(|v| {
                let mut v = v.borrow_mut();
                let Some(v) = v.as_mut() else {
                    return false;
                };
                v.sessions.remove(&id);
                v.published.remove(&id);
                v.tasks.retain(|(s, _), _| *s != id);
                v.owner.as_ref().is_some_and(|(_, o)| o.session.id == id)
            });
            if clear {
                retire_owner(true);
            }
        }
        Command::Release(session, offer) => STATE.with(|v| {
            if let Some(v) = v.borrow_mut().as_mut() {
                v.tasks.remove(&(session, offer));
            }
        }),
        Command::Offer(weak, epoch, offer, formats, pulled) => {
            if let Some(s) = weak
                .upgrade()
                .filter(|s| s.valid(epoch) && (pulled || s.auto.load(Ordering::Acquire)))
            {
                if let Err(e) = set_offer(s.clone(), epoch, offer, formats) {
                    s.fail(e.to_string());
                }
            }
        }
        Command::Request(weak, epoch, id, body) => {
            if let Some(s) = weak.upgrade() {
                if !s.valid(epoch) {
                    s.refuse(
                        id,
                        farwing_protocol::peer::error_code::UNAVAILABLE,
                        "剪贴板同步已暂停",
                    );
                } else if let Err(e) = serve(&s, epoch, id, body) {
                    s.refuse(
                        id,
                        farwing_protocol::peer::error_code::INTERNAL,
                        &e.to_string(),
                    );
                    if s.valid(epoch) {
                        s.fail(e.to_string());
                    }
                }
            }
        }
    }
}
fn set_offer(s: Arc<Inner>, epoch: u64, offer: u32, formats: Vec<wire::Format>) -> Result<()> {
    ensure!(
        s.file_offers.is_none() || !formats.is_empty(),
        "独立文件提议为空"
    );
    if formats.is_empty() {
        retire_owner(false);
        let _writing = Writing::new();
        clipboard_retry(|| unsafe { OleSetClipboard(None::<&IDataObject>) })?;
        *lock(&s.error) = None;
        STATE.with(|v| {
            if let Some(v) = v.borrow_mut().as_mut() {
                v.sequence = unsafe { GetClipboardSequenceNumber() };
            }
        });
        return Ok(());
    }
    let platform = s.platform.load(Ordering::Acquire);
    let mut links: Vec<Format> = formats
        .into_iter()
        .filter_map(|f| formats::incoming(f, platform, s.file_allowed()))
        .collect();
    let descriptor = formats::register("FileGroupDescriptorW");
    let is_files = links.iter().any(|f| f.local == descriptor);
    if is_files && s.file_allowed() {
        let contents = formats::register("FileContents");
        if !links.iter().any(|f| f.local == contents) {
            links.push(Format {
                wire: wire::Format {
                    id: contents,
                    name: "FileContents".into(),
                },
                local: contents,
            });
        }
    }
    if links.is_empty() {
        ensure!(s.file_offers.is_none(), "未提供可接收的文件格式");
        return Ok(());
    }
    if s.file_offers.is_some() {
        ensure!(
            is_files
                && s.file_allowed()
                && links
                    .iter()
                    .all(|f| f.local == descriptor || f.local == formats::register("FileContents")),
            "独立会话只接受文件格式"
        );
    } else {
        retire_owner(false);
    }
    let offer = Arc::new(RemoteOffer {
        file_operation: s.file_offers.is_some(),
        failure: Mutex::new(None),
        session: s,
        epoch,
        offer,
        alive: AtomicBool::new(true),
        formats: links,
        listed: AtomicBool::new(false),
        descriptors: Mutex::new(None),
        reading: Mutex::new(()),
        cached: Mutex::new(HashMap::new()),
        dragging: AtomicBool::new(false),
        objects: AtomicU32::new(0),
        streams: AtomicU32::new(0),
        had_object: AtomicBool::new(false),
        async_started: AtomicBool::new(false),
        async_done: AtomicBool::new(false),
        async_result: AtomicI32::new(0),
        bytes_read: AtomicU64::new(0),
    });
    if let Some(sender) = &offer.session.file_offers {
        sender
            .try_send(FileOffer(offer.clone()))
            .map_err(|_| anyhow!("文件提议接收方已关闭或繁忙"))?;
        return Ok(());
    }
    let object = data_object(offer.clone());
    let _writing = Writing::new();
    clipboard_retry(|| unsafe { OleSetClipboard(&object) })?;
    *lock(&offer.session.error) = None;
    STATE.with(|s| {
        if let Some(s) = s.borrow_mut().as_mut() {
            s.owner = Some((object, offer));
            s.sequence = unsafe { GetClipboardSequenceNumber() };
        }
    });
    Ok(())
}
fn local_change() -> Result<()> {
    if WRITING.with(Cell::get) {
        return Ok(());
    }
    if WORKER
        .get()
        .and_then(|w| w.as_ref().ok())
        .is_some_and(|w| unsafe { GetClipboardOwner() }.is_ok_and(|h| h.0 as isize == w.hwnd))
    {
        return Ok(());
    }
    // Only sessions with automatic sync receive local copies; the others
    // copy the host clipboard on demand (`Pull`).
    let sessions = sessions()
        .into_iter()
        .filter(|s| s.file_offers.is_none() && s.auto.load(Ordering::Acquire))
        .collect::<Vec<_>>();
    if sessions.is_empty() {
        return Ok(());
    }
    let (owner, sequence) = STATE.with(|s| {
        let s = s.borrow();
        let s = s.as_ref().unwrap();
        (s.owner.as_ref().map(|(o, _)| o.clone()), s.sequence)
    });
    let now = unsafe { GetClipboardSequenceNumber() };
    if now == sequence {
        return Ok(());
    }
    if owner.as_ref().is_some_and(is_current) {
        return Ok(());
    }
    let object = clipboard_retry(|| unsafe { OleGetClipboard() })?;
    let marker = format_etc(formats::register(REMOTE_MARKER), -1, TYMED_HGLOBAL.0 as u32);
    if unsafe { object.QueryGetData(&marker) } == S_OK {
        return Ok(());
    }
    if excluded(&object) {
        // Password managers and similar sources ask monitors not to copy
        // this content elsewhere; it stays on this device.
        STATE.with(|v| {
            if let Some(v) = v.borrow_mut().as_mut() {
                v.sequence = now;
            }
        });
        tracing::debug!("local clipboard content excluded from remote sync by its source");
        return Ok(());
    }
    let (source, ids) = describe(object)?;
    if unsafe { GetClipboardSequenceNumber() } != now {
        return Ok(());
    }
    retire_owner(false);
    STATE.with(|v| {
        if let Some(v) = v.borrow_mut().as_mut() {
            v.sequence = now;
        }
    });
    for session in sessions {
        let links = formats::outgoing(
            &ids,
            session.platform.load(Ordering::Acquire),
            session.file_allowed(),
        );
        if links.is_empty() && !ids.is_empty() {
            STATE.with(|v| {
                if let Some(v) = v.borrow_mut().as_mut() {
                    v.published.remove(&session.id);
                }
            });
            continue;
        }
        let offer = session.offer(
            session.epoch.load(Ordering::Acquire),
            links.iter().map(|f| f.wire.clone()).collect(),
        )?;
        STATE.with(|v| {
            if let Some(v) = v.borrow_mut().as_mut() {
                v.published
                    .insert(session.id, (source.clone(), links, offer));
            }
        });
        *lock(&session.error) = None;
    }
    Ok(())
}
/// The offerable formats of a clipboard data object: a file list as
/// `FileGroupDescriptorW` + `FileContents`, otherwise every supported format.
fn describe(object: IDataObject) -> Result<(Arc<LocalSource>, Vec<(u32, String)>)> {
    let enumeration = clipboard_retry(|| unsafe { object.EnumFormatEtc(DATADIR_GET.0 as u32) })?;
    let mut ids = Vec::new();
    let mut has_files = false;
    for _ in 0..256 {
        let mut f = [FORMATETC::default()];
        let mut got = 0;
        if unsafe { enumeration.Next(&mut f, Some(&mut got)) } != S_OK || got != 1 {
            break;
        }
        if !f[0].ptd.is_null() {
            unsafe { CoTaskMemFree(Some(f[0].ptd.cast())) };
        }
        let id = f[0].cfFormat as u32;
        let name = formats::name(id);
        if id == 15 || name == "FileGroupDescriptorW" {
            has_files = true;
        }
        if f[0].tymed & (TYMED_HGLOBAL.0 | TYMED_ENHMF.0) as u32 != 0
            && formats::supported(id, &name, false)
            && !ids.iter().any(|(i, _)| *i == id)
        {
            ids.push((id, name));
        }
    }
    if has_files {
        ids = vec![
            (
                formats::register("FileGroupDescriptorW"),
                "FileGroupDescriptorW".into(),
            ),
            (formats::register("FileContents"), "FileContents".into()),
        ];
    }
    let source = Arc::new(LocalSource {
        object: Some(object),
        files: has_files,
        prepared: None,
    });
    Ok((source, ids))
}
/// Answer `Pull`: offer the current system clipboard to this session now,
/// whatever its automatic-sync state. Contents the source excluded from
/// monitoring are refused.
fn pull(s: &Arc<Inner>, epoch: u64, id: u32) -> Result<()> {
    use farwing_protocol::peer::error_code;
    let object = clipboard_retry(|| unsafe { OleGetClipboard() })?;
    if excluded(&object) {
        s.refuse(id, error_code::FORBIDDEN, "被控剪贴板内容不允许同步");
        return Ok(());
    }
    let (source, ids) = describe(object)?;
    let links = formats::outgoing(&ids, s.platform.load(Ordering::Acquire), s.file_allowed());
    let offer = s.next_offer();
    STATE.with(|v| {
        if let Some(v) = v.borrow_mut().as_mut() {
            v.published.insert(s.id, (source, links.clone(), offer));
        }
    });
    tracing::info!(formats = links.len(), "host clipboard pulled by a participant");
    s.respond(
        epoch,
        id,
        Body::Offer(wire::Offer {
            offer,
            formats: links.into_iter().map(|f| f.wire).collect(),
        }),
    )
}
/// Whether the clipboard source opted out of monitoring and cloud sync:
/// `ExcludeClipboardContentFromMonitorProcessing` or `Clipboard Viewer Ignore`
/// present, or `CanUploadToCloudClipboard`/`CanIncludeInClipboardHistory`
/// set to 0.
fn excluded(object: &IDataObject) -> bool {
    let present = |name: &str| {
        let format = format_etc(formats::register(name), -1, TYMED_HGLOBAL.0 as u32);
        let result = unsafe { object.QueryGetData(&format) };
        result == S_OK
    };
    let disallowed = |name: &str| {
        present(name)
            && get_medium(object, formats::register(name), -1, TYMED_HGLOBAL.0 as u32)
                .ok()
                .and_then(|medium| global_bytes(unsafe { medium.0.u.hGlobal }).ok())
                .is_some_and(|bytes| {
                    bytes.len() >= 4 && u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) == 0
                })
    };
    present("ExcludeClipboardContentFromMonitorProcessing")
        || present("Clipboard Viewer Ignore")
        || disallowed("CanUploadToCloudClipboard")
        || disallowed("CanIncludeInClipboardHistory")
}
struct Medium(STGMEDIUM);
impl Drop for Medium {
    fn drop(&mut self) {
        unsafe { ReleaseStgMedium(&mut self.0) }
    }
}
fn format_etc(id: u32, index: i32, tymed: u32) -> FORMATETC {
    FORMATETC {
        cfFormat: id as u16,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: index,
        tymed,
    }
}
fn get_medium(o: &IDataObject, id: u32, index: i32, tymed: u32) -> Result<Medium> {
    Ok(Medium(clipboard_retry(|| unsafe {
        o.GetData(&format_etc(id, index, tymed))
    })?))
}
fn global_bytes(h: HGLOBAL) -> Result<Vec<u8>> {
    unsafe {
        let size = GlobalSize(h);
        ensure!(size <= MAX_DATA, "剪贴板内容超过内存预算");
        if size == 0 {
            return Ok(Vec::new());
        }
        let ptr = GlobalLock(h);
        ensure!(!ptr.is_null(), "无法锁定剪贴板内容");
        let bytes = std::slice::from_raw_parts(ptr.cast::<u8>(), size).to_vec();
        let _ = GlobalUnlock(h);
        Ok(bytes)
    }
}
fn memory_medium(bytes: &[u8]) -> windows::core::Result<STGMEDIUM> {
    unsafe {
        let h = GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, bytes.len().max(1))?;
        let ptr = GlobalLock(h);
        if ptr.is_null() {
            let _ = GlobalFree(Some(h));
            return Err(E_OUTOFMEMORY.into());
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr.cast(), bytes.len());
        let _ = GlobalUnlock(h);
        Ok(STGMEDIUM {
            tymed: TYMED_HGLOBAL.0 as u32,
            u: STGMEDIUM_0 { hGlobal: h },
            pUnkForRelease: ManuallyDrop::new(None),
        })
    }
}
struct LocalSource {
    object: Option<IDataObject>,
    files: bool,
    prepared: Option<Arc<LocalFiles>>,
}
impl LocalSource {
    fn data(&self, format: &Format, platform: i32) -> Result<Vec<u8>> {
        ensure!(!self.files, "文件数据必须通过文件请求获取");
        let kind = if matches!(format.local, 14 | 0x8e) {
            TYMED_ENHMF
        } else {
            TYMED_HGLOBAL
        };
        let medium = get_medium(
            self.object
                .as_ref()
                .ok_or_else(|| anyhow!("文件来源不含文本"))?,
            format.local,
            -1,
            kind.0 as u32,
        )?;
        let data = if kind == TYMED_ENHMF {
            unsafe {
                let h = medium.0.u.hEnhMetaFile;
                let n = GetEnhMetaFileBits(h, None);
                ensure!(n as usize <= MAX_DATA, "图元文件过大");
                let mut data = vec![0; n as usize];
                ensure!(
                    GetEnhMetaFileBits(h, Some(&mut data)) == n,
                    "无法读取图元文件"
                );
                data
            }
        } else {
            global_bytes(unsafe { medium.0.u.hGlobal })?
        };
        formats::convert(data, format, platform, true)
    }
    fn files(&self) -> Result<Arc<LocalFiles>> {
        ensure!(self.files, "本次剪贴板不含文件");
        if let Some(prepared) = &self.prepared {
            return Ok(prepared.clone());
        }
        let object = self
            .object
            .as_ref()
            .ok_or_else(|| anyhow!("文件来源不可用"))?;
        if let Ok(m) = get_medium(object, 15, -1, TYMED_HGLOBAL.0 as u32) {
            let drop = HDROP(unsafe { m.0.u.hGlobal.0 });
            let count = unsafe { DragQueryFileW(drop, u32::MAX, None) };
            ensure!(count as usize <= MAX_FILES, "文件数量过多");
            let mut items = Vec::new();
            for i in 0..count {
                let size = unsafe { DragQueryFileW(drop, i, None) };
                ensure!(size > 0 && size < 32768, "无效的文件路径");
                let mut buf = vec![0u16; size as usize + 1];
                unsafe {
                    DragQueryFileW(drop, i, Some(&mut buf));
                }
                let path = PathBuf::from(String::from_utf16(&buf[..size as usize])?);
                ensure!(
                    std::fs::symlink_metadata(&path)?.file_attributes() & 0x400 == 0,
                    "不传输重解析点或链接"
                );
                let root = dunce::canonicalize(&path)?;
                let name = path
                    .file_name()
                    .ok_or_else(|| anyhow!("不能复制磁盘根目录"))?
                    .to_string_lossy()
                    .into_owned();
                collect_file(&root, &root, &name, &mut items)?;
            }
            return Ok(Arc::new(LocalFiles {
                items,
                object: None,
            }));
        }
        let medium = get_medium(
            object,
            formats::register("FileGroupDescriptorW"),
            -1,
            TYMED_HGLOBAL.0 as u32,
        )?;
        let data = global_bytes(unsafe { medium.0.u.hGlobal })?;
        let mut desc = parse_descriptors(&data)?;
        for (index, desc) in desc.iter_mut().enumerate() {
            let start = 4 + index * 592;
            let flags = u32::from_le_bytes(data[start..start + 4].try_into().unwrap());
            if flags & 0x40 == 0 && desc.attributes & 0x10 == 0 {
                let content = get_medium(
                    object,
                    formats::register("FileContents"),
                    index as i32,
                    (TYMED_ISTREAM.0 | TYMED_HGLOBAL.0) as u32,
                )?;
                desc.size = if content.0.tymed == TYMED_HGLOBAL.0 as u32 {
                    unsafe { GlobalSize(content.0.u.hGlobal) as u64 }
                } else {
                    let stream = unsafe { content.0.u.pstm.as_ref().cloned() }
                        .ok_or_else(|| anyhow!("文件流不可用"))?;
                    let mut stat = STATSTG::default();
                    unsafe {
                        stream.Stat(&mut stat, STATFLAG_NONAME)?;
                    }
                    stat.cbSize
                };
            }
        }
        Ok(Arc::new(LocalFiles {
            items: desc
                .into_iter()
                .map(|desc| LocalFile {
                    desc,
                    path: None,
                    root: None,
                })
                .collect(),
            object: Some(object.clone()),
        }))
    }
}
struct LocalFile {
    desc: FileItem,
    path: Option<PathBuf>,
    root: Option<PathBuf>,
}
pub(super) struct PreparedFiles {
    items: Vec<LocalFile>,
    summary: FileSummary,
}
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct FileSummary {
    pub count: u32,
    pub bytes: u64,
    pub first_name: String,
}
impl PreparedFiles {
    pub fn summary(&self) -> FileSummary {
        self.summary.clone()
    }
}
pub(super) fn prepare_files(
    paths: Vec<PathBuf>,
    allowed: impl Fn() -> bool,
) -> Result<PreparedFiles> {
    ensure!(
        !paths.is_empty() && paths.len() <= MAX_FILES,
        "拖放文件数量无效"
    );
    let mut items = Vec::new();
    for path in paths {
        ensure!(allowed(), "文件准备已取消");
        ensure!(
            std::fs::symlink_metadata(&path)?.file_attributes() & 0x400 == 0,
            "不传输重解析点或链接"
        );
        let root = dunce::canonicalize(&path)?;
        let name = path
            .file_name()
            .ok_or_else(|| anyhow!("不能拖放磁盘根目录"))?
            .to_string_lossy();
        collect_file_guarded(&root, &root, &name, &mut items, &allowed)?;
    }
    let summary = file_summary(items.iter().map(|v| &v.desc))?;
    Ok(PreparedFiles { items, summary })
}
fn file_summary<'a>(items: impl Iterator<Item = &'a FileItem>) -> Result<FileSummary> {
    let mut summary = FileSummary {
        count: 0,
        bytes: 0,
        first_name: String::new(),
    };
    for item in items {
        if summary.first_name.is_empty() {
            summary.first_name = item.path.clone();
        }
        ensure!(safe_name(&item.path), "文件名称不安全");
        summary.count += 1;
        summary.bytes = summary
            .bytes
            .checked_add(item.size)
            .ok_or_else(|| anyhow!("文件总大小溢出"))?;
    }
    ensure!(
        summary.count > 0 && summary.count as usize <= MAX_FILES,
        "文件清单为空或过大"
    );
    Ok(summary)
}
struct LocalFiles {
    items: Vec<LocalFile>,
    object: Option<IDataObject>,
}
pub(super) fn safe_name(s: &str) -> bool {
    !s.is_empty()
        && s.encode_utf16().count() < 260
        && !s.contains(['\0', ':'])
        && !s.starts_with(['\\', '/'])
        && s.split(['\\', '/']).all(|p| {
            !p.is_empty()
                && p != "."
                && p != ".."
                && !p.ends_with(['.', ' '])
                && !p.chars().any(|c| c < ' ' || "<>\"|?*".contains(c))
                && !matches!(
                    p.split('.')
                        .next()
                        .unwrap_or("")
                        .to_ascii_uppercase()
                        .as_str(),
                    "CON"
                        | "PRN"
                        | "AUX"
                        | "NUL"
                        | "COM1"
                        | "COM2"
                        | "COM3"
                        | "COM4"
                        | "COM5"
                        | "COM6"
                        | "COM7"
                        | "COM8"
                        | "COM9"
                        | "LPT1"
                        | "LPT2"
                        | "LPT3"
                        | "LPT4"
                        | "LPT5"
                        | "LPT6"
                        | "LPT7"
                        | "LPT8"
                        | "LPT9"
                )
        })
}
fn collect_file(path: &Path, root: &Path, name: &str, items: &mut Vec<LocalFile>) -> Result<()> {
    collect_file_guarded(path, root, name, items, &|| true)
}
fn collect_file_guarded(
    path: &Path,
    root: &Path,
    name: &str,
    items: &mut Vec<LocalFile>,
    allowed: &dyn Fn() -> bool,
) -> Result<()> {
    ensure!(allowed(), "文件准备已取消");
    ensure!(
        items.len() < MAX_FILES && safe_name(name),
        "文件数量或名称不支持"
    );
    let meta = std::fs::symlink_metadata(path)?;
    ensure!(meta.file_attributes() & 0x400 == 0, "不传输重解析点或链接");
    let canonical = dunce::canonicalize(path)?;
    ensure!(canonical.starts_with(root), "文件超出复制范围");
    items.push(LocalFile {
        desc: FileItem {
            path: name.replace('/', "\\"),
            attributes: meta.file_attributes(),
            mtime: meta.last_write_time(),
            size: if meta.is_dir() { 0 } else { meta.file_size() },
        },
        path: Some(canonical.clone()),
        root: Some(root.into()),
    });
    if meta.is_dir() {
        for entry in std::fs::read_dir(canonical)? {
            let e = entry?;
            collect_file_guarded(
                &e.path(),
                root,
                &format!("{}\\{}", name, e.file_name().to_string_lossy()),
                items,
                allowed,
            )?;
        }
    }
    Ok(())
}
impl LocalFiles {
    fn read(&self, r: &wire::ReadFile) -> Result<Vec<u8>> {
        let f = self
            .items
            .get(r.index as usize)
            .ok_or_else(|| anyhow!("无效的文件索引"))?;
        ensure!(r.length as usize <= FILE_BLOCK, "文件读取请求过大");
        ensure!(f.desc.attributes & 0x10 == 0, "目录没有文件内容");
        let n = (r.length as u64).min(f.desc.size.saturating_sub(r.offset)) as usize;
        if let Some(path) = &f.path {
            ensure!(
                dunce::canonicalize(path)?.starts_with(f.root.as_ref().unwrap()),
                "文件超出原始复制范围"
            );
            let mut file = std::fs::OpenOptions::new()
                .read(true)
                .share_mode(7)
                .custom_flags(0x00200000)
                .open(path)?;
            ensure!(
                file.metadata()?.file_attributes() & 0x400 == 0,
                "文件已变为链接"
            );
            // Validate the opened handle, not just the path before opening (junction races).
            let mut final_name = vec![0u16; 32768];
            let size = unsafe {
                windows::Win32::Storage::FileSystem::GetFinalPathNameByHandleW(
                    HANDLE(file.as_raw_handle()),
                    &mut final_name,
                    windows::Win32::Storage::FileSystem::FILE_NAME_NORMALIZED,
                )
            } as usize;
            ensure!(
                size > 0 && size < final_name.len(),
                "无法确认复制文件的实际路径"
            );
            let final_path =
                dunce::simplified(Path::new(&String::from_utf16(&final_name[..size])?))
                    .to_path_buf();
            ensure!(
                final_path.starts_with(f.root.as_ref().unwrap()),
                "打开的文件超出原始复制范围"
            );
            file.seek(SeekFrom::Start(r.offset))?;
            let mut data = vec![0; n];
            let mut done = 0;
            while done < n {
                let got = file.read(&mut data[done..])?;
                if got == 0 {
                    break;
                }
                done += got;
            }
            data.truncate(done);
            return Ok(data);
        }
        let source = self
            .object
            .as_ref()
            .ok_or_else(|| anyhow!("文件来源已关闭"))?;
        let medium = get_medium(
            source,
            formats::register("FileContents"),
            r.index as i32,
            (TYMED_ISTREAM.0 | TYMED_HGLOBAL.0) as u32,
        )?;
        if medium.0.tymed == TYMED_HGLOBAL.0 as u32 {
            let bytes = global_bytes(unsafe { medium.0.u.hGlobal })?;
            let start = usize::try_from(r.offset)?.min(bytes.len());
            return Ok(bytes[start..(start + n).min(bytes.len())].to_vec());
        }
        let stream =
            unsafe { medium.0.u.pstm.as_ref().cloned() }.ok_or_else(|| anyhow!("没有文件流"))?;
        unsafe {
            stream.Seek(i64::try_from(r.offset)?, STREAM_SEEK_SET, None)?;
            let mut bytes = vec![0; n];
            let mut got = 0;
            stream
                .Read(bytes.as_mut_ptr().cast(), n as u32, Some(&mut got))
                .ok()?;
            bytes.truncate(got as usize);
            Ok(bytes)
        }
    }
}
/// Answer a peer request from our current offer (on the clipboard STA).
fn serve(s: &Arc<Inner>, epoch: u64, id: u32, body: Body) -> Result<()> {
    use farwing_protocol::peer::error_code;
    let published = STATE.with(|v| {
        v.borrow()
            .as_ref()
            .and_then(|v| v.published.get(&s.id).cloned())
    });
    match body {
        Body::Fetch(fetch) => {
            let result = (|| -> Result<Vec<u8>> {
                let (source, formats, offer) =
                    published.ok_or_else(|| anyhow!("原剪贴板已失效"))?;
                ensure!(offer == fetch.offer, "原剪贴板已失效");
                let wanted = fetch.format.ok_or_else(|| anyhow!("缺少剪贴板格式"))?;
                let format = formats
                    .iter()
                    .find(|f| {
                        f.wire.id == wanted.id
                            && (wanted.name.is_empty() || wanted.name == f.wire.name)
                    })
                    .ok_or_else(|| anyhow!("未发布该剪贴板格式"))?;
                source.data(format, s.platform.load(Ordering::Acquire))
            })();
            match result {
                Ok(data) if !data.is_empty() => s.respond_data(epoch, id, fetch.stream, data),
                Ok(_) => {
                    s.refuse(id, error_code::NOT_FOUND, "剪贴板内容为空");
                    Ok(())
                }
                Err(error) => {
                    tracing::debug!(%error, "clipboard fetch rejected");
                    s.refuse(id, error_code::NOT_FOUND, &error.to_string());
                    Ok(())
                }
            }
        }
        Body::ListFiles(list) => {
            let result = (|| -> Result<Arc<LocalFiles>> {
                ensure!(s.file_allowed(), "文件剪贴板已关闭");
                let existing = STATE.with(|v| {
                    v.borrow()
                        .as_ref()
                        .and_then(|v| v.tasks.get(&(s.id, list.offer)).cloned())
                });
                if let Some(files) = existing {
                    return Ok(files);
                }
                let (source, _, offer) =
                    published.ok_or_else(|| anyhow!("原文件剪贴板已失效"))?;
                ensure!(offer == list.offer, "原文件剪贴板已失效");
                let files = source.files()?;
                ensure!(!files.items.is_empty(), "文件列表为空");
                let inserted = STATE.with(|v| {
                    let mut v = v.borrow_mut();
                    let v = v.as_mut().unwrap();
                    if v.tasks.len() >= 32 {
                        return false;
                    }
                    v.tasks.insert((s.id, list.offer), files.clone());
                    true
                });
                ensure!(inserted, "正在读取的剪贴板文件过多");
                Ok(files)
            })();
            match result {
                Ok(files) => s.respond(
                    epoch,
                    id,
                    Body::Files(wire::Files {
                        items: files.items.iter().map(|f| f.desc.clone()).collect(),
                    }),
                ),
                Err(error) => {
                    tracing::debug!(%error, "clipboard file list rejected");
                    s.refuse(id, error_code::NOT_FOUND, &error.to_string());
                    Ok(())
                }
            }
        }
        Body::ReadFile(read) => {
            let files = STATE.with(|v| {
                v.borrow()
                    .as_ref()
                    .and_then(|v| v.tasks.get(&(s.id, read.offer)).cloned())
            });
            let result = if s.file_allowed() {
                files
                    .ok_or_else(|| anyhow!("文件任务已失效"))
                    .and_then(|f| f.read(&read))
            } else {
                Err(anyhow!("文件剪贴板已关闭"))
            };
            match result {
                Ok(data) => s.respond_data(epoch, id, read.stream, data),
                Err(error) => {
                    tracing::debug!(%error, "clipboard file read rejected");
                    s.refuse(id, error_code::NOT_FOUND, &error.to_string());
                    Ok(())
                }
            }
        }
        Body::Pull(_) => pull(s, epoch, id),
        _ => {
            s.refuse(id, error_code::BAD_REQUEST, "无效的剪贴板请求");
            Ok(())
        }
    }
}
pub(super) fn parse_descriptors(bytes: &[u8]) -> Result<Vec<FileItem>> {
    ensure!(bytes.len() >= 4, "无效文件描述");
    let count = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;
    ensure!(
        count <= MAX_FILES && 4 + count * 592 <= bytes.len(),
        "无效文件描述长度"
    );
    let mut out = Vec::with_capacity(count);
    for b in bytes[4..4 + count * 592].chunks_exact(592) {
        let u32at = |i| u32::from_le_bytes(b[i..i + 4].try_into().unwrap());
        let name: Vec<u16> = b[72..592]
            .chunks_exact(2)
            .map(|p| u16::from_le_bytes([p[0], p[1]]))
            .take_while(|c| *c != 0)
            .collect();
        let name = String::from_utf16(&name)?;
        ensure!(safe_name(&name), "不支持的文件名称");
        out.push(FileItem {
            path: name,
            attributes: u32at(36),
            mtime: u32at(56) as u64 | ((u32at(60) as u64) << 32),
            size: u32at(68) as u64 | ((u32at(64) as u64) << 32),
        });
    }
    Ok(out)
}
fn descriptor_bytes(items: &[FileItem]) -> Result<Vec<u8>> {
    ensure!(items.len() <= MAX_FILES, "文件数量过多");
    let mut bytes = vec![0; 4 + items.len() * 592];
    bytes[..4].copy_from_slice(&(items.len() as u32).to_le_bytes());
    for (b, f) in bytes[4..].chunks_exact_mut(592).zip(items) {
        ensure!(safe_name(&f.path), "不安全的远端文件名称");
        b[..4].copy_from_slice(&0x4064u32.to_le_bytes());
        b[36..40].copy_from_slice(&f.attributes.to_le_bytes());
        b[56..64].copy_from_slice(&f.mtime.to_le_bytes());
        b[64..68].copy_from_slice(&((f.size >> 32) as u32).to_le_bytes());
        b[68..72].copy_from_slice(&(f.size as u32).to_le_bytes());
        for (out, c) in b[72..]
            .chunks_exact_mut(2)
            .zip(f.path.replace('/', "\\").encode_utf16())
        {
            out.copy_from_slice(&c.to_le_bytes());
        }
    }
    Ok(bytes)
}
pub(super) struct RemoteOffer {
    file_operation: bool,
    failure: Mutex<Option<String>>,
    pub(super) session: Arc<Inner>,
    pub(super) epoch: u64,
    /// The peer's offer number.
    offer: u32,
    alive: AtomicBool,
    formats: Vec<Format>,
    /// The file list was requested; the peer keeps it until `Release`.
    listed: AtomicBool,
    descriptors: Mutex<Option<Vec<FileItem>>>,
    reading: Mutex<()>,
    cached: Mutex<HashMap<u32, Vec<u8>>>,
    dragging: AtomicBool,
    objects: AtomicU32,
    streams: AtomicU32,
    had_object: AtomicBool,
    async_started: AtomicBool,
    async_done: AtomicBool,
    async_result: AtomicI32,
    bytes_read: AtomicU64,
}
#[derive(Clone)]
pub(crate) struct FileOffer(pub(super) Arc<RemoteOffer>);
pub(crate) struct FileOfferStatus {
    pub bytes_read: u64,
    pub completed: Option<Result<(), String>>,
}
impl FileOffer {
    pub fn prepare(&self) -> Result<FileSummary> {
        file_summary(self.0.list()?.iter())
    }
    pub fn object(&self) -> Result<IDataObject> {
        self.0.valid()?;
        Ok(data_object(self.0.clone()))
    }
    pub fn dragging(&self, value: bool) {
        self.0.dragging.store(value, Ordering::Release);
    }
    pub fn cancel(&self) {
        self.0.cancel();
        self.0.session.cancel_pending();
    }
    pub fn status(&self) -> FileOfferStatus {
        let error = if self.0.file_operation {
            lock(&self.0.failure).clone()
        } else {
            lock(&self.0.session.error).clone()
        };
        let completed = if let Some(error) = error {
            Some(Err(error))
        } else if self.0.async_done.load(Ordering::Acquire) {
            let result = HRESULT(self.0.async_result.load(Ordering::Acquire));
            Some(result.ok().map_err(|e| e.to_string()))
        } else if !self.0.async_started.load(Ordering::Acquire)
            && self.0.had_object.load(Ordering::Acquire)
            && self.0.objects.load(Ordering::Acquire) == 0
            && self.0.streams.load(Ordering::Acquire) == 0
        {
            Some(Ok(()))
        } else {
            None
        };
        FileOfferStatus {
            bytes_read: self.0.bytes_read.load(Ordering::Acquire),
            completed,
        }
    }
}
fn data_object(offer: Arc<RemoteOffer>) -> IDataObject {
    offer.objects.fetch_add(1, Ordering::AcqRel);
    offer.had_object.store(true, Ordering::Release);
    RemoteData {
        offer,
        async_mode: AtomicBool::new(true),
        in_operation: AtomicBool::new(false),
        shell: Mutex::new(HashMap::new()),
    }
    .into()
}
impl RemoteOffer {
    fn cancel(&self) {
        self.alive.store(false, Ordering::Release);
        self.release();
    }
    /// Let the peer drop the file list it keeps for this offer.
    fn release(&self) {
        if self.listed.swap(false, Ordering::AcqRel) {
            self.session.release(self.offer);
        }
    }
    fn valid(&self) -> Result<()> {
        ensure!(
            self.alive.load(Ordering::Acquire) && self.session.valid(self.epoch),
            "剪贴板来源已失效"
        );
        Ok(())
    }
    fn list(&self) -> Result<Vec<FileItem>> {
        self.valid()?;
        let existing = lock(&self.descriptors).clone();
        if let Some(v) = existing {
            return Ok(v);
        }
        let _busy = self.enter_read()?;
        self.listed.store(true, Ordering::Release);
        let list = self.session.descriptors(self.epoch, self.offer)?;
        self.valid()?;
        *lock(&self.descriptors) = Some(list.clone());
        Ok(list)
    }
    fn enter_read(&self) -> Result<std::sync::MutexGuard<'_, ()>> {
        let guard = lock(&self.reading);
        self.valid()?;
        Ok(guard)
    }
}
impl Drop for RemoteOffer {
    fn drop(&mut self) {
        self.cancel();
    }
}
fn com_error(e: anyhow::Error) -> windows::core::Error {
    windows::core::Error::new(E_FAIL, e.to_string())
}
enum ShellMedium {
    Memory(Vec<u8>),
    Stream(windows::core::AgileReference<IStream>, usize),
}
impl ShellMedium {
    fn kind(&self) -> TYMED {
        match self {
            Self::Memory(_) => TYMED_HGLOBAL,
            Self::Stream(_, _) => TYMED_ISTREAM,
        }
    }
    fn size(&self) -> usize {
        match self {
            Self::Memory(v) => v.len(),
            Self::Stream(_, n) => *n,
        }
    }
}
#[implement(IDataObject, IDataObjectAsyncCapability)]
struct RemoteData {
    offer: Arc<RemoteOffer>,
    async_mode: AtomicBool,
    in_operation: AtomicBool,
    shell: Mutex<HashMap<u32, Arc<ShellMedium>>>,
}
impl Drop for RemoteData {
    fn drop(&mut self) {
        self.offer.objects.fetch_sub(1, Ordering::AcqRel);
    }
}
impl IDataObjectAsyncCapability_Impl for RemoteData_Impl {
    fn SetAsyncMode(&self, value: BOOL) -> windows::core::Result<()> {
        self.async_mode.store(value.as_bool(), Ordering::Release);
        Ok(())
    }
    fn GetAsyncMode(&self) -> windows::core::Result<BOOL> {
        Ok(self.async_mode.load(Ordering::Acquire).into())
    }
    fn StartOperation(&self, _: Ref<IBindCtx>) -> windows::core::Result<()> {
        self.offer.valid().map_err(com_error)?;
        self.in_operation.store(true, Ordering::Release);
        self.offer.async_started.store(true, Ordering::Release);
        Ok(())
    }
    fn InOperation(&self) -> windows::core::Result<BOOL> {
        Ok(self.in_operation.load(Ordering::Acquire).into())
    }
    fn EndOperation(&self, result: HRESULT, _: Ref<IBindCtx>, _: u32) -> windows::core::Result<()> {
        self.in_operation.store(false, Ordering::Release);
        self.offer.async_result.store(result.0, Ordering::Release);
        self.offer.async_done.store(true, Ordering::Release);
        if result.is_err() {
            self.offer.session.cancel_pending();
            *lock(&self.offer.descriptors) = None;
            self.offer.release();
        }
        Ok(())
    }
}
impl IDataObject_Impl for RemoteData_Impl {
    fn GetData(&self, p: *const FORMATETC) -> windows::core::Result<STGMEDIUM> {
        self.QueryGetData(p).ok()?;
        let f = unsafe { &*p };
        self.offer.valid().map_err(com_error)?;
        let id = f.cfFormat as u32;
        let private = lock(&self.shell).get(&id).cloned();
        if let Some(private) = private {
            return match private.as_ref() {
                ShellMedium::Memory(bytes) => memory_medium(&bytes),
                ShellMedium::Stream(reference, _) => {
                    // Resolve in the caller's apartment and give every consumer
                    // its own cursor. Never hold our mutex across COM calls.
                    let stream = unsafe { reference.resolve()?.Clone()? };
                    unsafe {
                        stream.Seek(0, STREAM_SEEK_SET, None)?;
                    }
                    Ok(STGMEDIUM {
                        tymed: TYMED_ISTREAM.0 as u32,
                        u: STGMEDIUM_0 {
                            pstm: ManuallyDrop::new(Some(stream)),
                        },
                        pUnkForRelease: ManuallyDrop::new(None),
                    })
                }
            };
        }
        if id == formats::register(REMOTE_MARKER) {
            return memory_medium(&[1]);
        }
        if self.offer.file_operation {
            if id == formats::register("InShellDragLoop") {
                return memory_medium(
                    &u32::from(self.offer.dragging.load(Ordering::Acquire)).to_le_bytes(),
                );
            }
            if id == formats::register("Preferred DropEffect") {
                return memory_medium(&1u32.to_le_bytes());
            }
        }
        if id == formats::register("FileContents") {
            let list = self.offer.list().map_err(com_error)?;
            let d = list
                .get(f.lindex as usize)
                .ok_or_else(|| windows::core::Error::from(E_INVALIDARG))?;
            self.offer.streams.fetch_add(1, Ordering::AcqRel);
            let stream: IStream = RemoteStream {
                offer: self.offer.clone(),
                index: f.lindex as u32,
                size: d.size,
                position: Mutex::new(0),
                cache: Mutex::new(None),
            }
            .into();
            return Ok(STGMEDIUM {
                tymed: TYMED_ISTREAM.0 as u32,
                u: STGMEDIUM_0 {
                    pstm: ManuallyDrop::new(Some(stream)),
                },
                pUnkForRelease: ManuallyDrop::new(None),
            });
        }
        let data = if id == formats::register("FileGroupDescriptorW") {
            descriptor_bytes(&self.offer.list().map_err(com_error)?).map_err(com_error)?
        } else {
            let cache = lock(&self.offer.cached).get(&id).cloned();
            if let Some(v) = cache {
                v
            } else {
                let f = self
                    .offer
                    .formats
                    .iter()
                    .find(|f| f.local == id)
                    .ok_or_else(|| windows::core::Error::from(DV_E_FORMATETC))?;
                let data = self
                    .offer
                    .session
                    .data(self.offer.epoch, self.offer.offer, &f.wire)
                    .and_then(|d| {
                        formats::convert(
                            d,
                            f,
                            self.offer.session.platform.load(Ordering::Acquire),
                            false,
                        )
                    })
                    .map_err(com_error)?;
                self.offer.valid().map_err(com_error)?;
                {
                    let mut cache = lock(&self.offer.cached);
                    if cache.values().map(Vec::len).sum::<usize>() + data.len() <= MAX_DATA {
                        cache.insert(id, data.clone());
                    }
                }
                data
            }
        };
        if matches!(id, 14 | 0x8e) {
            let metafile = unsafe { SetEnhMetaFileBits(&data) };
            if metafile.is_invalid() {
                return Err(windows::core::Error::from_thread());
            }
            return Ok(STGMEDIUM {
                tymed: TYMED_ENHMF.0 as u32,
                u: STGMEDIUM_0 {
                    hEnhMetaFile: metafile,
                },
                pUnkForRelease: ManuallyDrop::new(None),
            });
        }
        memory_medium(&data)
    }
    fn QueryGetData(&self, p: *const FORMATETC) -> HRESULT {
        if p.is_null() {
            return E_POINTER;
        }
        let f = unsafe { &*p };
        if self.offer.valid().is_err() {
            return E_FAIL;
        }
        if f.dwAspect != DVASPECT_CONTENT.0 {
            return DV_E_DVASPECT;
        }
        let id = f.cfFormat as u32;
        let private = self.offer.file_operation
            && (id == formats::register("InShellDragLoop")
                || id == formats::register("Preferred DropEffect")
                || lock(&self.shell).contains_key(&id));
        if id != formats::register(REMOTE_MARKER)
            && !self.offer.formats.iter().any(|v| v.local == id)
            && !private
        {
            return DV_E_FORMATETC;
        }
        let private_kind = lock(&self.shell).get(&id).map(|v| v.kind());
        let kind = if let Some(kind) = private_kind {
            kind
        } else if id == formats::register("FileContents") {
            if f.lindex < 0 {
                return DV_E_LINDEX;
            }
            TYMED_ISTREAM
        } else if matches!(id, 14 | 0x8e) {
            TYMED_ENHMF
        } else {
            TYMED_HGLOBAL
        };
        if f.tymed & kind.0 as u32 == 0 {
            return DV_E_TYMED;
        }
        S_OK
    }
    fn GetDataHere(&self, _: *const FORMATETC, _: *mut STGMEDIUM) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn GetCanonicalFormatEtc(&self, _: *const FORMATETC, out: *mut FORMATETC) -> HRESULT {
        if !out.is_null() {
            unsafe {
                (*out).ptd = std::ptr::null_mut();
            }
        }
        DATA_S_SAMEFORMATETC
    }
    fn SetData(
        &self,
        format: *const FORMATETC,
        medium: *const STGMEDIUM,
        release: BOOL,
    ) -> windows::core::Result<()> {
        if !self.offer.file_operation {
            return Err(E_NOTIMPL.into());
        }
        if format.is_null() || medium.is_null() {
            return Err(E_POINTER.into());
        }
        let (format, medium) = unsafe { (&*format, &*medium) };

        // Shell's drag-image helper uses registered private formats through
        // SetData/GetData. Never let them replace the read-only file provider.
        if format.cfFormat < 0xc000
            || format.dwAspect != DVASPECT_CONTENT.0
            || format.lindex != -1
            || !format.ptd.is_null()
            || format.cfFormat as u32 == formats::register(REMOTE_MARKER)
            || self
                .offer
                .formats
                .iter()
                .any(|f| f.local == format.cfFormat as u32)
        {
            return Err(E_NOTIMPL.into());
        }
        if format.tymed & medium.tymed == 0 {
            return Err(DV_E_TYMED.into());
        }
        let data = if medium.tymed == TYMED_HGLOBAL.0 as u32 {
            let size = unsafe { GlobalSize(medium.u.hGlobal) };
            if !(1..=1024 * 1024).contains(&size) {
                return Err(E_INVALIDARG.into());
            }
            ShellMedium::Memory(global_bytes(unsafe { medium.u.hGlobal }).map_err(com_error)?)
        } else if medium.tymed == TYMED_ISTREAM.0 as u32 {
            // The Shell helper supplies an owning IStream. Keep an agile COM
            // reference, not an apartment-bound pointer in our thread-safe object.
            if medium.pUnkForRelease.is_some() {
                return Err(E_NOTIMPL.into());
            }
            let stream = unsafe { medium.u.pstm.as_ref() }
                .ok_or_else(|| windows::core::Error::from(E_POINTER))?;
            let mut stat = STATSTG::default();
            unsafe {
                stream.Stat(&mut stat, STATFLAG_NONAME)?;
            }
            if stat.cbSize > 1024 * 1024 {
                return Err(E_INVALIDARG.into());
            }
            ShellMedium::Stream(
                windows::core::AgileReference::new(stream)?,
                stat.cbSize as usize,
            )
        } else {
            return Err(DV_E_TYMED.into());
        };
        let retired = {
            let mut cache = lock(&self.shell);
            let old = cache.get(&(format.cfFormat as u32)).map_or(0, |v| v.size());
            if (!cache.contains_key(&(format.cfFormat as u32)) && cache.len() >= 32)
                || cache.values().map(|v| v.size()).sum::<usize>() - old + data.size()
                    > 4 * 1024 * 1024
            {
                return Err(E_OUTOFMEMORY.into());
            }
            cache.insert(format.cfFormat as u32, Arc::new(data))
        };
        drop(retired); // COM references may release outside our application mutex.
        if release.as_bool() {
            unsafe {
                let mut owned = std::ptr::read(medium);
                ReleaseStgMedium(&mut owned);
            }
        }
        Ok(())
    }
    fn EnumFormatEtc(&self, dir: u32) -> windows::core::Result<IEnumFORMATETC> {
        if dir != DATADIR_GET.0 as u32 {
            return Err(E_NOTIMPL.into());
        }
        let mut formats: Vec<_> = self
            .offer
            .formats
            .iter()
            .map(|f| {
                format_etc(
                    f.local,
                    -1,
                    if f.local == formats::register("FileContents") {
                        TYMED_ISTREAM.0
                    } else if matches!(f.local, 14 | 0x8e) {
                        TYMED_ENHMF.0
                    } else {
                        TYMED_HGLOBAL.0
                    } as u32,
                )
            })
            .collect();
        formats.push(format_etc(
            formats::register(REMOTE_MARKER),
            -1,
            TYMED_HGLOBAL.0 as u32,
        ));
        if self.offer.file_operation {
            for name in ["InShellDragLoop", "Preferred DropEffect"] {
                formats.push(format_etc(
                    formats::register(name),
                    -1,
                    TYMED_HGLOBAL.0 as u32,
                ));
            }
            for (id, medium) in lock(&self.shell).iter() {
                if !formats.iter().any(|f| f.cfFormat as u32 == *id) {
                    formats.push(format_etc(*id, -1, medium.kind().0 as u32));
                }
            }
        }
        unsafe { SHCreateStdEnumFmtEtc(&formats) }
    }
    fn DAdvise(
        &self,
        _: *const FORMATETC,
        _: u32,
        _: Ref<IAdviseSink>,
    ) -> windows::core::Result<u32> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }
    fn DUnadvise(&self, _: u32) -> windows::core::Result<()> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }
    fn EnumDAdvise(&self) -> windows::core::Result<IEnumSTATDATA> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }
}
#[implement(IStream)]
struct RemoteStream {
    offer: Arc<RemoteOffer>,
    index: u32,
    size: u64,
    position: Mutex<u64>,
    cache: Mutex<Option<ReadCache>>,
}
static READ_CACHE_BYTES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
struct ReadCache {
    offset: u64,
    bytes: Vec<u8>,
}
impl ReadCache {
    fn reserve(offset: u64) -> Option<Self> {
        READ_CACHE_BYTES
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                n.checked_add(FILE_BLOCK).filter(|n| *n <= 16 * 1024 * 1024)
            })
            .ok()?;
        Some(Self {
            offset,
            bytes: Vec::new(),
        })
    }
    fn contains(&self, offset: u64) -> bool {
        offset >= self.offset && offset - self.offset < self.bytes.len() as u64
    }
}
impl Drop for ReadCache {
    fn drop(&mut self) {
        READ_CACHE_BYTES.fetch_sub(FILE_BLOCK, Ordering::AcqRel);
    }
}
impl Drop for RemoteStream {
    fn drop(&mut self) {
        self.offer.streams.fetch_sub(1, Ordering::AcqRel);
    }
}
impl ISequentialStream_Impl for RemoteStream_Impl {
    fn Read(&self, p: *mut std::ffi::c_void, length: u32, read: *mut u32) -> HRESULT {
        if !read.is_null() {
            unsafe {
                *read = 0;
            }
        }
        if p.is_null() && length != 0 {
            return E_POINTER;
        }
        let result = (|| -> Result<u32> {
            self.offer.valid()?;
            // Serialize before taking the cursor: another Read may have moved
            // it while this caller waited for the task's current RPC to finish.
            let _busy = self.offer.enter_read()?;
            let pos = *lock(&self.position);
            if pos >= self.size || length == 0 {
                return Ok(0);
            }
            // GetData(FileContents) already established this stream's descriptor.
            // Do not clone the entire directory manifest for every small read.
            let wanted = (length as u64).min(self.size - pos);
            // Shell commonly asks for small reads. Reuse one ordinary protocol
            // block locally, preserving task-only response correlation.
            let mut cache = lock(&self.cache).take();
            let mut done = 0u64;
            while done < wanted {
                let mut uncached = Vec::new();
                let offset = pos + done;
                if !cache.as_ref().is_some_and(|c| c.contains(offset)) {
                    drop(cache.take());
                    cache = ReadCache::reserve(offset);
                    let length = (if cache.is_some() {
                        FILE_BLOCK as u64
                    } else {
                        wanted - done
                    })
                    .min(FILE_BLOCK as u64)
                    .min(self.size - offset) as usize;
                    let data = self.offer.session.read_file(
                        self.offer.epoch,
                        self.offer.offer,
                        self.index,
                        offset,
                        length,
                    )?;
                    self.offer.valid()?;
                    if data.is_empty() {
                        break;
                    }
                    if let Some(c) = cache.as_mut() {
                        c.bytes = data;
                    } else {
                        uncached = data;
                    }
                }
                let data = if let Some(cached) = cache.as_ref() {
                    let start = (offset - cached.offset) as usize;
                    &cached.bytes
                        [start..start + ((wanted - done) as usize).min(cached.bytes.len() - start)]
                } else {
                    &uncached[..]
                };
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        data.as_ptr(),
                        p.cast::<u8>().add(done as usize),
                        data.len(),
                    );
                }
                done += data.len() as u64;
                self.offer
                    .bytes_read
                    .fetch_add(data.len() as u64, Ordering::Relaxed);
                *lock(&self.position) = pos + done;
                if !read.is_null() {
                    unsafe {
                        *read = done as u32;
                    }
                }
            }
            *lock(&self.cache) = cache;
            Ok(done as u32)
        })();
        match result {
            Ok(done) => {
                if !read.is_null() {
                    unsafe {
                        *read = done;
                    }
                }
                if done == length { S_OK } else { S_FALSE }
            }
            Err(e) => {
                *lock(&self.offer.failure) = Some(e.to_string());
                self.offer.session.fail(e.to_string());
                E_FAIL
            }
        }
    }
    fn Write(&self, _: *const std::ffi::c_void, _: u32, w: *mut u32) -> HRESULT {
        if !w.is_null() {
            unsafe {
                *w = 0;
            }
        }
        STG_E_ACCESSDENIED
    }
}
impl IStream_Impl for RemoteStream_Impl {
    fn Seek(&self, delta: i64, origin: STREAM_SEEK, out: *mut u64) -> windows::core::Result<()> {
        let mut pos = lock(&self.position);
        let base = match origin {
            STREAM_SEEK_SET => 0,
            STREAM_SEEK_CUR => *pos,
            STREAM_SEEK_END => self.size,
            _ => return Err(E_INVALIDARG.into()),
        };
        let new = base
            .checked_add_signed(delta)
            .ok_or_else(|| windows::core::Error::from(E_INVALIDARG))?;
        *pos = new;
        // Keep an adjacent seek inside the cached block, but do not retain an
        // unrelated block when a consumer jumps to another part of the file.
        let mut cache = lock(&self.cache);
        if !cache.as_ref().is_some_and(|c| c.contains(new)) {
            *cache = None;
        }
        if !out.is_null() {
            unsafe {
                *out = new;
            }
        }
        Ok(())
    }
    fn SetSize(&self, _: u64) -> windows::core::Result<()> {
        Err(STG_E_ACCESSDENIED.into())
    }
    fn CopyTo(
        &self,
        target: Ref<IStream>,
        count: u64,
        read: *mut u64,
        written: *mut u64,
    ) -> windows::core::Result<()> {
        let target = target.ok()?;
        let mut total = 0;
        let mut bytes = vec![0; FILE_BLOCK];
        while total < count {
            let n = (count - total).min(FILE_BLOCK as u64) as u32;
            let mut got = 0;
            self.Read(bytes.as_mut_ptr().cast(), n, &mut got).ok()?;
            if got == 0 {
                break;
            }
            let mut sent = 0;
            unsafe {
                target
                    .Write(bytes.as_ptr().cast(), got, Some(&mut sent))
                    .ok()?;
            }
            if sent != got {
                return Err(STG_E_WRITEFAULT.into());
            }
            total += got as u64;
        }
        if !read.is_null() {
            unsafe {
                *read = total;
            }
        }
        if !written.is_null() {
            unsafe {
                *written = total;
            }
        }
        Ok(())
    }
    fn Commit(&self, _: &STGC) -> windows::core::Result<()> {
        Ok(())
    }
    fn Revert(&self) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn LockRegion(&self, _: u64, _: u64, _: &LOCKTYPE) -> windows::core::Result<()> {
        Err(STG_E_INVALIDFUNCTION.into())
    }
    fn UnlockRegion(&self, _: u64, _: u64, _: u32) -> windows::core::Result<()> {
        Err(STG_E_INVALIDFUNCTION.into())
    }
    fn Stat(&self, p: *mut STATSTG, _: &STATFLAG) -> windows::core::Result<()> {
        if p.is_null() {
            return Err(E_POINTER.into());
        }
        unsafe {
            *p = STATSTG {
                r#type: STGTY_STREAM.0 as u32,
                cbSize: self.size,
                grfMode: STGM_READ,
                ..Default::default()
            };
        }
        Ok(())
    }
    fn Clone(&self) -> windows::core::Result<IStream> {
        self.offer.streams.fetch_add(1, Ordering::AcqRel);
        Ok(RemoteStream {
            offer: self.offer.clone(),
            index: self.index,
            size: self.size,
            position: Mutex::new(*lock(&self.position)),
            cache: Mutex::new(None),
        }
        .into())
    }
}
