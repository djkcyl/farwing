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
use crate::media::video_format::{VideoFormatSignature, parse_stream_format};
use crate::media::{ConnectionMediaProfile, VideoCodec};
use crate::transport::rtc::EncodedVideoFrame;

/// An owning NV12 picture, cropped to visible size and tightly packed. Color
/// interpretation comes from the Farwing frame's RTP color metadata.
#[derive(Debug)]
pub(crate) struct CpuVideoFrame {
    width: u32,
    height: u32,
    data: Bytes,
}

impl CpuVideoFrame {
    pub(crate) const fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Full-resolution Y, then interleaved U/V at half resolution. Each plane
    /// has a byte stride of `width`.
    pub(crate) fn planes(&self) -> (&[u8], &[u8]) {
        self.data
            .split_at(self.width as usize * self.height as usize)
    }

    pub(crate) fn luma_hash(&self, width: u32, height: u32) -> Result<u64> {
        ensure!(
            self.size() == (width, height),
            "visible area outside the frame"
        );
        let mut hasher = DefaultHasher::new();
        for row in self.planes().0.chunks_exact(width as usize) {
            hasher.write(row);
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
        if codec == VideoCodec::H264 && preference.mode.software() {
            vec![Self::Software]
        } else {
            Vec::new()
        }
    }

    pub(crate) fn supports_format(self, codec: VideoCodec, format: VideoFormatSignature) -> bool {
        codec == VideoCodec::H264
            && format.chroma_format_idc == 1
            && format.bit_depth_luma == 8
            && format.bit_depth_chroma == 8
    }
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
            codec == VideoCodec::H264
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
            Ok(frames) if frames.is_empty() => {
                batch
                    .output_issues
                    .push(DecoderOutputIssue::Dropped(decode_token));
            }
            Ok(frames) => {
                for frame in frames {
                    if frame.format != PixelFormat::Nv12 {
                        batch.output_issues.push(DecoderOutputIssue::Failed {
                            token: Some(frame.pts),
                            error: DecodeError::Unsupported.into(),
                        });
                        continue;
                    }
                    batch.frames.push(DecodedFrame {
                        pts: frame.pts,
                        width: frame.width,
                        height: frame.height,
                        ready_at: Instant::now(),
                        surface: CpuVideoFrame {
                            width: frame.width,
                            height: frame.height,
                            data: frame.data,
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
    if !profile.decoder.mode.software() || !profile.codec.accepts(VideoCodec::H264) {
        bail!("no decoder supports the selected codec and decoding mode");
    }
    Ok(vec![session::Decoder {
        codec: session::Codec::H264,
        chroma444: false,
        bit10: false,
        // Match Farwing's software H.264 offer ceiling. The decoder itself
        // uses the actual SPS and does not allocate based on this offer size.
        width: 1920,
        height: 1080,
        hardware: false,
        av1_visible_size: false,
    }])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::decoder_pool::DecoderPool;
    use crate::media::decoder_result::VideoDecodeResult;
    use crate::media::{CodecPreference, ConnectionMediaOptions, LocalDisplayInfo};
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

    #[test]
    fn software_h264_offer_matches_cropped_nv12_output() {
        let options = ConnectionMediaOptions::default();
        let profile = options.resolve(LocalDisplayInfo::FALLBACK).unwrap();
        let caps = detect_native_decoder_support(profile).unwrap();
        assert_eq!(
            caps,
            [session::Decoder {
                codec: session::Codec::H264,
                chroma444: false,
                bit10: false,
                width: 1920,
                height: 1080,
                hardware: false,
                av1_visible_size: false,
            }]
        );
        assert!(
            detect_native_decoder_support(ConnectionMediaProfile {
                codec: CodecPreference::Av1,
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
        let (y, uv) = frame.surface.planes();
        assert_eq!(y.len(), 2_073_600);
        assert_eq!(uv.len(), 1_036_800);
        // FFmpeg's NV12 output has an inset light rectangle with flat
        // chroma. Check both inside and outside at the cropped stride.
        assert_eq!([y[0], y[539 * 1920 + 959], y[2_073_599]], [64, 192, 64]);
        assert!(uv.chunks_exact(2).all(|pair| pair == [96, 160]));
        assert!(decoder.poll().frames.is_empty());
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
