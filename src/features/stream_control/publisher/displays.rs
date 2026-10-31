//! Display and capture requests on the host (SESSION `Capture`, `Display`)
//! and its display list. The screen and display owners do the native work;
//! a refusal carries a [`display_reason`] the controller words.
use super::*;
use crate::features::host::peer::screens::{ScreenInfo, Screens};
use crate::features::stream_control::wire::{
    self, CaptureControl, DisplayOp, DisplayRequest, SessionBody, display_reason, stream_reason,
};
use farwing_protocol::peer::error_code;
use std::collections::BTreeMap;

/// The displays as one participant sees them: its video track of each and
/// its active display.
pub(crate) fn display_list(
    screens: &[ScreenInfo],
    tracks: &BTreeMap<i32, usize>,
    active: i32,
    revision: u64,
) -> wire::Displays {
    let size = |(width, height): (u32, u32)| wire::Size { width, height };
    let displays = screens
        .iter()
        .map(|info| {
            let screen = &info.screen;
            let mut modes: Vec<_> = info
                .target
                .as_ref()
                .map(|t| t.modes.iter().map(|m| (m.width, m.height)).collect())
                .unwrap_or_default();
            modes.push((screen.width, screen.height));
            modes.sort_unstable();
            modes.dedup();
            let dpi = info.target.as_ref().and_then(|t| t.dpi.as_ref());
            wire::Display {
                id: screen.id,
                kind: info.kind as i32,
                name: info
                    .target
                    .as_ref()
                    .and_then(|target| crate::media::capture::monitor_name(&target.name))
                    .or_else(|| crate::media::capture::monitor_name(&screen.display_name))
                    .unwrap_or_default()
                    .to_owned(),
                primary: screen.primary,
                width: screen.width,
                height: screen.height,
                refresh_hz: screen.fps,
                hdr: screen.hdr,
                modes: modes.into_iter().map(size).collect(),
                preferred: info.target.as_ref().and_then(|t| t.preferred_mode).map(size),
                initial: info
                    .initial
                    .filter(|initial| *initial != (screen.width, screen.height))
                    .map(size),
                scale: dpi.map_or(0, |d| d.current),
                recommended_scale: dpi.map_or(0, |d| d.recommended),
                scales: dpi.map(|d| d.supported.clone()).unwrap_or_default(),
                track: tracks.get(&screen.id).map_or(-1, |i| *i as i32),
                mirror: false,
                quarter_turns: 0,
            }
        })
        .collect();
    wire::Displays {
        revision,
        displays,
        active,
    }
}

/// A refused request: the `PeerError` code and [`display_reason`] the
/// controller gets.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Refused(pub &'static str, pub &'static str);
impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.1, self.0)
    }
}
impl std::error::Error for Refused {}

pub(crate) fn refuse(code: &'static str, reason: &'static str) -> anyhow::Error {
    Refused(code, reason).into()
}

/// Tell the other participants that this one changed the displays.
fn display_changed(session: &Screens) {
    let lease = session.authorization();
    crate::features::host::participants::display_changed(
        lease.participant(),
        &lease.profile().nickname,
    );
}

/// The answer to a `Capture` or `Display` request: the display list it
/// left, or why it was refused.
fn answer(session: &Screens, id: u32, result: &Result<()>) -> Vec<u8> {
    match result {
        Ok(()) => wire::response(id, SessionBody::Displays(session.display_list())),
        Err(error) => {
            let Refused(code, reason) = error
                .downcast_ref::<Refused>()
                .copied()
                .unwrap_or(Refused(error_code::INTERNAL, display_reason::FAILED));
            tracing::warn!(error = %format!("{error:#}"), "host display request refused or failed");
            wire::error_response(id, code, reason)
        }
    }
}

/// Display and capture messages of the session channel (`SESSION`).
pub(crate) async fn receive_session(session: &mut Screens, bytes: &[u8]) -> Result<Received> {
    anyhow::ensure!(session.authorization().requested(), "被控许可已失效");
    let Some(received) = wire::decode(bytes)? else {
        return Ok(Received::default());
    };
    if received.response {
        return Ok(Received::default());
    }
    let id = received.request_id;
    let Some(body) = received.body else {
        return Ok(Received::default());
    };
    tracing::debug!(message = body.name(), id, "host session message");
    let push = matches!(body, SessionBody::ReceiverStats(_));
    anyhow::ensure!(push || id != 0, "会话请求缺少标识");
    match body {
        SessionBody::Capture(capture) => {
            tracing::debug!(
                start = capture.start,
                screen_id = capture.screen_id,
                "host capture request"
            );
            let result = capture_request(session, &capture).await;
            Ok(Received {
                messages: vec![answer(session, id, &result)],
                refresh_state: true,
                media_active: (capture.start && result.is_ok()).then_some(true),
                ..Default::default()
            })
        }
        SessionBody::Display(request) => {
            tracing::info!(?request, "host display request");
            let result = display_request(session, request).await;
            if result.is_ok() {
                display_changed(session);
            }
            Ok(vec![answer(session, id, &result)].into())
        }
        SessionBody::StreamRequest(request) => {
            tracing::info!(?request, "host stream request");
            Ok(vec![stream_request(session, id, request)].into())
        }
        SessionBody::CameraRequest(request) => {
            tracing::info!(?request, "host camera request");
            let result = match request.op {
                Some(op) => session.camera_request(op).await,
                None => Err(refuse(error_code::BAD_REQUEST, display_reason::INVALID)),
            };
            let message = match &result {
                Ok(()) => wire::response(id, SessionBody::Cameras(session.camera_list())),
                Err(error) => {
                    let Refused(code, reason) = error
                        .downcast_ref::<Refused>()
                        .copied()
                        .unwrap_or(Refused(error_code::INTERNAL, display_reason::FAILED));
                    tracing::warn!(error = %format!("{error:#}"), "host camera request refused or failed");
                    wire::error_response(id, code, reason)
                }
            };
            Ok(Received {
                messages: vec![message],
                refresh_state: true,
                ..Default::default()
            })
        }
        SessionBody::MicrophonePolicy(_) => Ok(Received::default()),
        SessionBody::ReceiverStats(stats) => {
            session.receiver_stats(&stats);
            Ok(Received::default())
        }
        other => {
            tracing::debug!(message = other.name(), "unsupported session request");
            Ok(vec![wire::error_response(id, error_code::UNSUPPORTED, "")].into())
        }
    }
}

async fn capture_request(session: &mut Screens, capture: &CaptureControl) -> Result<()> {
    if !capture.start {
        return session.stop(capture.screen_id).await;
    }
    // A files or ports session, or one whose `view` was withdrawn, never
    // gets the picture; a camera needs `camera` instead (checked there).
    if !crate::features::host::cameras::is_camera(capture.screen_id)
        && !session
            .authorization()
            .granted(farwing_protocol::caps::VIEW)
    {
        return Err(refuse(error_code::FORBIDDEN, display_reason::NOT_ALLOWED));
    }
    match capture.track {
        _ if capture.screen_id == -1 => session.resume_capture().await,
        Some(track) => {
            let track = usize::try_from(track)
                .ok()
                .filter(|track| *track < crate::features::host::peer::screens::TRACK_COUNT)
                .ok_or_else(|| refuse(error_code::BAD_REQUEST, display_reason::INVALID))?;
            session.start_on(capture.screen_id, Some(track)).await
        }
        None => session.start(capture.screen_id).await,
    }
}

fn valid_size(width: u32, height: u32) -> Result<()> {
    if (2..=16384).contains(&width) && (2..=16384).contains(&height) {
        Ok(())
    } else {
        Err(refuse(error_code::BAD_REQUEST, display_reason::INVALID))
    }
}

async fn display_request(session: &mut Screens, request: DisplayRequest) -> Result<()> {
    let op = request
        .op
        .ok_or_else(|| refuse(error_code::BAD_REQUEST, display_reason::INVALID))?;
    let lease = session.authorization();
    let allowed = match op {
        DisplayOp::AddVirtual(_) | DisplayOp::EnterExclusive(_) => {
            lease.display_expansion_allowed()
        }
        _ => lease.display_allowed(),
    };
    if !allowed {
        return Err(refuse(error_code::FORBIDDEN, display_reason::NOT_ALLOWED));
    }
    if lease.updating() {
        // An update snapshots the layout under this same screen owner lock;
        // a change now would not survive it.
        return Err(refuse(error_code::BUSY, display_reason::UPDATING));
    }
    match op {
        DisplayOp::SetMode(mode) => {
            valid_size(mode.width, mode.height)?;
            session.refresh()?;
            let info = session.info(mode.id)?;
            session.set_mode(info, mode.width, mode.height).await
        }
        DisplayOp::SetScale(scale) => {
            session.refresh()?;
            let info = session.info(scale.id)?;
            session.set_scale(info, scale.scale).await
        }
        DisplayOp::AddVirtual(add) => {
            valid_size(add.width, add.height)?;
            session.add_virtual(add.width, add.height, add.scale).await
        }
        DisplayOp::Remove(remove) => session.remove_virtual(remove.id).await,
        DisplayOp::EnterExclusive(enter) => {
            valid_size(enter.width, enter.height)?;
            session
                .enter_super(enter.width, enter.height, enter.scale)
                .await
                .map(drop)
        }
        DisplayOp::ExitExclusive(_) => session.quit_super().await,
    }
}

/// A change of this participant's stream: the state it leads to, or why
/// it was refused.
fn stream_request(session: &mut Screens, id: u32, request: wire::StreamRequest) -> Vec<u8> {
    if session.authorization().updating() {
        // An update snapshots under this same screen owner lock. Do not
        // acknowledge settings that cannot survive it.
        return wire::error_response(id, error_code::BUSY, stream_reason::UPDATING);
    }
    let settings = request.apply(session.stream().into()).into();
    if !super::valid(&settings) {
        return wire::error_response(id, error_code::BAD_REQUEST, stream_reason::INVALID);
    }
    session.set_stream(settings);
    wire::response(id, SessionBody::Stream(session.stream_state()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::stream_control::wire::DisplayKind;

    fn screen(id: i32, width: u32, height: u32) -> crate::features::host::capture::Screen {
        crate::features::host::capture::Screen {
            id,
            device_name: String::new(),
            display_name: format!("DISPLAY{id}"),
            width,
            height,
            left: 0,
            top: 0,
            primary: id == 0,
            fps: 60,
            dpi_scale: None,
            hdr: false,
            adapter: 0,
            render_adapter: None,
            identity: None,
        }
    }

    #[test]
    fn the_list_shows_kinds_tracks_and_only_changed_initial_modes() {
        let screens = [
            ScreenInfo {
                screen: screen(0, 2560, 1440),
                initial: Some((3840, 2160)),
                target: None,
                kind: DisplayKind::Physical,
            },
            ScreenInfo {
                screen: screen(7, 1920, 1080),
                initial: Some((1920, 1080)),
                target: None,
                kind: DisplayKind::Virtual,
            },
        ];
        let list = display_list(&screens, &BTreeMap::from([(7, 2)]), 7, 42);
        assert_eq!((list.revision, list.active), (42, 7));
        let [first, second] = &list.displays[..] else {
            panic!("two displays");
        };
        assert_eq!(first.kind(), DisplayKind::Physical);
        assert_eq!(first.initial, Some(wire::Size { width: 3840, height: 2160 }));
        assert_eq!(first.track, -1);
        assert_eq!(first.name, "", "a numbered source is no name");
        assert_eq!(second.kind(), DisplayKind::Virtual);
        assert_eq!(second.initial, None, "unchanged");
        assert_eq!(second.track, 2);
        assert_eq!(second.modes, vec![wire::Size { width: 1920, height: 1080 }]);
    }

    #[test]
    fn refusals_answer_with_their_reason() {
        let error = refuse(error_code::BUSY, display_reason::NO_FREE_TRACK).context("starting");
        let Refused(code, reason) = *error.downcast_ref::<Refused>().unwrap();
        assert_eq!((code, reason), (error_code::BUSY, display_reason::NO_FREE_TRACK));
    }
}
