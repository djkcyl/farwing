// SPDX-License-Identifier: LGPL-2.1-or-later
// FFmpeg h264data.c scan orders, transposed to the inverse-transform layout.
// These are immutable codec tables, never accepted from the bitstream.
pub(crate) struct Scan {
    positions: &'static [u8],
    extent: usize,
}
impl Scan {
    const fn new(positions: &'static [u8], extent: usize) -> Self {
        assert!(matches!(positions.len(), 4 | 15 | 16 | 64));
        let mut i = 0;
        while i < positions.len() {
            assert!((positions[i] as usize) < extent);
            let mut j = 0;
            while j < i {
                assert!(positions[i] != positions[j]);
                j += 1;
            }
            i += 1;
        }
        Self { positions, extent }
    }
    pub fn positions(&self) -> &'static [u8] {
        self.positions
    }
    pub fn fits(&self, dst: &[i16], qmul: Option<&[i32]>) -> bool {
        dst.len() >= self.extent && qmul.is_none_or(|q| q.len() >= self.extent)
    }
}
pub(crate) static FOUR: Scan =
    Scan::new(&[0, 4, 1, 2, 5, 8, 12, 9, 6, 3, 7, 10, 13, 14, 11, 15], 16);
pub(crate) static FOUR_AC: Scan =
    Scan::new(&[4, 1, 2, 5, 8, 12, 9, 6, 3, 7, 10, 13, 14, 11, 15], 16);
pub(crate) static DC_LUMA: Scan =
    Scan::new(&[0, 1, 4, 8, 5, 2, 3, 6, 9, 12, 13, 10, 7, 11, 14, 15], 16);
pub(crate) static DC_CHROMA: Scan = Scan::new(&[0, 1, 2, 3], 4);
pub(crate) static EIGHT: Scan = Scan::new(
    &[
        0, 8, 1, 2, 9, 16, 24, 17, 10, 3, 4, 11, 18, 25, 32, 40, 33, 26, 19, 12, 5, 6, 13, 20, 27,
        34, 41, 48, 56, 49, 42, 35, 28, 21, 14, 7, 15, 22, 29, 36, 43, 50, 57, 58, 51, 44, 37, 30,
        23, 31, 38, 45, 52, 59, 60, 53, 46, 39, 47, 54, 61, 62, 55, 63,
    ],
    64,
);
pub(crate) static EIGHT_CAVLC: [Scan; 4] = [
    Scan::new(
        &[0, 9, 10, 18, 33, 5, 27, 56, 28, 15, 43, 51, 23, 52, 46, 61],
        64,
    ),
    Scan::new(
        &[8, 16, 3, 25, 26, 6, 34, 49, 21, 22, 50, 44, 31, 59, 39, 62],
        64,
    ),
    Scan::new(
        &[1, 24, 4, 32, 19, 13, 41, 42, 14, 29, 57, 37, 38, 60, 47, 55],
        64,
    ),
    Scan::new(
        &[2, 17, 11, 40, 12, 20, 48, 35, 7, 36, 58, 30, 45, 53, 54, 63],
        64,
    ),
];
