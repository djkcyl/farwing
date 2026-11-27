use std::hash::{DefaultHasher, Hasher};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use bytes::Bytes;
use farwing_codec::PixelFormat;
use farwing_codec::decoder::{Decoder, Error};
use farwing_protocol::session;

use super::{DecodedBatch, DecodedFrame, DecoderOutputIssue};
use crate::media::decode_api::{DecodeError, DecoderNotification};
use crate::media::selection::{DecoderPreference, ProcessingMode};
use crate::media::video_color::{ColorMatrix, RenderColor};
use crate::media::video_format::{VideoFormatSignature, parse_stream_format};
use crate::media::{CodecPreference, ConnectionMediaProfile, VideoCodec};
use crate::transport::rtc::EncodedVideoFrame;

/// Owning software-codec output. The visible picture starts in the upper
/// left, but AV1 can have larger storage strides and an aligned height.
#[derive(Debug)]
pub(crate) struct CpuVideoFrame {
    width: u32,
    height: u32,
    coded_width: u32,
    coded_height: u32,
    pixels: CpuPixels,
}

#[derive(Debug)]
enum CpuPixels {
    Nv12(Bytes),
    /// The shared codec's packed AYUV uses V, U, Y, A byte order.
    Vuya(Bytes),
}

impl CpuVideoFrame {
    pub(crate) const fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn nv12_planes<'a>(&self, data: &'a [u8]) -> (&'a [u8], &'a [u8]) {
        data.split_at(self.coded_width as usize * self.coded_height as usize)
    }

    pub(crate) fn rgba(&self, color: RenderColor) -> Result<Vec<u8>> {
        let (width, height) = self.size();
        let range = if color.full_range {
            yuv::YuvRange::Full
        } else {
            yuv::YuvRange::Limited
        };
        let matrix = match color.matrix {
            ColorMatrix::Bt601 => yuv::YuvStandardMatrix::Bt601,
            ColorMatrix::Bt709 => yuv::YuvStandardMatrix::Bt709,
            ColorMatrix::Bt2020 => yuv::YuvStandardMatrix::Bt2020,
        };
        let mut pixels = vec![0; width as usize * height as usize * 4];
        match &self.pixels {
            CpuPixels::Nv12(data) => {
                let (y, uv) = self.nv12_planes(data);
                // The converter derives a final odd row from the plane
                // lengths, so keep coded strides but exclude bottom padding.
                let stride = self.coded_width as usize;
                yuv::yuv_nv12_to_rgba(
                    &yuv::YuvBiPlanarImage {
                        y_plane: &y[..stride * height as usize],
                        y_stride: self.coded_width,
                        uv_plane: &uv[..stride * height.div_ceil(2) as usize],
                        uv_stride: self.coded_width,
                        width,
                        height,
                    },
                    &mut pixels,
                    width * 4,
                    range,
                    matrix,
                    yuv::YuvConversionMode::Balanced,
                )?;
            }
            CpuPixels::Vuya(data) => {
                yuv::vyua_to_rgba(
                    &yuv::YuvPackedImage {
                        yuy: data,
                        yuy_stride: self.coded_width * 4,
                        width,
                        height,
                    },
                    &mut pixels,
                    width * 4,
                    range,
                    matrix,
                    false,
                )?;
            }
        }
        Ok(pixels)
    }

    pub(crate) fn luma_hash(&self, width: u32, height: u32) -> Result<u64> {
        ensure!(
            self.size() == (width, height),
            "visible area outside the frame"
        );
        let mut hasher = DefaultHasher::new();
        let (width, height) = (width as usize, height as usize);
        let stride = self.coded_width as usize;
        match &self.pixels {
            CpuPixels::Nv12(data) => {
                let (y, _) = self.nv12_planes(data);
                for row in y.chunks_exact(stride).take(height) {
                    hasher.write(&row[..width]);
                }
            }
            CpuPixels::Vuya(data) => {
                let mut luma = vec![0; width];
                for row in data.chunks_exact(stride * 4).take(height) {
                    for (y, pixel) in luma.iter_mut().zip(row.chunks_exact(4)) {
                        *y = pixel[2];
                    }
                    hasher.write(&luma);
                }
            }
        }
        Ok(hasher.finish())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DecoderCandidate {
    Software,
}

impl DecoderCandidate {
    pub(crate) fn preferred(codec: VideoCodec, preference: DecoderPreference) -> Vec<Self> {
        if supports_software_format(codec, 1, 8) && preference.mode.software() {
            vec![Self::Software]
        } else {
            Vec::new()
        }
    }

    pub(crate) fn supports_format(self, codec: VideoCodec, format: VideoFormatSignature) -> bool {
        format.bit_depth_luma == format.bit_depth_chroma
            && supports_software_format(codec, format.chroma_format_idc, format.bit_depth_luma)
    }
}

const SOFTWARE_CODECS: [(VideoCodec, session::Codec); 2] = [
    (VideoCodec::H264, session::Codec::H264),
    (VideoCodec::Av1, session::Codec::Av1),
];

fn supports_software_format(codec: VideoCodec, chroma: u8, depth: u8) -> bool {
    // Linux presents 8-bit SDR. The host treats a bit10 decoder offer as
    // eligible for HDR, so do not offer the shared AV1 core's 10-bit output.
    depth == 8
        && farwing_codec::Format {
            codec,
            chroma,
            depth,
        }
        .can_decode()
}

/// Codec choices with a Linux backend. Device menus keep preferences only;
/// global settings may also restrict negotiation to one codec.
pub(crate) fn codec_preferences() -> impl Iterator<Item = CodecPreference> {
    CodecPreference::PREFERRED
        .into_iter()
        .chain(CodecPreference::ONLY)
        .filter(|choice| {
            choice.preferred().is_none_or(|chosen| {
                SOFTWARE_CODECS.iter().any(|&(codec, offered)| {
                    offered == chosen && supports_software_format(codec, 1, 8)
                })
            })
        })
}

pub(crate) struct NativeVideoDecoder {
    decoder: Decoder,
}

impl NativeVideoDecoder {
    pub(crate) fn open(
        codec: VideoCodec,
        width: u32,
        height: u32,
        frame_rate: u32,
        preference: DecoderPreference,
        extra_data: Bytes,
    ) -> Result<Self> {
        let candidate = DecoderCandidate::preferred(codec, preference)
            .into_iter()
            .next()
            .ok_or(DecodeError::NoBackend)?;
        Self::open_candidate(candidate, codec, width, height, frame_rate, extra_data)
    }

    pub(crate) fn open_candidate(
        candidate: DecoderCandidate,
        codec: VideoCodec,
        width: u32,
        height: u32,
        frame_rate: u32,
        extra_data: Bytes,
    ) -> Result<Self> {
        ensure!(
            width != 0 && height != 0 && frame_rate != 0,
            DecodeError::InvalidInput
        );
        ensure!(
            supports_software_format(codec, 1, 8)
                && parse_stream_format(codec, &extra_data)
                    .is_none_or(|format| candidate.supports_format(codec, format)),
            DecodeError::Unsupported
        );
        let decoder = Decoder::new(codec, &extra_data).map_err(software_error)?;
        Ok(Self { decoder })
    }

    pub(crate) const fn candidate(&self) -> DecoderCandidate {
        DecoderCandidate::Software
    }

    pub(crate) fn label(&self) -> &str {
        crate::i18n::t().session.decoder_software
    }

    pub(crate) const fn is_software(&self) -> bool {
        true
    }

    pub(crate) fn set_notification(&mut self, notification: DecoderNotification) {
        self.decoder
            .set_cancellation(notification.shared_cancellation());
    }

    pub(crate) fn reset_for_keyframe(&mut self) -> Result<()> {
        self.decoder
            .reset()
            .map_err(software_error)
            .context("reset software decoder for keyframe cutover")
    }

    pub(crate) fn push(&mut self, frame: EncodedVideoFrame, decode_token: i64) -> DecodedBatch {
        self.push_packet(&frame.data, decode_token)
    }

    fn push_packet(&mut self, data: &[u8], decode_token: i64) -> DecodedBatch {
        let mut batch = DecodedBatch::default();
        match self
            .decoder
            .push(data, decode_token)
            .map_err(software_error)
        {
            // Submission is synchronous and low-delay: no picture for this
            // token (e.g. an AV1 sequence header alone) will appear in poll.
            Ok(frames) if frames.is_empty() => {
                batch
                    .output_issues
                    .push(DecoderOutputIssue::Dropped(decode_token));
            }
            Ok(frames) => {
                for frame in frames {
                    let pixels = match frame.format {
                        PixelFormat::Nv12 => CpuPixels::Nv12(frame.data),
                        PixelFormat::Ayuv => CpuPixels::Vuya(frame.data),
                        _ => {
                            batch.output_issues.push(DecoderOutputIssue::Failed {
                                token: Some(frame.pts),
                                error: DecodeError::Unsupported.into(),
                            });
                            continue;
                        }
                    };
                    batch.frames.push(DecodedFrame {
                        pts: frame.pts,
                        width: frame.width,
                        height: frame.height,
                        ready_at: Instant::now(),
                        surface: CpuVideoFrame {
                            width: frame.width,
                            height: frame.height,
                            coded_width: frame.coded_width,
                            coded_height: frame.coded_height,
                            pixels,
                        },
                    });
                }
            }
            Err(error) => {
                batch.input_error = Some(
                    anyhow::Error::new(error)
                        .context("submit encoded video frame to native decoder"),
                );
            }
        }
        batch
    }

    /// The software core returns output synchronously with submission.
    pub(crate) fn poll(&mut self) -> DecodedBatch {
        DecodedBatch::default()
    }
}

fn software_error(error: Error) -> DecodeError {
    match error {
        Error::Closed => DecodeError::Closed,
        Error::Unsupported => DecodeError::Unsupported,
        Error::Allocation => DecodeError::Backend,
        Error::InvalidInput => DecodeError::InvalidInput,
        Error::NeedKeyframe => DecodeError::NeedKeyframe,
    }
}

pub(crate) fn prepare_native_decoder_support(_mode: ProcessingMode, _delay: Duration) {
    // No device probing is needed for the built-in software decoder.
}

pub(crate) fn detect_native_decoder_support(
    profile: ConnectionMediaProfile,
) -> Result<Vec<session::Decoder>> {
    let mut capabilities = Vec::new();
    if profile.decoder.mode.software() {
        for (codec, wire) in SOFTWARE_CODECS {
            if !profile.codec.accepts(codec) {
                continue;
            }
            for chroma in [1, 3] {
                if supports_software_format(codec, chroma, 8) {
                    capabilities.push(session::Decoder {
                        codec: wire,
                        chroma444: chroma == 3,
                        bit10: false,
                        // Match Farwing's software decoder offer ceiling, not
                        // the storage geometry of any individual picture.
                        width: 1920,
                        height: 1080,
                        hardware: false,
                        av1_visible_size: codec == VideoCodec::Av1,
                    });
                }
            }
        }
    }
    if capabilities.is_empty() {
        bail!("no decoder supports the selected codec and decoding mode");
    }
    Ok(capabilities)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::decoder_pool::DecoderPool;
    use crate::media::decoder_result::VideoDecodeResult;
    use crate::media::{ConnectionMediaOptions, LocalDisplayInfo};
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    fn take_picture(batch: DecodedBatch, expected_token: i64) -> DecodedFrame {
        assert!(batch.input_error.is_none(), "{:?}", batch.input_error);
        assert!(batch.output_issues.is_empty());
        let [frame] = batch.frames.try_into().expect("one picture per AU");
        assert_eq!(frame.pts, expected_token);
        frame
    }

    fn assert_rgba_pixel(rgba: &[u8], width: usize, x: usize, y: usize, expected: [u8; 4]) {
        let at = (y * width + x) * 4;
        let actual = &rgba[at..at + 4];
        assert_eq!(actual[3], expected[3]);
        // Allow minor rounding differences between yuv's kernels and FFmpeg.
        assert!(
            actual[..3]
                .iter()
                .zip(expected)
                .all(|(actual, expected)| actual.abs_diff(expected) <= 3),
            "pixel ({x},{y}): {actual:?}, expected {expected:?}"
        );
    }

    #[test]
    fn software_h264_offer_matches_cropped_nv12_and_vuya_output() {
        let options = ConnectionMediaOptions {
            codec: CodecPreference::H264,
            ..ConnectionMediaOptions::default()
        };
        let profile = options.resolve(LocalDisplayInfo::FALLBACK).unwrap();
        let caps = detect_native_decoder_support(profile).unwrap();
        let nv12 = session::Decoder {
            codec: session::Codec::H264,
            chroma444: false,
            bit10: false,
            width: 1920,
            height: 1080,
            hardware: false,
            av1_visible_size: false,
        };
        assert_eq!(caps, [nv12, session::Decoder { chroma444: true, ..nv12 }]);
        assert!(
            detect_native_decoder_support(ConnectionMediaProfile {
                codec: CodecPreference::H265,
                ..profile
            })
            .is_err()
        );
        assert!(
            detect_native_decoder_support(ConnectionMediaProfile {
                decoder: DecoderPreference {
                    mode: ProcessingMode::Hardware,
                    ..profile.decoder
                },
                ..profile
            })
            .is_err()
        );

        let au = include_bytes!("fixtures/matrix/h264_1920x1080.annexb");
        let format = parse_stream_format(VideoCodec::H264, au).unwrap();
        assert_eq!((format.coded_width, format.coded_height), (1920, 1088));
        let mut decoder = NativeVideoDecoder::open(
            VideoCodec::H264,
            1920,
            1080,
            60,
            profile.decoder,
            Bytes::new(),
        )
        .unwrap();
        let frame = take_picture(decoder.push_packet(au, 0x4000_0000_0001), 0x4000_0000_0001);
        assert_eq!((frame.width, frame.height), (1920, 1080));
        assert_eq!(frame.surface.size(), (1920, 1080));
        let CpuPixels::Nv12(data) = &frame.surface.pixels else {
            panic!("H.264 4:2:0 must remain NV12");
        };
        let (y, uv) = frame.surface.nv12_planes(data);
        assert_eq!(y.len(), 2_073_600);
        assert_eq!(uv.len(), 1_036_800);
        // FFmpeg's NV12 output has an inset light rectangle with flat
        // chroma. Check both inside and outside at the cropped stride.
        assert_eq!([y[0], y[539 * 1920 + 959], y[2_073_599]], [64, 192, 64]);
        assert!(uv.chunks_exact(2).all(|pair| pair == [96, 160]));
        assert!(decoder.poll().frames.is_empty());

        let au = include_bytes!("fixtures/matrix/h264_444_1280x720.annexb");
        let format = parse_stream_format(VideoCodec::H264, au).unwrap();
        assert_eq!(format.chroma_format_idc, 3);
        assert!(
            decoder
                .candidate()
                .supports_format(VideoCodec::H264, format)
        );
        let mut decoder =
            NativeVideoDecoder::open(VideoCodec::H264, 1280, 720, 60, profile.decoder, Bytes::new())
                .unwrap();
        let frame = take_picture(decoder.push_packet(au, 0x4000_0000_0002), 0x4000_0000_0002);
        assert_eq!(frame.surface.size(), (1280, 720));
        let CpuPixels::Vuya(data) = &frame.surface.pixels else {
            panic!("H.264 4:4:4 must preserve full chroma");
        };
        assert_eq!(&data[..4], &[160, 96, 64, 255]);
        let rgba = frame.surface.rgba(RenderColor::default()).unwrap();
        assert_rgba_pixel(&rgba, 1280, 0, 0, [107, 42, 0, 255]);
        assert_rgba_pixel(&rgba, 1280, 639, 359, [255, 191, 140, 255]);
    }

    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    #[test]
    fn software_av1_groups_rtp_and_presents_visible_size_after_reset() {
        use crate::media::av1;
        use farwing_codec::syntax::av1::visible;

        let options = ConnectionMediaOptions {
            codec: CodecPreference::Av1,
            ..ConnectionMediaOptions::default()
        };
        let profile = options.resolve(LocalDisplayInfo::FALLBACK).unwrap();
        let nv12 = session::Decoder {
            codec: session::Codec::Av1,
            chroma444: false,
            bit10: false,
            width: 1920,
            height: 1080,
            hardware: false,
            av1_visible_size: true,
        };
        assert_eq!(
            detect_native_decoder_support(profile).unwrap(),
            [nv12, session::Decoder { chroma444: true, ..nv12 }]
        );
        let sdr_10_bit = include_bytes!("fixtures/matrix/av1_10_1280x720.obu");
        let result = NativeVideoDecoder::open(
            VideoCodec::Av1,
            1280,
            720,
            60,
            profile.decoder,
            av1::parameters(sdr_10_bit).unwrap(),
        );
        assert_eq!(
            result.err().unwrap().downcast_ref(),
            Some(&DecodeError::Unsupported)
        );

        const VISIBLE: (u32, u32) = (1269, 715);
        let h264 =
            NativeVideoDecoder::open(VideoCodec::H264, 1280, 720, 60, profile.decoder, Bytes::new())
                .unwrap();
        let mut pool = DecoderPool::new(
            h264,
            VideoCodec::H264,
            1280,
            720,
            60,
            profile.decoder,
            Bytes::new(),
        );
        for (fixture, chroma, first_pixel, inner_pixel, last_pixel) in [
            (
                include_bytes!("fixtures/matrix/av1_8_1280x720.obu").as_slice(),
                1,
                [64, 64, 64, 255],
                [192, 192, 192, 255],
                [64, 64, 64, 255],
            ),
            (
                include_bytes!("fixtures/matrix/av1_444_8_1280x720.obu").as_slice(),
                3,
                [13, 109, 0, 255],
                [141, 237, 95, 255],
                [115, 19, 161, 255],
            ),
        ] {
            let payloads =
                av1::payloads(39, &visible::declare(fixture.to_vec(), VISIBLE), true).unwrap();
            let mut records = Vec::new();
            for payload in &payloads {
                records.extend_from_slice(&(payload.len() as u32).to_le_bytes());
                records.extend_from_slice(payload);
            }
            // The last record contains the end of a fragmented picture OBU.
            let last = records.len() - payloads.last().unwrap().len() - 4;
            assert!(av1::assemble(&records[..last]).is_err());
            let assembled = av1::assemble(&records).unwrap();
            let format = parse_stream_format(VideoCodec::Av1, &assembled).unwrap();
            assert_eq!((format.visible_width, format.visible_height), VISIBLE);
            assert_eq!((format.coded_width, format.coded_height), (1280, 720));
            assert_eq!(format.chroma_format_idc, chroma);
            let parameters = av1::parameters(&assembled).unwrap();
            let transition = pool.prepare(VideoCodec::Av1, Some(format), true, parameters.clone());
            assert!(transition.replaced);
            assert_eq!(transition.result, None);
            let decoder = pool.decoder().unwrap();

            let no_picture = decoder.push_packet(&parameters, 0x1_0000_0001);
            assert!(
                no_picture.input_error.is_none(),
                "{:?}", no_picture.input_error
            );
            assert!(no_picture.frames.is_empty());
            assert!(matches!(
                no_picture.output_issues.as_slice(),
                [DecoderOutputIssue::Dropped(0x1_0000_0001)]
            ));
            assert!(decoder.poll().frames.is_empty());

            let first = take_picture(decoder.push_packet(&assembled, 0x1_0000_0002), 0x1_0000_0002);
            let first_hash = first.surface.luma_hash(VISIBLE.0, VISIBLE.1).unwrap();
            assert_eq!((first.width, first.height), VISIBLE);
            let rgba = first.surface.rgba(RenderColor::default()).unwrap();
            assert_eq!(rgba.len(), VISIBLE.0 as usize * VISIBLE.1 as usize * 4);
            assert_rgba_pixel(&rgba, VISIBLE.0 as usize, 0, 0, first_pixel);
            assert_rgba_pixel(&rgba, VISIBLE.0 as usize, 639, 359, inner_pixel);
            assert_rgba_pixel(&rgba, VISIBLE.0 as usize, 1268, 714, last_pixel);

            let picture_obu = av1::units(&assembled)
                .unwrap()
                .map(|unit| unit.unwrap())
                .find(|unit| unit.kind == 6)
                .expect("fixture has a single frame OBU")
                .bytes;
            // Reset must preserve the sequence and visible-size declaration,
            // even when the next keyframe has only a frame OBU.
            decoder.reset_for_keyframe().unwrap();
            let recovered = take_picture(
                decoder.push_packet(picture_obu, 0x2_0000_0001),
                0x2_0000_0001,
            );
            assert_eq!((recovered.width, recovered.height), VISIBLE);
            assert_eq!(
                recovered.surface.luma_hash(VISIBLE.0, VISIBLE.1).unwrap(),
                first_hash
            );
        }
    }

    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    #[test]
    fn software_av1_rejects_multiple_pictures_per_token_and_loses_references_on_reset() {
        // Two 32x32 black pictures encoded with libaom in low-delay mode:
        // `-lag-in-frames 0 -g 120 -f obu`. FFprobe reports I then P.
        const KEY: &[u8] = &[
            0x12, 0x00, 0x0a, 0x0a, 0x00, 0x00, 0x00, 0x02, 0x27, 0xfe, 0x6d, 0x7c, 0x80, 0x20,
            0x32, 0x0e, 0x10, 0x00, 0x90, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00, 0x3e, 0xfd,
            0x81, 0xa0,
        ];
        const DELTA: &[u8] = &[
            0x12, 0x00, 0x32, 0x11, 0x30, 0x03, 0xc0, 0x80, 0x00, 0x00, 0x06, 0xc0, 0x00, 0x00,
            0x02, 0x80, 0x00, 0x20, 0x00, 0x8c, 0x70,
        ];
        let mut decoder = NativeVideoDecoder::open(
            VideoCodec::Av1,
            32,
            32,
            30,
            DecoderPreference::default(),
            Bytes::new(),
        )
        .unwrap();
        take_picture(decoder.push_packet(KEY, 1), 1);
        take_picture(decoder.push_packet(DELTA, 2), 2);

        decoder.reset_for_keyframe().unwrap();
        let missing_key = decoder.push_packet(DELTA, 0x1_0000_0001);
        assert!(missing_key.frames.is_empty());
        assert_eq!(
            VideoDecodeResult::from_error(&missing_key.input_error.unwrap()),
            VideoDecodeResult::RequestKeyframe
        );

        let grouped = decoder.push_packet(&[KEY, DELTA].concat(), 0x1_0000_0002);
        assert!(grouped.frames.is_empty());
        assert_eq!(
            grouped.input_error.unwrap().downcast_ref(),
            Some(&DecodeError::Unsupported)
        );
        take_picture(decoder.push_packet(KEY, 0x1_0000_0003), 0x1_0000_0003);
        take_picture(decoder.push_packet(DELTA, 0x1_0000_0004), 0x1_0000_0004);
    }

    #[test]
    fn resize_and_reset_wait_for_a_keyframe_without_reusing_tokens() {
        const SMALL: &[u8] = include_bytes!("fixtures/h264.annexb");
        const LARGE: &[u8] = include_bytes!("fixtures/h264_resize.annexb");
        // Annex-B SPS/PPS seed each decoder. The first picture uses only the
        // VCL NALs through the next frame's AUD, without in-band parameters.
        let small = &SMALL[666..4647];
        let large = &LARGE[668..7794];
        let large_delta = &LARGE[7794..10915];
        let small_parameters = Bytes::from_static(&SMALL[6..45]);
        let large_parameters = Bytes::from_static(&LARGE[6..46]);
        let small_format = parse_stream_format(VideoCodec::H264, &small_parameters);
        let large_format = parse_stream_format(VideoCodec::H264, &large_parameters);
        let codec = VideoCodec::H264;
        let preference = DecoderPreference::default();
        let decoder =
            NativeVideoDecoder::open(codec, 256, 256, 60, preference, small_parameters.clone())
                .unwrap();
        let mut pool = DecoderPool::new(decoder, codec, 256, 256, 60, preference, small_parameters);
        pool.adopt_format(small_format);
        assert!(
            !pool
                .prepare(codec, small_format, true, Bytes::new())
                .replaced
        );
        let first = take_picture(pool.decoder().unwrap().push_packet(small, 11), 11);
        assert_eq!(first.surface.size(), (256, 256));

        let before_idr = pool.prepare(codec, large_format, false, Bytes::new());
        assert!(!before_idr.replaced);
        assert_eq!(before_idr.result, Some(VideoDecodeResult::RequestKeyframe));
        let on_idr = pool.prepare(codec, large_format, true, large_parameters);
        assert!(on_idr.replaced);
        assert_eq!(on_idr.result, None);
        let decoder = pool.decoder().unwrap();
        let resized = take_picture(decoder.push_packet(large, 0x1_0000_000b), 0x1_0000_000b);
        assert_eq!(resized.surface.size(), (640, 360));
        assert_eq!(first.surface.size(), (256, 256));

        decoder.reset_for_keyframe().unwrap();
        let missing_idr = decoder.push_packet(large_delta, 0x2_0000_000b);
        let error = missing_idr.input_error.expect("a delta needs the IDR");
        assert_eq!(
            VideoDecodeResult::from_error(&error),
            VideoDecodeResult::RequestKeyframe
        );
        assert!(missing_idr.frames.is_empty());
        take_picture(decoder.push_packet(large, 0x2_0000_000c), 0x2_0000_000c);

        let cancelled = Arc::new(AtomicBool::new(false));
        decoder.set_notification(DecoderNotification::new(cancelled.clone()));
        cancelled.store(true, Ordering::Release);
        let batch = decoder.push_packet(large, 0x2_0000_000d);
        let error = batch.input_error.expect("submission is cancelled");
        assert_eq!(error.downcast_ref(), Some(&DecodeError::Closed));
        assert!(batch.frames.is_empty());
    }

    #[test]
    fn unsupported_codec_switch_fails_without_a_fallback_candidate() {
        let codec = VideoCodec::H264;
        let preference = DecoderPreference::default();
        let decoder =
            NativeVideoDecoder::open(codec, 256, 256, 60, preference, Bytes::new()).unwrap();
        let mut pool = DecoderPool::new(decoder, codec, 256, 256, 60, preference, Bytes::new());

        let changed = pool.prepare(VideoCodec::H265, None, true, Bytes::new());
        assert!(changed.replaced);
        assert_eq!(changed.result, Some(VideoDecodeResult::Error));
        assert!(pool.decoder().is_none());
        // Decode failures after the rejected switch must not index an empty
        // candidate list when the fallback threshold is reached.
        for _ in 0..4 {
            let transition = pool.complete(VideoDecodeResult::Uninitialized, false, None);
            assert_eq!(transition.result, Some(VideoDecodeResult::Uninitialized));
        }
        let transition = pool.complete(VideoDecodeResult::Uninitialized, false, None);
        assert_eq!(transition.result, Some(VideoDecodeResult::Error));

        let recovered = pool.prepare(codec, None, true, Bytes::new());
        assert!(recovered.replaced);
        assert_eq!(recovered.result, None);
        assert!(pool.decoder().is_some());
    }
}
