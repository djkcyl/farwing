//! Read a cropped, pitched VA surface back into packed NV12.
//!
//! This is the fallback for drivers that cannot export the surface as a dmabuf
//! or for renderers that cannot import one. When both sides support it, the
//! picture stays on the GPU and never takes this path.
use cros_libva::{Image, VAImage};

use crate::media::decode_api::DecodeError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct VisibleRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

pub(super) fn pack_nv12(image: &Image<'_>, rect: VisibleRect) -> Result<Vec<u8>, DecodeError> {
    pack_planes(image.as_ref(), image.image(), rect)
}

fn pack_planes(bytes: &[u8], info: &VAImage, rect: VisibleRect) -> Result<Vec<u8>, DecodeError> {
    let invalid = DecodeError::HardwareFailure;
    if info.format.fourcc != cros_libva::VA_FOURCC_NV12
        || rect.width == 0
        || rect.height == 0
        || (rect.x | rect.y) & 1 != 0
        || rect
            .x
            .checked_add(rect.width)
            .is_none_or(|end| end > u32::from(info.width))
        || rect
            .y
            .checked_add(rect.height)
            .is_none_or(|end| end > u32::from(info.height))
    {
        return Err(invalid);
    }
    let (x, y, width, height) = (
        rect.x as usize,
        rect.y as usize,
        rect.width as usize,
        rect.height as usize,
    );
    let chroma_width = width.div_ceil(2) * 2;
    let chroma_height = height.div_ceil(2);
    let luma_bytes = width.checked_mul(height).ok_or(invalid.clone())?;
    let total = chroma_width
        .checked_mul(chroma_height)
        .and_then(|n| luma_bytes.checked_add(n))
        .ok_or(invalid.clone())?;

    // Validate both planes before allocating or copying. Chroma rows are
    // rounded to complete UV pairs, independently of an odd visible width.
    let region = |plane: usize, first_row: usize, rows: usize, row_width: usize| {
        let pitch = info.pitches[plane] as usize;
        if pitch < x.checked_add(row_width).ok_or(invalid.clone())? {
            return Err(invalid.clone());
        }
        let start = first_row
            .checked_mul(pitch)
            .and_then(|n| n.checked_add(info.offsets[plane] as usize))
            .and_then(|n| n.checked_add(x))
            .ok_or(invalid.clone())?;
        let end = (rows - 1)
            .checked_mul(pitch)
            .and_then(|n| n.checked_add(start))
            .and_then(|n| n.checked_add(row_width))
            .ok_or(invalid.clone())?;
        if end > bytes.len() {
            return Err(invalid.clone());
        }
        Ok((start, pitch))
    };
    let (luma_start, luma_pitch) = region(0, y, height, width)?;
    let (chroma_start, chroma_pitch) = region(1, y / 2, chroma_height, chroma_width)?;
    let mut packed = vec![0; total];
    for row in 0..height {
        let source = luma_start + row * luma_pitch;
        packed[row * width..(row + 1) * width].copy_from_slice(&bytes[source..source + width]);
    }
    for row in 0..chroma_height {
        let source = chroma_start + row * chroma_pitch;
        let target = luma_bytes + row * chroma_width;
        packed[target..target + chroma_width]
            .copy_from_slice(&bytes[source..source + chroma_width]);
    }
    Ok(packed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(width: u16, height: u16) -> VAImage {
        let mut info = VAImage::default();
        info.format.fourcc = cros_libva::VA_FOURCC_NV12;
        info.width = width;
        info.height = height;
        info.pitches = [8, 8, 0];
        info.offsets = [5, 45, 0];
        info
    }

    #[test]
    fn cropped_pitched_nv12_reads_both_plane_origins() {
        let info = image(6, 4);
        let mut data = vec![0xee; 64];
        data[23..25].copy_from_slice(&[22, 23]);
        data[31..33].copy_from_slice(&[32, 33]);
        data[55..57].copy_from_slice(&[122, 123]);
        let rect = VisibleRect {
            x: 2,
            y: 2,
            width: 2,
            height: 2,
        };
        assert_eq!(
            pack_planes(&data, &info, rect).unwrap(),
            [22, 23, 32, 33, 122, 123]
        );
        assert!(pack_planes(&data[..56], &info, rect).is_err());
    }

    #[test]
    fn odd_visible_size_keeps_complete_chroma_pairs() {
        let info = image(4, 4);
        let mut data = vec![0; 64];
        data[45..49].copy_from_slice(&[1, 2, 3, 4]);
        data[53..57].copy_from_slice(&[5, 6, 7, 8]);
        let rect = VisibleRect {
            x: 0,
            y: 0,
            width: 3,
            height: 3,
        };
        let packed = pack_planes(&data, &info, rect).unwrap();
        assert_eq!(packed.len(), 17);
        assert_eq!(&packed[9..], [1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn invalid_crop_and_stride_are_rejected() {
        let mut info = image(4, 4);
        for rect in [
            VisibleRect {
                x: 0,
                y: 0,
                width: 0,
                height: 2,
            },
            VisibleRect {
                x: 1,
                y: 0,
                width: 2,
                height: 2,
            },
            VisibleRect {
                x: 2,
                y: 2,
                width: 4,
                height: 2,
            },
            VisibleRect {
                x: u32::MAX,
                y: 0,
                width: 2,
                height: 2,
            },
        ] {
            assert!(pack_planes(&[0; 64], &info, rect).is_err());
        }
        info.pitches[1] = 2;
        assert!(
            pack_planes(
                &[0; 64],
                &info,
                VisibleRect {
                    x: 0,
                    y: 0,
                    width: 3,
                    height: 3
                }
            )
            .is_err()
        );
    }
}
