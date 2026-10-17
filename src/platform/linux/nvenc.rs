//! NVENC with CUDA input. The session (presets, rate control, encoding) is
//! the one Windows uses, from `media::encoding::nvenc`; this side owns a
//! pitched device buffer registered as the encoder's input. NvFBC frames are
//! copied into it on the GPU, frames from other capture methods are uploaded.
//!
//! The input is 8-bit BGRA ("ARGB" in NVENC's word order), which NVENC
//! converts to the negotiated 4:2:0 or 4:4:4 itself.
use super::capture::Image;
use super::cuda::{Buffer, Context};
use crate::media::encoding::nvenc::{
    NV_ENC_BUFFER_FORMAT, NV_ENC_DEVICE_TYPE, NV_ENC_INPUT_RESOURCE_TYPE, Registered, Session,
    timing,
};
use crate::media::encoding::{Color, Encoded, Format, Rate};
use anyhow::{Result, ensure};
use std::sync::Arc;

pub(crate) struct Encoder {
    /// Destroyed in `drop` while the CUDA context is current.
    session: std::mem::ManuallyDrop<Session>,
    registered: Option<Registered>,
    input: Buffer,
    context: Arc<Context>,
}

impl Encoder {
    /// The formats NVENC encodes from BGRA input.
    pub fn accepts(format: Format) -> bool {
        format.valid() && format.depth == 8
    }

    pub fn new(
        context: &Arc<Context>,
        size: (u32, u32),
        format: Format,
        rate: Rate,
    ) -> Result<Self> {
        ensure!(Self::accepts(format), "NVENC 的 CUDA 输入只支持 8 位格式");
        let input = Buffer::new(context, size.0, size.1)?;
        let _current = context.enter()?;
        let session = Session::open(
            NV_ENC_DEVICE_TYPE::NV_ENC_DEVICE_TYPE_CUDA,
            context.raw(),
            size.0,
            size.1,
            rate,
            format,
            Color::Sdr,
        )?;
        let registered = session.register(
            NV_ENC_INPUT_RESOURCE_TYPE::NV_ENC_INPUT_RESOURCE_TYPE_CUDADEVICEPTR,
            input.ptr as *mut std::ffi::c_void,
            u32::try_from(input.pitch)?,
            NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ARGB,
        )?;
        Ok(Self {
            session: std::mem::ManuallyDrop::new(session),
            registered: Some(registered),
            input,
            context: context.clone(),
        })
    }

    pub fn maximum_size(&self) -> (u32, u32) {
        self.session.maximum_size()
    }

    pub fn configure_rate(&mut self, rate: Rate) -> Result<bool> {
        let _current = self.context.enter()?;
        self.session.configure_rate(rate)
    }

    pub fn encode(
        &mut self,
        image: &Image,
        timestamp: i64,
        keyframe: bool,
    ) -> Result<Vec<Encoded>> {
        let (width, height) = image.size();
        ensure!(
            (width, height) == (self.input.width, self.input.height),
            "采集画面尺寸与编码器不一致"
        );
        let mut timing = timing::Frame::new();
        let result = self.encode_timed(image, timestamp, keyframe, &mut timing);
        self.session.record(timing, result.is_ok());
        result
    }

    fn encode_timed(
        &mut self,
        image: &Image,
        timestamp: i64,
        keyframe: bool,
        timing: &mut timing::Frame,
    ) -> Result<Vec<Encoded>> {
        // NVENC converts the BGRA itself, so there is no conversion stage.
        timing.stage(0);
        let copied = match image {
            Image::Cpu { pixels, .. } => self.input.upload(pixels),
            Image::Cuda(buffer) => self.input.copy_from_device(buffer.ptr, buffer.pitch),
            Image::Nv12(_) => self.input.upload(&image.pixels()?),
        };
        timing.stage(1);
        copied?;
        let _current = self.context.enter()?;
        let registered = self.registered.as_ref().expect("registered until drop");
        self.session.encode(registered, timestamp, keyframe, timing)
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        let _current = self.context.enter();
        // The input is unregistered before its session is destroyed, both
        // with the session's CUDA context current.
        if let Some(registered) = self.registered.take() {
            self.session.unregister(registered);
        }
        // SAFETY: the session is not used again.
        unsafe { std::mem::ManuallyDrop::drop(&mut self.session) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Needs an NVIDIA GPU: `cargo test -- --ignored nvenc_colour`.
    /// NVENC converts the BGRA input itself; its matrix and range must be the
    /// ones the stream's VUI declares (BT.601, limited range for SDR).
    #[test]
    #[ignore]
    fn nvenc_colour_matches_the_declared_matrix() {
        let context = super::super::cuda::display_context().unwrap();
        let (width, height) = (256u32, 256u32);
        let rate = Rate {
            target: 20_000_000,
            peak: 20_000_000,
            fps: 30,
            quality: 4,
            quality_target: crate::media::encoding::QualityTarget {
                bitrate: 20_000_000,
                fps: 60,
            },
        };
        let colour = Color::Sdr.space(None);
        println!("declared VUI: {colour:?}");
        for (name, bgr, bt601, bt709) in [
            ("red", [0u8, 0, 255], (81, 90, 240), (63, 102, 240)),
            ("green", [0, 255, 0], (145, 54, 34), (173, 42, 26)),
            ("blue", [255, 0, 0], (41, 240, 110), (32, 240, 118)),
            ("white", [255, 255, 255], (235, 128, 128), (235, 128, 128)),
        ] {
            let mut pixels = Vec::with_capacity((width * height * 4) as usize);
            for _ in 0..width * height {
                pixels.extend_from_slice(&[bgr[0], bgr[1], bgr[2], 255]);
            }
            let image = Image::Cpu {
                pixels: Arc::new(pixels),
                width,
                height,
            };
            let mut encoder = Encoder::new(
                &context,
                (width, height),
                crate::media::encoding::Format::AVC,
                rate,
            )
            .unwrap();
            let encoded = encoder.encode(&image, 0, true).unwrap();
            let mut decoder = openuuyc_h264::stream::Decoder::new();
            let outputs = decoder.submit(&encoded[0].data, 0).unwrap();
            let outputs = if outputs.is_empty() {
                decoder.finish().unwrap()
            } else {
                outputs
            };
            let mut nv12 = Vec::new();
            outputs[0].picture.pack_into(&mut nv12).unwrap();
            let luma = (width * height) as usize;
            let (y, u, v) = (nv12[luma / 2 + 128], nv12[luma + 1000], nv12[luma + 1001]);
            println!("{name}: Y {y} U {u} V {v}   BT.601 {bt601:?}   BT.709 {bt709:?}");
            let near = |a: u8, b: i32| (i32::from(a) - b).abs() <= 3;
            assert!(
                near(y, bt601.0) && near(u, bt601.1) && near(v, bt601.2),
                "{name} is not BT.601 limited"
            );
        }
    }
}

#[cfg(test)]
mod format_tests {
    use super::*;
    use crate::media::encoding::{Backend, Codec, QualityTarget};

    /// Needs an NVIDIA GPU: `cargo test -- --ignored nvenc_encodes_every_format`.
    /// Every format the host probe would try on NVENC (the same two gates)
    /// must open on a GPU that has it and come out as a stream whose sequence
    /// header names that format and size; where the controller's software
    /// decoder takes the format, it must decode every frame. With
    /// `OPENUUYC_NVENC_DUMP=<dir>` the streams are written there (`.h264`,
    /// `.h265`, `.obu`) for an outside decoder to check.
    #[test]
    #[ignore]
    fn nvenc_encodes_every_format_it_accepts() {
        let context = super::super::cuda::display_context().unwrap();
        let (width, height) = (1280u32, 720u32);
        let rate = Rate {
            target: 8_000_000,
            peak: 8_000_000,
            fps: 30,
            quality: 4,
            quality_target: QualityTarget {
                bitrate: 8_000_000,
                fps: 30,
            },
        };
        let dump = std::env::var_os("OPENUUYC_NVENC_DUMP").map(std::path::PathBuf::from);
        let mut produced = Vec::new();
        for codec in [Codec::H264, Codec::H265, Codec::Av1] {
            for chroma in [1u8, 3] {
                let format = Format {
                    codec,
                    chroma,
                    depth: 8,
                };
                if !(Backend::Nvidia.accepts(format) && Encoder::accepts(format)) {
                    println!("{format:?}: never requested from NVENC");
                    continue;
                }
                let mut encoder = match Encoder::new(&context, (width, height), format, rate) {
                    Ok(encoder) => encoder,
                    Err(error) => {
                        println!("{format:?}: not offered by this GPU: {error:#}");
                        continue;
                    }
                };
                let started = std::time::Instant::now();
                let mut stream = Vec::new();
                let mut packets = Vec::new();
                for index in 0..60u32 {
                    // A moving pattern, so inter frames carry real motion.
                    let mut pixels = Vec::with_capacity((width * height * 4) as usize);
                    for y in 0..height {
                        for x in 0..width {
                            pixels.extend_from_slice(&[
                                (x + index * 8) as u8,
                                (y + index * 4) as u8,
                                (x ^ y) as u8,
                                255,
                            ]);
                        }
                    }
                    let image = Image::Cpu {
                        pixels: Arc::new(pixels),
                        width,
                        height,
                    };
                    for encoded in encoder
                        .encode(&image, i64::from(index) * 333_333, index == 0)
                        .unwrap()
                    {
                        stream.extend_from_slice(&encoded.data);
                        packets.push(encoded.data);
                    }
                }
                let elapsed = started.elapsed();
                let header =
                    crate::media::video_format::parse_stream_format(codec.media(), &stream)
                        .unwrap_or_else(|| panic!("{format:?}: no sequence header"));
                assert_eq!(
                    (
                        header.chroma_format_idc,
                        header.bit_depth_luma,
                        header.bit_depth_chroma,
                        header.visible_width,
                        header.visible_height,
                    ),
                    (chroma, 8, 8, width, height),
                    "{format:?}: the stream says otherwise"
                );
                println!(
                    "{format:?}: 60 frames, {} KiB, {elapsed:?} with uploads",
                    stream.len() / 1024
                );
                if format.software().can_decode() {
                    let mut decoder =
                        openuuyc_codec::decoder::Decoder::new(codec.media(), &[]).unwrap();
                    let mut decoded = 0;
                    for (token, packet) in packets.iter().enumerate() {
                        for frame in decoder.push(packet, token as i64).unwrap() {
                            assert_eq!((frame.width, frame.height), (width, height));
                            decoded += 1;
                        }
                    }
                    assert_eq!(decoded, 60, "{format:?}: the software decoder lost frames");
                    println!("{format:?}: software decoder returned all 60 frames");
                }
                if let Some(directory) = &dump {
                    let extension = match codec {
                        Codec::H264 => "h264",
                        Codec::H265 => "h265",
                        Codec::Av1 => "obu",
                    };
                    std::fs::write(
                        directory.join(format!("{codec:?}-{chroma}.{extension}")),
                        &stream,
                    )
                    .unwrap();
                }
                produced.push(format);
            }
        }
        println!("produced {produced:?}");
        assert!(produced.contains(&Format::AVC));
    }
}

#[cfg(test)]
mod memory_tests {
    /// Stage by stage, so an outside `nvidia-smi` can attribute video memory.
    #[test]
    #[ignore]
    fn video_memory_by_stage() {
        let pause = |stage: &str| {
            println!("STAGE {stage}");
            std::thread::sleep(std::time::Duration::from_secs(4));
        };
        pause("start");
        let context = super::super::cuda::display_context().unwrap();
        pause("cuda-context");
        let (w, h) = (2560, 1440);
        let buffer = super::super::cuda::Buffer::new(&context, w, h).unwrap();
        pause("cuda-buffer");
        let rate = crate::media::encoding::Rate {
            target: 20_000_000,
            peak: 20_000_000,
            fps: 60,
            quality: 4,
            quality_target: crate::media::encoding::QualityTarget {
                bitrate: 20_000_000,
                fps: 60,
            },
        };
        let encoder =
            super::Encoder::new(&context, (w, h), crate::media::encoding::Format::AVC, rate)
                .unwrap();
        pause("nvenc-h264-1440p");
        let screens = super::super::capture::screens().unwrap();
        let desktop = super::super::capture::Desktop::open_selected(&screens[0]).unwrap();
        println!("backend {}", desktop.backend_name());
        pause("nvfbc");
        drop((desktop, encoder, buffer));
        pause("dropped");
    }
}
