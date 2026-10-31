//! `dev camera`: this machine's cameras with their modes and adjustments,
//! and a capture through the encoder pipeline's frame source that reports
//! what arrived (`docs/protocol/camera-design.md` §10, C1).
use crate::media::camera::{Camera, Mode, PixelFormat, Spec};
use crate::platform::camera;
use anyhow::{Context, Result};
use std::time::{Duration, Instant};

/// `dev camera list`
pub fn list() -> Result<()> {
    let cameras = camera::list()?;
    let described: Vec<_> = cameras
        .iter()
        .map(|c| {
            let (default, controls) = camera::describe(c);
            serde_json::json!({
                "id": c.id,
                "name": c.name,
                "default": default,
                "modes": c.modes,
                "unavailable": c.unavailable,
                "controls": controls,
            })
        })
        .collect();
    println!("{}", serde_json::to_string_pretty(&described)?);
    Ok(())
}

/// `dev camera adjust`: set one picture adjustment and print them all.
pub fn adjust(wanted: Option<&str>, control: &str, value: i32, auto: bool) -> Result<()> {
    let camera = find(camera::list()?, wanted)?;
    let control: crate::media::camera::Control =
        serde_json::from_value(serde_json::Value::String(control.to_owned()))
            .context("--control: brightness, contrast, ..., zoom")?;
    camera::set_control(&camera.id, control, value, auto)?;
    println!("{}", serde_json::to_string_pretty(&camera::controls(&camera.id)?)?);
    Ok(())
}

/// A camera by its position in `dev camera list` or a part of its ID or name.
fn find(cameras: Vec<Camera>, wanted: Option<&str>) -> Result<Camera> {
    let Some(wanted) = wanted else {
        return cameras.into_iter().next().context(crate::i18n::t().camera.not_found);
    };
    if let Ok(index) = wanted.parse::<usize>() {
        return cameras.into_iter().nth(index).context(crate::i18n::t().camera.not_found);
    }
    let wanted = wanted.to_lowercase();
    cameras
        .into_iter()
        .find(|c| c.id.to_lowercase().contains(&wanted) || c.name.to_lowercase().contains(&wanted))
        .context(crate::i18n::t().camera.not_found)
}

/// `WxH@fps` with an optional `:nv12|yuy2|mjpeg|h264`.
fn parse_mode(camera: &Camera, text: &str) -> Result<Mode> {
    let (size, format) = text.split_once(':').map_or((text, None), |(s, f)| (s, Some(f)));
    let (size, fps) = size.split_once('@').context("--mode: WxH@fps")?;
    let (width, height) = size.split_once('x').context("--mode: WxH@fps")?;
    let (width, height, fps): (u32, u32, u32) = (width.parse()?, height.parse()?, fps.parse()?);
    let format = format
        .map(|f| match f.to_ascii_lowercase().as_str() {
            "nv12" => Ok(PixelFormat::Nv12),
            "yuy2" => Ok(PixelFormat::Yuy2),
            "mjpeg" | "mjpg" => Ok(PixelFormat::Mjpeg),
            "h264" => Ok(PixelFormat::H264),
            other => anyhow::bail!("--mode: unknown format {other}"),
        })
        .transpose()?;
    camera
        .modes
        .iter()
        .filter(|m| (m.width, m.height) == (width, height) && m.fps() == fps)
        .filter(|m| format.is_none_or(|f| m.format == f))
        .min_by_key(|m| m.format)
        .copied()
        .context(crate::i18n::t().camera.no_mode)
}

/// `dev camera probe`: capture for `seconds` and report the pictures.
pub fn probe(wanted: Option<&str>, mode: Option<&str>, seconds: u64, snapshot: Option<&std::path::Path>) -> Result<()> {
    let camera = find(camera::list()?, wanted)?;
    let spec = match mode {
        Some(mode) => Spec::new(
            &camera,
            parse_mode(&camera, mode)?,
            Default::default(),
            camera::default_adapter()?,
        ),
        None => camera::default_spec(&camera)?,
    };
    let opened_at = Instant::now();
    let mut capture = crate::platform::source::open(&crate::media::capture::Source::Camera(spec.clone()))?;
    let opened_ms = opened_at.elapsed().as_millis();
    let started = Instant::now();
    let mut frames = 0u32;
    let mut first_ms = None;
    let mut last = None::<Instant>;
    let mut gaps = Vec::new();
    let mut size = (0, 0);
    let mut saved = None;
    while started.elapsed() < Duration::from_secs(seconds) {
        let Some(frame) = capture.next(
            100,
            crate::media::geometry::Tier::P2160,
            false,
            false,
            (3840, 2160),
        )?
        else {
            continue;
        };
        if !frame.is_new {
            continue;
        }
        let now = Instant::now();
        frames += 1;
        first_ms.get_or_insert(started.elapsed().as_millis());
        if let Some(last) = last {
            gaps.push(now.duration_since(last).as_secs_f64() * 1000.0);
        }
        last = Some(now);
        size = (frame.width, frame.height);
        if let (Some(path), None) = (snapshot, &saved) {
            let image = camera::read_back(capture.device(), &frame.texture)?;
            image.save(path)?;
            saved = Some(path.display().to_string());
        }
    }
    gaps.sort_by(f64::total_cmp);
    let percentile = |p: f64| gaps.get(((gaps.len() as f64 - 1.0) * p).round() as usize).copied();
    let report = serde_json::json!({
        "camera": camera.name,
        "mode": spec.mode,
        "backend": capture.backend_name(),
        "opened_ms": opened_ms,
        "first_frame_ms": first_ms,
        "frames": frames,
        "fps": f64::from(frames) / seconds as f64,
        "size": [size.0, size.1],
        "interval_p50_ms": percentile(0.5),
        "interval_max_ms": gaps.last(),
        "snapshot": saved,
    });
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}
