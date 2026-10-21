//! In-memory clipboard contents instead of the Windows clipboard: the
//! protocol loopback tests and `dev clipboard` exercise CLIPBOARD v2 (and the
//! DRAG file exchange) without OLE or the shared system clipboard.
use super::*;

#[derive(Default)]
pub(crate) struct Store {
    local: Mutex<Option<Local>>,
    remote: Mutex<Option<(u32, Vec<Format>)>>,
    offered: Condvar,
}

struct Local {
    offer: u32,
    formats: Vec<Format>,
    contents: Vec<(Format, Vec<u8>)>,
    files: Vec<(FileItem, Vec<u8>)>,
}

/// The formats announcing a file list, as the native adapter offers them.
pub(crate) fn file_formats() -> Vec<Format> {
    ["FileGroupDescriptorW", "FileContents"]
        .into_iter()
        .map(|name| Format {
            id: 0,
            name: name.into(),
        })
        .collect()
}

impl Store {
    pub(super) fn remote_offer(&self, offer: u32, formats: Vec<Format>) {
        *lock(&self.remote) = Some((offer, formats));
        self.offered.notify_all();
    }
}

/// Serve a request from the local in-memory contents.
pub(super) fn serve(inner: &Arc<Inner>, store: &Store, epoch: u64, id: u32, body: Body) {
    use farwing_protocol::peer::error_code;
    let local = lock(&store.local);
    let current = |offer: u32| local.as_ref().filter(|local| local.offer == offer);
    let result = match body {
        Body::Fetch(fetch) => match (current(fetch.offer), fetch.format) {
            (Some(local), Some(format)) => match local
                .contents
                .iter()
                .find(|(f, _)| f.id == format.id && f.name == format.name)
            {
                Some((_, data)) => inner.respond_data(epoch, id, fetch.stream, data.clone()),
                None => Err(anyhow!("{}", t().features.clipboard_format_not_offered)),
            },
            _ => Err(anyhow!("{}", t().features.clipboard_offer_expired)),
        },
        Body::ListFiles(list) => match current(list.offer).filter(|l| !l.files.is_empty()) {
            Some(local) => inner.respond(
                epoch,
                id,
                Body::Files(wire::Files {
                    items: local.files.iter().map(|(item, _)| item.clone()).collect(),
                }),
            ),
            None => Err(anyhow!("{}", t().features.clipboard_file_offer_expired)),
        },
        Body::ReadFile(read) => match current(read.offer)
            .and_then(|local| local.files.get(read.index as usize))
        {
            Some((item, data)) if item.attributes & 0x10 == 0 && read.length as usize <= FILE_BLOCK => {
                let start = (read.offset as usize).min(data.len());
                let end = (start + read.length as usize).min(data.len());
                inner.respond_data(epoch, id, read.stream, data[start..end].to_vec())
            }
            _ => Err(anyhow!("无效的文件读取")),
        },
        Body::Pull(_) => inner.respond(
            epoch,
            id,
            Body::Offer(wire::Offer {
                offer: local.as_ref().map_or(0, |local| local.offer),
                formats: local.as_ref().map(|local| local.formats.clone()).unwrap_or_default(),
            }),
        ),
        _ => Err(anyhow!("{}", t().features.clipboard_invalid_request)),
    };
    if let Err(error) = result {
        inner.refuse(id, error_code::NOT_FOUND, &error.to_string());
    }
}

impl Clipboard {
    fn store(&self) -> Result<&Arc<Store>> {
        match &self.0.backend {
            Backend::Memory(store) => Ok(store),
            Backend::Native => bail!("不是内存剪贴板"),
        }
    }
    /// Publish in-memory contents: formats with their data and, optionally,
    /// files (`FileItem` + contents; directories have no contents).
    pub(crate) fn memory_offer(
        &self,
        contents: Vec<(Format, Vec<u8>)>,
        files: Vec<(FileItem, Vec<u8>)>,
    ) -> Result<u32> {
        let store = self.store()?;
        let mut formats: Vec<Format> = contents.iter().map(|(f, _)| f.clone()).collect();
        if !files.is_empty() {
            formats.extend(file_formats());
        }
        let mut local = lock(&store.local);
        let offer = self.0.offer(self.epoch(), formats.clone())?;
        *local = Some(Local {
            offer,
            formats,
            contents,
            files,
        });
        Ok(offer)
    }
    /// Wait for the peer's next offer newer than `after` (0: any).
    pub(crate) fn memory_wait_offer(
        &self,
        after: u32,
        timeout: Duration,
    ) -> Result<(u32, Vec<Format>)> {
        let store = self.store()?;
        let deadline = std::time::Instant::now() + timeout;
        let mut remote = lock(&store.remote);
        loop {
            if let Some((offer, formats)) = remote.as_ref()
                && *offer != after
            {
                return Ok((*offer, formats.clone()));
            }
            let left = deadline
                .checked_duration_since(std::time::Instant::now())
                .ok_or_else(|| anyhow!("等待远端剪贴板内容超时"))?;
            remote = store
                .offered
                .wait_timeout(remote, left)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }
    /// Contents of `format` in the peer's offer `offer` (blocks).
    pub(crate) fn memory_fetch(&self, offer: u32, format: &Format) -> Result<Vec<u8>> {
        self.0.data(self.epoch(), offer, format)
    }
    /// The file list of the peer's offer (blocks).
    pub(crate) fn memory_list(&self, offer: u32) -> Result<Vec<FileItem>> {
        self.0.descriptors(self.epoch(), offer)
    }
    /// A whole file of the peer's offer, read in protocol blocks (blocks).
    pub(crate) fn memory_read(&self, offer: u32, index: u32, size: u64) -> Result<Vec<u8>> {
        let mut data = Vec::with_capacity(size as usize);
        while (data.len() as u64) < size {
            let length = (size - data.len() as u64).min(FILE_BLOCK as u64) as usize;
            let block = self
                .0
                .read_file(self.epoch(), offer, index, data.len() as u64, length)?;
            ensure!(!block.is_empty(), "远端文件提前结束");
            data.extend_from_slice(&block);
        }
        Ok(data)
    }
    /// The peer no longer reads the files of its `offer`.
    pub(crate) fn memory_release(&self, offer: u32) {
        self.0.release(offer);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::Digest;

    /// Two exchanges wired back to back: every frame one sends is received
    /// by the other, as the session and bulk channels would deliver it.
    fn pair(kind: i32) -> (tokio::runtime::Runtime, Clipboard, Clipboard) {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let a = Clipboard::memory(kind);
        let b = Clipboard::memory(kind);
        for (from, to) in [(a.clone(), b.clone()), (b.clone(), a.clone())] {
            let _guard = runtime.enter();
            runtime.spawn(from.sender(move |_, frame| {
                let to = to.clone();
                async move {
                    assert!(frame.len() <= 64 * 1024, "frame of {} bytes", frame.len());
                    assert!(to.receive(&frame)?, "frame not claimed");
                    Ok(())
                }
            }));
        }
        (runtime, a, b)
    }

    fn blob(len: usize, seed: u8) -> Vec<u8> {
        (0..len).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed)).collect()
    }

    #[test]
    fn text_rich_formats_and_large_contents_round_trip() {
        let (_runtime, a, b) = pair(transfer::kind::CLIPBOARD);
        let text = Format { id: 13, name: String::new() };
        let html = Format { id: 0xc0b1, name: "HTML Format".into() };
        let image = Format { id: 8, name: String::new() };
        let large = Format { id: 0xc0f0, name: "Farwing.Test.Blob".into() };
        let contents = vec![
            (text.clone(), "你好, clipboard\0".encode_utf16().flat_map(u16::to_le_bytes).collect()),
            (html.clone(), b"Version:1.0\r\n<b>rich</b>".to_vec()),
            (image.clone(), blob(300 * 1024, 1)),
            (large.clone(), blob(24 * 1024 * 1024 + 17, 2)),
        ];
        let offer = a.memory_offer(contents.clone(), Vec::new()).unwrap();
        let (seen, formats) = b.memory_wait_offer(0, Duration::from_secs(5)).unwrap();
        assert_eq!(seen, offer);
        assert_eq!(formats.len(), 4);
        for (format, data) in &contents {
            let got = b.memory_fetch(offer, format).unwrap();
            assert_eq!(got.len(), data.len());
            assert!(got == *data, "format {} differs", format.id);
        }
        // An outdated offer or an unknown format fails without hanging.
        assert!(b.memory_fetch(offer + 1, &text).is_err());
        assert!(b.memory_fetch(offer, &Format { id: 2, name: String::new() }).is_err());
        // The other direction uses the same exchange.
        let reply = b
            .memory_offer(vec![(text.clone(), b"back".to_vec())], Vec::new())
            .unwrap();
        let (seen, _) = a.memory_wait_offer(0, Duration::from_secs(5)).unwrap();
        assert_eq!(seen, reply);
        assert_eq!(a.memory_fetch(reply, &text).unwrap(), b"back");
    }

    #[test]
    fn files_are_listed_and_read_in_blocks() {
        for kind in [transfer::kind::CLIPBOARD, transfer::kind::DRAG] {
            let (_runtime, a, b) = pair(kind);
            let files = vec![
                (
                    FileItem { path: "folder".into(), attributes: 0x10, mtime: 1, size: 0 },
                    Vec::new(),
                ),
                (
                    FileItem { path: "folder\\empty.txt".into(), attributes: 0x20, mtime: 2, size: 0 },
                    Vec::new(),
                ),
                (
                    FileItem { path: "folder\\small.txt".into(), attributes: 0x20, mtime: 3, size: 5 },
                    b"hello".to_vec(),
                ),
                (
                    FileItem {
                        path: "folder\\big.bin".into(),
                        attributes: 0x20,
                        mtime: 4,
                        size: 9 * 1024 * 1024 + 3,
                    },
                    blob(9 * 1024 * 1024 + 3, 3),
                ),
            ];
            let offer = a.memory_offer(Vec::new(), files.clone()).unwrap();
            let (seen, formats) = b.memory_wait_offer(0, Duration::from_secs(5)).unwrap();
            assert_eq!(seen, offer);
            assert_eq!(formats, file_formats());
            let list = b.memory_list(offer).unwrap();
            assert_eq!(list, files.iter().map(|(f, _)| f.clone()).collect::<Vec<_>>());
            for (index, (item, data)) in files.iter().enumerate() {
                if item.attributes & 0x10 != 0 {
                    continue;
                }
                let got = b.memory_read(offer, index as u32, item.size).unwrap();
                assert_eq!(
                    sha2::Sha256::digest(&got),
                    sha2::Sha256::digest(data),
                    "{}",
                    item.path
                );
            }
            b.memory_release(offer);
        }
    }

    #[test]
    fn pull_copies_the_host_clipboard_without_automatic_sync() {
        let (_runtime, host, viewer) = pair(transfer::kind::CLIPBOARD);
        host.host_role();
        let text = Format { id: 13, name: String::new() };
        // The viewer does not sync automatically: the host's offer is not
        // applied, but a pull installs it.
        viewer.policy(true, false, true);
        let offer = host.memory_offer(vec![(text.clone(), b"pulled".to_vec())], Vec::new()).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        assert!(viewer.memory_wait_offer(0, Duration::from_millis(100)).is_err());
        assert_eq!(viewer.pull().unwrap(), 1);
        let (seen, _) = viewer.memory_wait_offer(0, Duration::from_secs(5)).unwrap();
        assert_eq!(seen, offer);
        assert_eq!(viewer.memory_fetch(offer, &text).unwrap(), b"pulled");
        // Only the host answers a pull.
        assert!(host.pull().is_err());
    }

    #[test]
    fn suspended_exchange_refuses_requests() {
        let (_runtime, a, b) = pair(transfer::kind::CLIPBOARD);
        let text = Format { id: 13, name: String::new() };
        let offer = a.memory_offer(vec![(text.clone(), blob(100 * 1024, 4))], Vec::new()).unwrap();
        b.memory_wait_offer(0, Duration::from_secs(5)).unwrap();
        a.suspend();
        let started = std::time::Instant::now();
        assert!(b.memory_fetch(offer, &text).is_err());
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
