//! Bounded wallpaper memory/disk cache; downloads never use account credentials.
mod disk;
use anyhow::{Context, Result, bail};
use std::{
    collections::{HashMap, HashSet},
    io::Cursor,
    path::Path,
    sync::mpsc,
    time::Duration,
};
use tokio_util::sync::CancellationToken;

#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Source {
    pub device_id: String,
    pub url: String,
}
impl Source {
    pub fn new(device_id: &str, url: &str) -> Self {
        Self {
            device_id: device_id.into(),
            url: url.into(),
        }
    }
}
impl std::fmt::Debug for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WallpaperSource").finish_non_exhaustive()
    }
}

struct Entry {
    url: String,
    ticket: u64,
    touched: u64,
    texture: Option<egui::TextureHandle>,
    failed: bool,
}

struct Loaded {
    id: String,
    ticket: u64,
    pixels: Option<egui::ColorImage>,
    complete: bool,
}

pub(crate) struct Wallpapers {
    entries: HashMap<String, Entry>,
    tasks: HashMap<u64, CancellationToken>,
    sender: mpsc::Sender<Loaded>,
    receiver: mpsc::Receiver<Loaded>,
    refresh: HashSet<String>,
    next: u64,
    clock: u64,
}

impl Default for Wallpapers {
    fn default() -> Self {
        let (sender, receiver) = mpsc::channel();
        Self {
            entries: HashMap::new(),
            tasks: HashMap::new(),
            sender,
            receiver,
            refresh: HashSet::new(),
            next: 0,
            clock: 0,
        }
    }
}

impl Wallpapers {
    pub fn clear(&mut self) {
        for cancel in self.tasks.values() {
            cancel.cancel();
        }
        self.entries.clear();
        self.refresh.clear();
    }

    pub fn invalidate(&mut self, id: &str) {
        if let Some(entry) = self.entries.remove(id)
            && let Some(cancel) = self.tasks.get(&entry.ticket)
        {
            cancel.cancel();
        }
    }

    pub fn refresh(&mut self, id: &str) {
        self.invalidate(id);
        self.refresh.insert(id.into());
    }

    pub fn poll(&mut self, ctx: &egui::Context) {
        while let Ok(Loaded {
            id,
            ticket,
            pixels,
            complete,
        }) = self.receiver.try_recv()
        {
            if complete {
                self.tasks.remove(&ticket);
            }
            if let Some(entry) = self.entries.get_mut(&id)
                && entry.ticket == ticket
            {
                if let Some(pixels) = pixels {
                    entry.texture = Some(ctx.load_texture(
                        format!("device-wallpaper-{ticket}"),
                        pixels,
                        egui::TextureOptions::LINEAR,
                    ));
                }
                entry.failed = complete && entry.texture.is_none();
            }
        }
    }

    pub fn failed(&self, id: &str) -> bool {
        self.entries.get(id).is_some_and(|e| e.failed)
    }

    pub fn texture(
        &mut self,
        ctx: &egui::Context,
        id: &str,
        url: &str,
    ) -> Option<egui::TextureHandle> {
        self.clock = self.clock.wrapping_add(1);
        if let Some(entry) = self.entries.get_mut(id)
            && entry.url == url
        {
            entry.touched = self.clock;
            return entry.texture.clone();
        }
        self.invalidate(id);
        if url.is_empty() || self.tasks.len() >= 3 {
            return None;
        }
        if self.entries.len() >= 24
            && let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, e)| e.touched)
                .map(|(id, _)| id.clone())
        {
            self.invalidate(&oldest);
        }
        self.next = self.next.wrapping_add(1);
        let ticket = self.next;
        let cancel = CancellationToken::new();
        self.entries.insert(
            id.into(),
            Entry {
                url: url.into(),
                ticket,
                touched: self.clock,
                texture: None,
                failed: false,
            },
        );
        self.tasks.insert(ticket, cancel.clone());
        let sender = self.sender.clone();
        let key = id.to_owned();
        let id = key.clone();
        let address = url.to_owned();
        let disk_path = disk::path(&id, url);
        let force_refresh = self.refresh.remove(&id);
        let repaint = ctx.clone();
        let result = std::thread::Builder::new()
            .name("wallpaper".into())
            .spawn(move || {
                let pixels = load(
                    &address,
                    disk_path.as_deref(),
                    force_refresh,
                    &cancel,
                    |pixels| {
                        let _ = sender.send(Loaded {
                            id: id.clone(),
                            ticket,
                            pixels: Some(pixels),
                            complete: false,
                        });
                        repaint.request_repaint();
                    },
                )
                .ok();
                let _ = sender.send(Loaded {
                    id,
                    ticket,
                    pixels,
                    complete: true,
                });
                repaint.request_repaint();
            });
        if result.is_err() {
            self.tasks.remove(&ticket);
            if let Some(entry) = self.entries.get_mut(&key) {
                entry.failed = true;
            }
        }
        None
    }
}

impl Drop for Wallpapers {
    fn drop(&mut self) {
        self.clear();
    }
}

fn decode(bytes: Vec<u8>) -> Result<image::RgbaImage> {
    let mut reader = image::ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    limits.max_alloc = Some(64 * 1024 * 1024);
    reader.limits(limits);
    let image = reader.decode()?;
    Ok(if image.width() > 960 || image.height() > 540 {
        image.thumbnail(960, 540)
    } else {
        image
    }
    .into_rgba8())
}

fn color_image(pixels: &image::RgbaImage) -> egui::ColorImage {
    egui::ColorImage::from_rgba_unmultiplied(
        [pixels.width() as usize, pixels.height() as usize],
        pixels.as_raw(),
    )
}

fn load(
    address: &str,
    path: Option<&Path>,
    force_refresh: bool,
    cancel: &CancellationToken,
    preview: impl FnOnce(egui::ColorImage),
) -> Result<egui::ColorImage> {
    if cancel.is_cancelled() {
        bail!("cancelled");
    }
    if let Some((pixels, fresh)) = path
        .and_then(|path| disk::read(path).ok())
        .and_then(|(bytes, fresh)| decode(bytes).ok().map(|pixels| (pixels, fresh)))
    {
        if fresh && !force_refresh {
            return Ok(color_image(&pixels));
        }
        // Keep the old image visible if the background refresh fails.
        preview(color_image(&pixels));
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let bytes = runtime.block_on(async {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => bail!("cancelled"),
            result = download(address) => result,
        }
    })?;
    if cancel.is_cancelled() {
        bail!("cancelled");
    }
    let pixels = decode(bytes)?;
    if let Some(path) = path {
        let mut png = Cursor::new(Vec::new());
        if pixels.write_to(&mut png, image::ImageFormat::Png).is_ok() {
            let _ = disk::write(path, png.get_ref(), cancel);
        }
    }
    Ok(color_image(&pixels))
}

async fn download(address: &str) -> Result<Vec<u8>> {
    let url = reqwest::Url::parse(address)?;
    let local_wallpaper = url.scheme() == "http"
        && url.host_str().is_some_and(crate::transport::signal::is_loopback_host)
        && url.path().strip_prefix("/v1/wallpapers/").is_some_and(|id| uuid::Uuid::parse_str(id).is_ok())
        && url.query().is_none();
    if (!local_wallpaper && url.scheme() != "https") || !url.username().is_empty() || url.password().is_some() {
        bail!("unsupported wallpaper URL");
    }
    let client = if local_wallpaper {
        crate::session::local_host::http_client()?
    } else {
        crate::transport::http_client()
            .https_only(true)
            .redirect(reqwest::redirect::Policy::limited(3))
            .timeout(Duration::from_secs(12))
            .build()?
    };
    let mut response = crate::transport::http::send(
        crate::transport::http::Operation::WallpaperDownload,
        client.get(url),
    )
    .await?
    .error_for_status()?;
    const LIMIT: usize = 8 * 1024 * 1024;
    if response.content_length().is_some_and(|n| n > LIMIT as u64) {
        bail!("wallpaper too large");
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if bytes
            .len()
            .checked_add(chunk.len())
            .context("wallpaper size")?
            > LIMIT
        {
            bail!("wallpaper too large");
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}
