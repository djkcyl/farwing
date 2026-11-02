//! Pictures in a voice channel (`docs/protocol/chat-design.md` §8): each
//! watched camera or screen's packets put back in order (lost ones come
//! again from the forwarder, or a keyframe is asked for), made into frames,
//! opened in end-to-end channels, decoded on the window's graphics adapter
//! and drawn into a shared texture the window shows
//! (`ui::video_textures`).

use crate::media::frame_crypto::{Codec, Decryptor};
use crate::platform::windows::decoder::{WindowsDecodedFrame, WindowsVideoDecoder};
use crate::platform::windows::video_shader::{VideoShaderRenderer, VideoTextureView};
use anyhow::{Context, Result};
use bytes::Bytes;
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use webrtc::rtp::codecs::h264::H264Packet;
use webrtc::rtp::packet::Packet;
use webrtc::rtp::packetizer::Depacketizer;
use webrtc::track::track_remote::TrackRemote;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::IDXGIResource;
use windows::core::Interface;

/// A gap the forwarder has not filled by then is given up: the picture
/// waits for a keyframe.
const REPAIR_WAIT: Duration = Duration::from_millis(200);
/// Keyframes are asked for at most this often.
const KEYFRAME_ASK: Duration = Duration::from_millis(500);
/// Textures drawn in turn, so the window never reads the one being drawn.
const OUTPUTS: usize = 4;

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The newest picture of one track: a shared texture on the window's
/// adapter (`ui::video_textures::texture`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Picture {
    pub handle: isize,
    pub width: u32,
    pub height: u32,
}

/// What a watched track brought (`dev voice`).
#[derive(Clone, Debug, Default, serde::Serialize)]
pub(crate) struct PictureStats {
    pub participant: u32,
    pub kind: String,
    pub frames: u64,
    pub width: u32,
    pub height: u32,
}

struct Feed {
    picture: Arc<Mutex<Option<Picture>>>,
    frames: Arc<AtomicU64>,
    task: tokio::task::JoinHandle<()>,
}

/// The tracks this device watches, by participant and kind.
#[derive(Clone, Default)]
pub(crate) struct Pictures(Arc<Mutex<HashMap<(u32, String), Feed>>>);

impl Pictures {
    /// Starts showing `track` (`participant`'s `kind`); `decryptor` opens
    /// its frames in an end-to-end channel.
    pub(crate) fn receive(
        &self,
        participant: u32,
        kind: &str,
        track: Arc<TrackRemote>,
        decryptor: Option<Arc<Mutex<Decryptor>>>,
        pc: Weak<RTCPeerConnection>,
    ) {
        let picture: Arc<Mutex<Option<Picture>>> = Arc::default();
        let frames = Arc::new(AtomicU64::new(0));
        let wants_keyframe = Arc::new(AtomicBool::new(false));
        let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(4);
        {
            let (picture, frames, wants_keyframe) = (Arc::clone(&picture), Arc::clone(&frames), Arc::clone(&wants_keyframe));
            let large = kind != farwing_protocol::chat::track::CAM_LO;
            let spawned = std::thread::Builder::new().name("voice-picture".into()).spawn(move || {
                if let Err(e) = decode_loop(&rx, &picture, &frames, &wants_keyframe, large) {
                    tracing::warn!(error = %format!("{e:#}"), "voice picture decoding stopped");
                }
            });
            if spawned.is_err() {
                return;
            }
        }
        tracing::info!(participant, kind, "voice picture track");
        let task = tokio::spawn(receive_loop(track, tx, decryptor, pc, wants_keyframe));
        let old = lock(&self.0).insert(
            (participant, kind.to_owned()),
            Feed {
                picture,
                frames,
                task,
            },
        );
        if let Some(old) = old {
            old.task.abort();
        }
    }

    pub(crate) fn picture(&self, participant: u32, kind: &str) -> Option<Picture> {
        let feeds = lock(&self.0);
        let feed = feeds.get(&(participant, kind.to_owned()))?;
        *lock(&feed.picture)
    }

    /// `participant` is gone.
    pub(crate) fn forget(&self, participant: u32) {
        lock(&self.0).retain(|(p, _), feed| {
            if *p == participant {
                feed.task.abort();
            }
            *p != participant
        });
    }

    pub(crate) fn stats(&self) -> Vec<PictureStats> {
        let mut out: Vec<PictureStats> = lock(&self.0)
            .iter()
            .map(|((participant, kind), feed)| {
                let picture = lock(&feed.picture).unwrap_or_default();
                PictureStats {
                    participant: *participant,
                    kind: kind.clone(),
                    frames: feed.frames.load(Ordering::Relaxed),
                    width: picture.width,
                    height: picture.height,
                }
            })
            .collect();
        out.sort_by(|a, b| (a.participant, &a.kind).cmp(&(b.participant, &b.kind)));
        out
    }

    pub(crate) fn close(&self) {
        for (_, feed) in lock(&self.0).drain() {
            feed.task.abort();
        }
    }
}

/// Packets back in order by sequence number (extended past wrapping).
#[derive(Default)]
struct Order {
    next: Option<u64>,
    highest: u64,
    held: BTreeMap<u64, Packet>,
    /// Since when the next packet is missing while later ones wait.
    gap: Option<Instant>,
}

impl Order {
    fn extend(&mut self, seq: u16) -> u64 {
        if self.highest == 0 {
            self.highest = (1 << 32) + u64::from(seq);
            return self.highest;
        }
        let near = (self.highest & !0xFFFF) | u64::from(seq);
        let ext = if near + 0x8000 < self.highest {
            near + 0x1_0000
        } else if near > self.highest + 0x8000 {
            near - 0x1_0000
        } else {
            near
        };
        self.highest = self.highest.max(ext);
        ext
    }

    fn push(&mut self, packet: Packet) {
        let ext = self.extend(packet.header.sequence_number);
        let next = *self.next.get_or_insert(ext);
        if ext >= next {
            self.held.entry(ext).or_insert(packet);
        }
    }

    /// The packets now in order; true when a gap was given up.
    fn take(&mut self, now: Instant) -> (Vec<Packet>, bool) {
        let (mut out, mut lost) = (Vec::new(), false);
        while let Some(next) = self.next {
            if let Some(packet) = self.held.remove(&next) {
                out.push(packet);
                self.next = Some(next + 1);
                self.gap = None;
                continue;
            }
            let Some(&first) = self.held.keys().next() else { break };
            let since = *self.gap.get_or_insert(now);
            if now.duration_since(since) < REPAIR_WAIT && self.held.len() < 1024 {
                break;
            }
            self.next = Some(first);
            self.gap = None;
            lost = true;
        }
        (out, lost)
    }
}

/// H.264 frames out of ordered packets (Annex B).
#[derive(Default)]
struct Assembler {
    depacketizer: H264Packet,
    timestamp: Option<u32>,
    data: Vec<u8>,
    broken: bool,
}

impl Assembler {
    /// The frames `packet` completes.
    fn push(&mut self, packet: &Packet, done: &mut Vec<Vec<u8>>) {
        // The previous frame's last packet (with the marker) never came.
        if self.timestamp.is_some_and(|t| t != packet.header.timestamp) {
            done.extend(self.finish());
        }
        self.timestamp = Some(packet.header.timestamp);
        match self.depacketizer.depacketize(&packet.payload) {
            Ok(bytes) => self.data.extend_from_slice(&bytes),
            Err(_) => self.broken = true,
        }
        if packet.header.marker {
            done.extend(self.finish());
        }
    }

    fn finish(&mut self) -> Option<Vec<u8>> {
        let data = std::mem::take(&mut self.data);
        let whole = !self.broken && !data.is_empty();
        self.reset();
        whole.then_some(data)
    }

    fn reset(&mut self) {
        self.data.clear();
        self.timestamp = None;
        self.broken = false;
        self.depacketizer = H264Packet::default();
    }
}

fn keyframe(frame: &[u8]) -> bool {
    crate::media::video_format::annex_b_units(frame)
        .iter()
        .any(|unit| unit.first().is_some_and(|b| b & 0x1F == 5))
}

async fn receive_loop(
    track: Arc<TrackRemote>,
    frames: SyncSender<Vec<u8>>,
    decryptor: Option<Arc<Mutex<Decryptor>>>,
    pc: Weak<RTCPeerConnection>,
    wants_keyframe: Arc<AtomicBool>,
) {
    let ssrc = track.ssrc();
    let payload_type = track.payload_type();
    // Repairs: the original packet inside each retransmission.
    let (repaired_tx, mut repaired) = tokio::sync::mpsc::channel::<Packet>(256);
    let rtx = {
        let track = Arc::clone(&track);
        tokio::spawn(async move {
            while let Ok((mut packet, _)) = track.read_rtx().await {
                if packet.payload.len() <= 2 {
                    continue;
                }
                packet.header.sequence_number = u16::from_be_bytes([packet.payload[0], packet.payload[1]]);
                packet.payload = packet.payload.slice(2..);
                packet.header.ssrc = ssrc;
                packet.header.payload_type = payload_type;
                packet.header.padding = false;
                if repaired_tx.send(packet).await.is_err() {
                    break;
                }
            }
        })
    };
    let mut order = Order::default();
    let mut assembler = Assembler::default();
    let mut need_keyframe = true;
    let mut asked: Option<Instant> = None;
    let mut done = Vec::new();
    let mut tick = tokio::time::interval(Duration::from_millis(50));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    'receive: loop {
        let packet = tokio::select! {
            read = track.read_rtp() => match read {
                Ok((packet, _)) => Some(packet),
                Err(_) => break,
            },
            Some(packet) = repaired.recv() => Some(packet),
            _ = tick.tick() => None,
        };
        let now = Instant::now();
        if let Some(packet) = packet {
            order.push(packet);
        }
        let (ready, lost) = order.take(now);
        if lost {
            assembler.reset();
            need_keyframe = true;
        }
        for packet in &ready {
            assembler.push(packet, &mut done);
        }
        for frame in done.drain(..) {
            let frame = match &decryptor {
                Some(d) => match lock(d).decrypt(Codec::H264, &frame) {
                    Ok(opened) => opened,
                    Err(_) => {
                        need_keyframe = true;
                        continue;
                    }
                },
                None => frame,
            };
            let key = keyframe(&frame);
            if need_keyframe && !key {
                continue;
            }
            match frames.try_send(frame) {
                Ok(()) => need_keyframe &= !key,
                // The decoder is behind: start again from a keyframe.
                Err(TrySendError::Full(_)) => need_keyframe = true,
                Err(TrySendError::Disconnected(_)) => break 'receive,
            }
        }
        if wants_keyframe.swap(false, Ordering::AcqRel) {
            need_keyframe = true;
        }
        if need_keyframe && asked.is_none_or(|at| now.duration_since(at) >= KEYFRAME_ASK) {
            asked = Some(now);
            if let Some(pc) = pc.upgrade() {
                let _ = pc
                    .write_rtcp(&[Box::new(PictureLossIndication {
                        sender_ssrc: 0,
                        media_ssrc: ssrc,
                    })])
                    .await;
            }
        }
    }
    rtx.abort();
}

/// The decoder, opened on the first keyframe: hardware first.
fn open_decoder(device: &ID3D11Device) -> Result<WindowsVideoDecoder> {
    use crate::media::decode_api::{DecoderMode, VideoDecoderConfig};
    let config = |mode| VideoDecoderConfig {
        codec: crate::media::VideoCodec::H264,
        width: 1280,
        height: 720,
        mode,
        gpu_device: Some(device.clone()),
        extra_data: Bytes::new(),
    };
    WindowsVideoDecoder::open(&config(DecoderMode::Hardware))
        .or_else(|e| {
            tracing::debug!(error = ?e, "voice picture: software decoder");
            WindowsVideoDecoder::open(&config(DecoderMode::Software))
        })
        .map_err(|e| anyhow::anyhow!("open decoder: {e:?}"))
}

/// Where decoded pictures are drawn: textures shared with the window.
struct Outputs {
    size: (u32, u32),
    textures: Vec<(ID3D11Texture2D, ID3D11RenderTargetView, isize)>,
    turn: usize,
}

impl Outputs {
    fn new(device: &ID3D11Device, size: (u32, u32)) -> Result<Self> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: size.0,
            Height: size.1,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: D3D11_RESOURCE_MISC_SHARED.0 as u32,
        };
        let mut textures = Vec::with_capacity(OUTPUTS);
        for _ in 0..OUTPUTS {
            // SAFETY: a texture and its target on our device; the shared
            // handle lives as long as the texture.
            unsafe {
                let mut texture = None;
                device.CreateTexture2D(&desc, None, Some(&mut texture))?;
                let texture = texture.context("picture texture")?;
                let mut target = None;
                device.CreateRenderTargetView(&texture, None, Some(&mut target))?;
                let handle = texture.cast::<IDXGIResource>()?.GetSharedHandle()?;
                textures.push((texture, target.context("picture target")?, handle.0 as isize));
            }
        }
        Ok(Self { size, textures, turn: 0 })
    }
}

fn decode_loop(
    frames: &Receiver<Vec<u8>>,
    picture: &Mutex<Option<Picture>>,
    count: &AtomicU64,
    wants_keyframe: &AtomicBool,
    large: bool,
) -> Result<()> {
    let adapter = crate::ui::video_textures::adapter()
        .or_else(|| {
            let screens = crate::platform::windows::capture::screens().ok()?;
            let screen = screens.iter().find(|s| s.primary).or(screens.first())?;
            Some(screen.render_adapter.unwrap_or(screen.adapter))
        })
        .context("no graphics adapter")?;
    let (device, context) = crate::platform::windows::capture::create_device(adapter)?;
    let mut renderer = VideoShaderRenderer::new(&device)?;
    let mut decoder: Option<WindowsVideoDecoder> = None;
    let mut outputs: Option<Outputs> = None;
    let mut token = 0i64;
    // SAFETY: an event query on our device.
    let query = unsafe {
        let mut query = None;
        device.CreateQuery(
            &D3D11_QUERY_DESC {
                Query: D3D11_QUERY_EVENT,
                MiscFlags: 0,
            },
            Some(&mut query),
        )?;
        query.context("picture query")?
    };
    loop {
        let frame = match frames.recv_timeout(Duration::from_secs(1)) {
            Ok(frame) => frame,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => return Ok(()),
        };
        if decoder.is_none() {
            if !keyframe(&frame) {
                continue;
            }
            decoder = Some(open_decoder(&device)?);
        }
        let Some(dec) = decoder.as_mut() else { continue };
        token += 1;
        if let Err(e) = dec.push_packet(&frame, token) {
            tracing::debug!(error = ?e, "voice picture not decoded");
            wants_keyframe.store(true, Ordering::Release);
            if dec.reset_for_keyframe().is_err() {
                decoder = None;
            }
            continue;
        }
        let mut latest = None;
        loop {
            match dec.poll_owned_frame() {
                Ok(Some(f)) => latest = Some(f),
                Ok(None) => break,
                Err(e) => {
                    tracing::debug!(error = ?e, "voice picture not decoded");
                    wants_keyframe.store(true, Ordering::Release);
                    break;
                }
            }
        }
        let Some(decoded) = latest else { continue };
        let drawn = draw(&device, &context, &mut renderer, &mut outputs, &decoded, large)?;
        // Done on the GPU before the window may read it.
        // SAFETY: the query was ended on this context; polled until done.
        unsafe {
            context.End(&query);
            context.Flush();
            let started = Instant::now();
            loop {
                let mut finished = windows::core::BOOL(0);
                let polled = context.GetData(&query, Some((&raw mut finished).cast()), size_of::<windows::core::BOOL>() as u32, 0);
                if polled.is_err() || finished.as_bool() || started.elapsed() > Duration::from_millis(50) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        count.fetch_add(1, Ordering::Relaxed);
        *lock(picture) = Some(drawn);
    }
}

/// Draws `decoded` into the next output texture.
fn draw(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    renderer: &mut VideoShaderRenderer,
    outputs: &mut Option<Outputs>,
    decoded: &WindowsDecodedFrame,
    large: bool,
) -> Result<Picture> {
    // The decoder's surfaces are shared under a keyed mutex: owned while
    // drawn, even on their own device.
    let mut sync = None;
    let (texture, view, size) = match decoded {
        WindowsDecodedFrame::Gpu(frame) => {
            sync = crate::platform::surface::acquire_owned_texture_sync(frame.texture(), 100)?;
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            // SAFETY: a description query of the decoder's texture.
            unsafe { frame.texture().GetDesc(&mut desc) };
            let size = (frame.width(), frame.height());
            (
                frame.texture().clone(),
                VideoTextureView {
                    color: color(size, large),
                    array_slice: frame.subresource(),
                    input_format: desc.Format,
                    visible_x: frame.visible_x(),
                    visible_y: frame.visible_y(),
                    coded_width: desc.Width,
                    coded_height: desc.Height,
                    width: size.0,
                    height: size.1,
                },
                size,
            )
        }
        WindowsDecodedFrame::Cpu(frame) => {
            anyhow::ensure!(
                frame.format == crate::platform::windows::decoder::WindowsCpuFormat::Nv12,
                "picture format {:?}",
                frame.format
            );
            let desc = D3D11_TEXTURE2D_DESC {
                Width: frame.coded_width,
                Height: frame.coded_height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_NV12,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                ..Default::default()
            };
            let data = D3D11_SUBRESOURCE_DATA {
                pSysMem: frame.data.as_ptr().cast(),
                SysMemPitch: frame.coded_width,
                SysMemSlicePitch: 0,
            };
            let mut uploaded = None;
            // SAFETY: the frame's bytes outlive the call.
            unsafe { device.CreateTexture2D(&desc, Some(&data), Some(&mut uploaded))? };
            let size = (frame.width, frame.height);
            (
                uploaded.context("picture upload")?,
                VideoTextureView {
                    color: color(size, large),
                    array_slice: 0,
                    input_format: DXGI_FORMAT_NV12,
                    visible_x: 0,
                    visible_y: 0,
                    coded_width: frame.coded_width,
                    coded_height: frame.coded_height,
                    width: size.0,
                    height: size.1,
                },
                size,
            )
        }
    };
    if outputs.as_ref().is_none_or(|o| o.size != size) {
        *outputs = Some(Outputs::new(device, size)?);
    }
    let out = outputs.as_mut().expect("made above");
    out.turn = (out.turn + 1) % out.textures.len();
    let (_, target, handle) = &out.textures[out.turn];
    let drawn = renderer.draw(device, context, target, &texture, view, winit::dpi::PhysicalSize::new(size.0, size.1), 0);
    if let Some(sync) = sync {
        // SAFETY: acquired above on this thread.
        let _ = unsafe { sync.ReleaseSync(0) };
    }
    drawn?;
    Ok(Picture {
        handle: *handle,
        width: size.0,
        height: size.1,
    })
}

/// Senders signal no color (browsers' small pictures are BT.601, the rest
/// BT.709), limited range.
fn color(size: (u32, u32), large: bool) -> crate::media::video_color::RenderColor {
    use crate::media::video_color::{ColorMatrix, RenderColor};
    RenderColor {
        matrix: if large && size.1 >= 720 { ColorMatrix::Bt709 } else { ColorMatrix::Bt601 },
        full_range: false,
        hdr_peak_nits: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(seq: u16) -> Packet {
        let mut p = Packet::default();
        p.header.sequence_number = seq;
        p
    }

    #[test]
    fn order_waits_for_a_repair_then_gives_up() {
        let mut order = Order::default();
        let now = Instant::now();
        order.push(packet(65534));
        order.push(packet(65535));
        order.push(packet(1));
        let (out, lost) = order.take(now);
        assert_eq!(out.len(), 2);
        assert!(!lost);
        order.push(packet(0));
        let (out, lost) = order.take(now);
        assert_eq!(out.iter().map(|p| p.header.sequence_number).collect::<Vec<_>>(), [0, 1]);
        assert!(!lost);
        order.push(packet(3));
        assert!(order.take(now).0.is_empty());
        let (out, lost) = order.take(now + REPAIR_WAIT);
        assert_eq!(out.len(), 1);
        assert!(lost);
    }
}
