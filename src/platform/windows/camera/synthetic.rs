//! A generated camera for machines without one (`FARWING_DEV_CAMERA=synthetic`):
//! color bands with a square that crosses the picture, at the mode's rate.
use super::{Picture, SYNTHETIC};
use crate::media::camera::{Camera, Matrix, Mode, PixelFormat};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub(super) fn camera() -> Camera {
    let mode = |width, height, fps| Mode {
        width,
        height,
        fps_numerator: fps,
        fps_denominator: 1,
        format: PixelFormat::Nv12,
    };
    Camera {
        id: SYNTHETIC.to_owned(),
        name: crate::i18n::t().camera.synthetic_name.to_owned(),
        modes: vec![
            mode(1920, 1080, 30),
            mode(1280, 720, 60),
            mode(1280, 720, 30),
            mode(640, 480, 30),
        ],
        unavailable: None,
        controls: Vec::new(),
    }
}

pub(super) struct Generator {
    mode: Mode,
    started: Instant,
}

impl Generator {
    pub(super) fn start(mode: Mode) -> Self {
        Self {
            mode,
            started: Instant::now(),
        }
    }

    fn interval(&self) -> Duration {
        Duration::from_secs_f64(
            f64::from(self.mode.fps_denominator.max(1)) / f64::from(self.mode.fps_numerator.max(1)),
        )
    }

    /// The picture after `seen` once it is due, waiting at most `timeout` ms.
    pub(super) fn next(&mut self, seen: u64, timeout: u32) -> Option<(u64, Arc<Picture>)> {
        let interval = self.interval();
        let due = self.started + interval.mul_f64(seen as f64);
        let now = Instant::now();
        if due > now {
            let wait = due - now;
            if wait > Duration::from_millis(timeout.into()) {
                std::thread::sleep(Duration::from_millis(timeout.into()));
                return None;
            }
            std::thread::sleep(wait);
        }
        let frame = (self.started.elapsed().as_secs_f64() / interval.as_secs_f64()) as u64;
        Some((frame + 1, Arc::new(self.picture(frame))))
    }

    fn picture(&self, frame: u64) -> Picture {
        let (width, height) = (self.mode.width as usize, self.mode.height as usize);
        let mut data = vec![0u8; width * height * 3 / 2];
        let square = height / 4;
        let travel = (width - square).max(1);
        let left = (frame as usize * 8) % (travel * 2);
        let left = if left > travel { travel * 2 - left } else { left };
        let top = (height - square) / 2;
        for y in 0..height {
            let row = &mut data[y * width..(y + 1) * width];
            for (x, luma) in row.iter_mut().enumerate() {
                let inside = (left..left + square).contains(&x) && (top..top + square).contains(&y);
                *luma = if inside { 235 } else { 40 + (x * 160 / width) as u8 };
            }
        }
        let chroma = &mut data[width * height..];
        for y in 0..height / 2 {
            for x in 0..width / 2 {
                let band = (x * 2 * 6 / width) as u8;
                chroma[y * width + x * 2] = 64 + band * 24;
                chroma[y * width + x * 2 + 1] = 200 - band * 24;
            }
        }
        Picture {
            data,
            width: self.mode.width,
            height: self.mode.height,
            captured: Instant::now(),
            matrix: Matrix::Bt709,
            full_range: false,
        }
    }
}
