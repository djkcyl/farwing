//! The participants' pointers drawn for the person at this computer
//! (`docs/protocol/v2-design.md` §4.3). One thread runs while anyone is
//! connected. It draws only in the active, signed-in session on the default
//! desktop: never in Session 0, over the logon screen or on the secure desktop.
use super::{MemberView, PointerView};
use crate::media::capture::Screen;
use crate::i18n::t;
use crate::platform::windows::{
    host_service::process,
    participant_overlay::{self as platform, Key, Overlay, Shape, Sprite},
};
use crate::protocol::participants::{button, phase};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// A pointer fades out after this long without moving.
const IDLE: Duration = Duration::from_secs(5);
const FADE: Duration = Duration::from_millis(800);
/// How long a click ring expands and fades.
const RING: Duration = Duration::from_millis(600);
/// Repaint interval while something animates.
const FRAME: Duration = Duration::from_millis(16);
/// Longest wait when nothing animates; also bounds message pumping.
const IDLE_WAIT: Duration = Duration::from_millis(500);
const SCREENS_REFRESH: Duration = Duration::from_secs(2);
/// A pointer on a screen not in the cache refreshes it at most this often.
const MISSING_SCREEN_REFRESH: Duration = Duration::from_secs(1);
const GATE_REFRESH: Duration = Duration::from_millis(250);
const RETRY: Duration = Duration::from_secs(5);
const MAX_RINGS: usize = 32;
const MAX_LABEL: usize = 16;

static RUNNING: AtomicBool = AtomicBool::new(false);

/// Start the overlay thread unless it already runs. It ends by itself once
/// the last participant has left.
pub(super) fn start() {
    // Unit tests join participants; they must never draw on the desktop.
    if cfg!(test) || RUNNING.swap(true, Ordering::AcqRel) {
        return;
    }
    if !platform::capture_exclusion() {
        RUNNING.store(false, Ordering::Release);
        static LOGGED: AtomicBool = AtomicBool::new(false);
        if !LOGGED.swap(true, Ordering::Relaxed) {
            tracing::info!("participant overlay needs Windows 10 2004 to stay out of the video");
        }
        return;
    }
    if let Err(error) = std::thread::Builder::new()
        .name("participant-overlay".into())
        .spawn(run)
    {
        RUNNING.store(false, Ordering::Release);
        tracing::warn!(%error, "participant overlay thread could not start");
    }
}

fn run() {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            RUNNING.store(false, Ordering::Release);
            tracing::warn!(%error, "participant overlay could not start");
            return;
        }
    };
    let (mut members, mut pointers) = super::subscribe();
    let mut driver = Driver::default();
    let mut drawn: Option<Instant> = None;
    loop {
        // Coalesce bursts of pointer frames into the repaint rate.
        if let Some(rest) = drawn.and_then(|at| FRAME.checked_sub(at.elapsed())) {
            std::thread::sleep(rest);
        }
        members.borrow_and_update();
        pointers.borrow_and_update();
        let snapshot = super::snapshot();
        if snapshot.is_empty() {
            // Windows belong to this thread and go with it.
            driver.overlay = None;
            RUNNING.store(false, Ordering::Release);
            // Someone who joined meanwhile saw the flag still set.
            if super::state().members.is_empty() || RUNNING.swap(true, Ordering::AcqRel) {
                return;
            }
            continue;
        }
        let wait = driver.frame(&snapshot, Instant::now());
        drawn = Some(Instant::now());
        runtime.block_on(async {
            tokio::select! {
                _ = members.changed() => (),
                _ = pointers.changed() => (),
                _ = tokio::time::sleep(wait) => (),
            }
        });
    }
}

/// A position on the host's desktop.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Place {
    /// Desktop pixels.
    at: (i32, i32),
    /// The screen: left, top, right, bottom.
    bounds: [i32; 4],
    /// The screen's DPI scale, 1.0 at 100 %.
    scale: f32,
}

fn bounds(screen: &Screen) -> [i32; 4] {
    [
        screen.left,
        screen.top,
        screen.left.saturating_add_unsigned(screen.width),
        screen.top.saturating_add_unsigned(screen.height),
    ]
}

fn scale(screen: &Screen) -> f32 {
    screen
        .dpi_scale
        .map_or(1.0, |percent| percent as f32 / 100.0)
        .clamp(0.5, 4.0)
}

/// A normalized position on `screen_id`, mapped the way absolute input is.
fn locate(screens: &[Screen], screen_id: i32, x: f32, y: f32) -> Option<Place> {
    if !(x.is_finite() && y.is_finite()) {
        return None;
    }
    let screen = screens
        .iter()
        .find(|s| s.id == screen_id && s.width > 0 && s.height > 0)?;
    let [left, top, right, bottom] = bounds(screen);
    let px = (f64::from(left) + f64::from(screen.width) * f64::from(x.clamp(0.0, 1.0)))
        .round()
        .clamp(f64::from(left), f64::from(right - 1));
    let py = (f64::from(top) + f64::from(screen.height) * f64::from(y.clamp(0.0, 1.0)))
        .round()
        .clamp(f64::from(top), f64::from(bottom - 1));
    Some(Place {
        at: (px as i32, py as i32),
        bounds: [left, top, right, bottom],
        scale: scale(screen),
    })
}

/// The screen holding a desktop point, such as the real cursor.
fn containing(screens: &[Screen], at: (i32, i32)) -> Option<Place> {
    let screen = screens.iter().find(|s| {
        let [left, top, right, bottom] = bounds(s);
        (left..right).contains(&at.0) && (top..bottom).contains(&at.1)
    })?;
    Some(Place {
        at,
        bounds: bounds(screen),
        scale: scale(screen),
    })
}

/// Full until the pointer has idled for [`IDLE`], then fading over [`FADE`].
fn opacity(idle: Duration) -> f32 {
    match idle.checked_sub(IDLE) {
        None => 1.0,
        Some(over) => (1.0 - over.as_secs_f32() / FADE.as_secs_f32()).max(0.0),
    }
}

/// The button of a click not shown yet: a press, or several click events
/// since the last frame. `seen` is the click counter shown last.
fn new_click(seen: Option<u32>, pointer: &PointerView) -> Option<i32> {
    let (click, _) = pointer.click.filter(|(_, age)| *age < RING)?;
    let advanced = seen.map_or(1, |seen| pointer.clicks.wrapping_sub(seen));
    (advanced >= 2 || (advanced == 1 && click.phase == phase::DOWN)).then_some(click.button)
}

fn click_color(pressed: i32) -> u32 {
    match pressed {
        button::LEFT => 0x3B82F6,
        button::RIGHT => 0xF97316,
        _ => 0x9CA3AF,
    }
}

/// The name on a participant's label: nickname, else device name.
fn label(member: &MemberView) -> String {
    let clean = |text: &str| {
        text.chars()
            .filter(|c| {
                !c.is_control() && !matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
            })
            .collect::<String>()
            .trim()
            .to_owned()
    };
    let name = [&member.nickname, &member.device_name]
        .into_iter()
        .map(|text| clean(text))
        .find(|text| !text.is_empty())
        .unwrap_or_else(|| t().features.host_participant.into());
    if name.chars().count() > MAX_LABEL {
        name.chars().take(MAX_LABEL - 1).chain(Some('…')).collect()
    } else {
        name
    }
}

fn shown(pointer: &PointerView) -> bool {
    pointer.visible && opacity(pointer.idle) > 0.0
}

/// How long to wait for the next frame without a revision change.
fn next_wait(members: &[MemberView], rings: bool) -> Duration {
    if rings {
        return FRAME;
    }
    let mut wait = IDLE_WAIT;
    for member in members {
        let Some(pointer) = member.pointer.as_ref().filter(|p| shown(p)) else {
            continue;
        };
        // The controlling label follows the real cursor; others fade.
        if member.controlling || pointer.idle >= IDLE {
            return FRAME;
        }
        wait = wait.min(IDLE - pointer.idle);
    }
    wait
}

struct Ring {
    key: u64,
    seq: u32,
    place: Place,
    color: u32,
    at: Instant,
}

#[derive(Default)]
struct Driver {
    overlay: Option<Overlay>,
    screens: Vec<Screen>,
    screens_at: Option<Instant>,
    /// Whether this session may be drawn on, and when that was checked.
    gate: Option<(Instant, bool)>,
    /// The click counter last seen per participant.
    clicks: BTreeMap<u64, u32>,
    rings: Vec<Ring>,
    retry: Option<Instant>,
    failure: Option<String>,
}
impl Driver {
    fn frame(&mut self, members: &[MemberView], now: Instant) -> Duration {
        self.rings
            .retain(|ring| now.saturating_duration_since(ring.at) < RING);
        if !self.interactive(now) {
            self.overlay = None;
            // Clicks meanwhile are seen, never replayed.
            self.observe_clicks(members, None, now);
            self.rings.clear();
            return IDLE_WAIT;
        }
        if let Some(retry) = self.retry.filter(|at| now < *at) {
            self.observe_clicks(members, None, now);
            self.rings.clear();
            return (retry - now).min(IDLE_WAIT);
        }
        let active: Vec<_> = members
            .iter()
            .filter_map(|m| m.pointer.as_ref().filter(|p| shown(p)).map(|p| (m, p)))
            .collect();
        let mut cursor = None;
        if !active.is_empty() {
            let missing = active
                .iter()
                .any(|(m, p)| !m.controlling && !self.screens.iter().any(|s| s.id == p.screen_id));
            self.refresh_screens(now, missing);
            if active.iter().any(|(m, _)| m.controlling) {
                cursor = platform::cursor().and_then(|at| containing(&self.screens, at));
            }
        }
        self.observe_clicks(members, cursor, now);
        let mut sprites: Vec<_> = active
            .iter()
            .filter_map(|(member, pointer)| {
                let place = if member.controlling {
                    cursor?
                } else {
                    locate(&self.screens, pointer.screen_id, pointer.x, pointer.y)?
                };
                Some(Sprite {
                    key: Key::Pointer(member.key),
                    shape: Shape::Pointer {
                        label: label(member),
                        color: member.color,
                        arrow: !member.controlling,
                    },
                    at: place.at,
                    bounds: place.bounds,
                    scale: place.scale,
                    opacity: opacity(pointer.idle),
                })
            })
            .collect();
        sprites.extend(self.rings.iter().map(|ring| Sprite {
            key: Key::Click(ring.key, ring.seq),
            shape: Shape::Click {
                color: ring.color,
                progress: now.saturating_duration_since(ring.at).as_secs_f32()
                    / RING.as_secs_f32(),
            },
            at: ring.place.at,
            bounds: ring.place.bounds,
            scale: ring.place.scale,
            opacity: 1.0,
        }));
        if sprites.is_empty() && self.overlay.is_none() {
            return next_wait(members, false);
        }
        if let Err(error) = self.draw(&sprites) {
            let error = format!("{error:#}");
            if self.failure.as_ref() != Some(&error) {
                tracing::warn!(%error, "participant overlay failed; retrying");
                self.failure = Some(error);
            }
            self.overlay = None;
            self.rings.clear();
            self.retry = Some(now + RETRY);
            return RETRY.min(IDLE_WAIT);
        }
        self.failure = None;
        next_wait(members, !self.rings.is_empty())
    }

    fn draw(&mut self, sprites: &[Sprite]) -> anyhow::Result<()> {
        let overlay = match &mut self.overlay {
            Some(overlay) => overlay,
            None => self.overlay.insert(Overlay::new()?),
        };
        if overlay.maintain()? {
            // Screens moved: the next frame maps pointers again.
            self.screens_at = None;
        }
        overlay.show(sprites)
    }

    fn interactive(&mut self, now: Instant) -> bool {
        if let Some((at, value)) = self.gate
            && now.saturating_duration_since(at) < GATE_REFRESH
        {
            return value;
        }
        let value = process::session(std::process::id()).is_ok_and(|session| {
            session != 0 && session == process::active_session() && process::logged_on(session)
        }) && crate::features::host::clipboard::agent::desktop_available();
        self.gate = Some((now, value));
        value
    }

    fn refresh_screens(&mut self, now: Instant, missing: bool) {
        let age = self
            .screens_at
            .map(|at| now.saturating_duration_since(at));
        if age.is_some_and(|age| age < SCREENS_REFRESH && !(missing && age >= MISSING_SCREEN_REFRESH))
        {
            return;
        }
        match crate::platform::windows::capture::screens() {
            Ok(screens) => self.screens = screens,
            Err(error) => tracing::debug!(%error, "participant overlay screens unavailable"),
        }
        self.screens_at = Some(now);
    }

    /// Start a ring for every new click; `cursor` places the controlling
    /// participant's clicks. Without a place, the click is only marked seen.
    fn observe_clicks(&mut self, members: &[MemberView], cursor: Option<Place>, now: Instant) {
        self.clicks
            .retain(|key, _| members.iter().any(|m| m.key == *key));
        for member in members {
            let Some(pointer) = &member.pointer else {
                continue;
            };
            let seen = self.clicks.insert(member.key, pointer.clicks);
            let Some(pressed) = new_click(seen, pointer) else {
                continue;
            };
            let place = if member.controlling {
                cursor
            } else {
                None
            }
            .or_else(|| locate(&self.screens, pointer.screen_id, pointer.x, pointer.y));
            let (Some(place), Some((_, age))) = (place, pointer.click) else {
                continue;
            };
            if self.rings.len() >= MAX_RINGS {
                self.rings.remove(0);
            }
            self.rings.push(Ring {
                key: member.key,
                seq: pointer.clicks,
                place,
                color: click_color(pressed),
                at: now.checked_sub(age).unwrap_or(now),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::participants::Click;

    fn screen(id: i32, left: i32, top: i32, width: u32, height: u32, dpi: u32) -> Screen {
        Screen {
            id,
            device_name: format!(r"\\.\DISPLAY{id}"),
            display_name: String::new(),
            width,
            height,
            left,
            top,
            primary: left == 0 && top == 0,
            fps: 60,
            dpi_scale: Some(dpi),
            hdr: false,
            adapter: 0,
            render_adapter: None,
            identity: None,
        }
    }

    fn pointer(clicks: u32, click: Option<(i32, i32, u64)>) -> PointerView {
        PointerView {
            screen_id: 1,
            x: 0.5,
            y: 0.5,
            visible: true,
            idle: Duration::ZERO,
            click: click.map(|(button, phase, ms)| {
                (Click { button, phase }, Duration::from_millis(ms))
            }),
            clicks,
        }
    }

    fn member(key: u64, pointer: Option<PointerView>, controlling: bool) -> MemberView {
        MemberView {
            key,
            nickname: format!("p{key}"),
            device_name: String::new(),
            relation: "friend".into(),
            color: 0x22C55E,
            control: controlling,
            controlling,
            pointer,
        }
    }

    #[test]
    fn pointers_map_onto_their_screen() {
        let screens = [screen(1, 0, 0, 2560, 1440, 150), screen(2, -1920, 120, 1920, 1080, 100)];
        let place = locate(&screens, 1, 0.5, 0.25).unwrap();
        assert_eq!(place.at, (1280, 360));
        assert_eq!(place.bounds, [0, 0, 2560, 1440]);
        assert_eq!(place.scale, 1.5);
        // The far edge stays on the screen, like absolute input.
        assert_eq!(locate(&screens, 2, 1.0, 1.0).unwrap().at, (-1, 1199));
        assert_eq!(locate(&screens, 2, 0.0, 0.0).unwrap().at, (-1920, 120));
        assert!(locate(&screens, 3, 0.5, 0.5).is_none());
        assert!(locate(&screens, -1, 0.5, 0.5).is_none());
        assert!(locate(&screens, 1, f32::NAN, 0.5).is_none());

        let place = containing(&screens, (-5, 500)).unwrap();
        assert_eq!((place.bounds[0], place.scale), (-1920, 1.0));
        assert_eq!(containing(&screens, (2559, 1439)).unwrap().scale, 1.5);
        assert!(containing(&screens, (2560, 0)).is_none());
        assert!(containing(&screens, (-100, 50)).is_none());
    }

    #[test]
    fn idle_pointers_fade_out() {
        assert_eq!(opacity(Duration::ZERO), 1.0);
        assert_eq!(opacity(IDLE), 1.0);
        assert!((opacity(IDLE + FADE / 2) - 0.5).abs() < 1e-3);
        assert_eq!(opacity(IDLE + FADE), 0.0);
        assert_eq!(opacity(Duration::from_secs(60)), 0.0);
    }

    #[test]
    fn rings_start_once_per_press() {
        use crate::protocol::participants::button::{LEFT, MIDDLE, RIGHT};
        let down = |clicks, ms| pointer(clicks, Some((LEFT, phase::DOWN, ms)));
        assert_eq!(new_click(None, &down(1, 10)), Some(LEFT));
        assert_eq!(new_click(Some(0), &down(1, 10)), Some(LEFT));
        assert_eq!(new_click(Some(1), &down(1, 10)), None);
        // Too old to animate, or only the release.
        assert_eq!(new_click(None, &down(1, 700)), None);
        let up = pointer(2, Some((RIGHT, phase::UP, 10)));
        assert_eq!(new_click(Some(1), &up), None);
        // Press and release both arrived between frames.
        assert_eq!(new_click(Some(0), &up), Some(RIGHT));
        assert_eq!(new_click(None, &pointer(0, None)), None);
        assert_eq!(click_color(LEFT), 0x3B82F6);
        assert_eq!(click_color(RIGHT), 0xF97316);
        assert_eq!(click_color(MIDDLE), 0x9CA3AF);
    }

    #[test]
    fn labels_fall_back_and_stay_short() {
        let mut m = member(1, None, false);
        m.nickname = " \u{202e}Alice\n ".into();
        assert_eq!(label(&m), "Alice");
        m.nickname = " ".into();
        m.device_name = "办公室电脑".into();
        assert_eq!(label(&m), "办公室电脑");
        m.device_name.clear();
        assert_eq!(label(&m), crate::i18n::t().features.host_participant);
        m.nickname = "一二三四五六七八九十一二三四五六七八".into();
        let short = label(&m);
        assert_eq!(short.chars().count(), MAX_LABEL);
        assert!(short.ends_with('…'));
    }

    #[test]
    fn waits_follow_what_animates() {
        let still = member(1, Some(pointer(0, None)), false);
        assert_eq!(next_wait(&[still.clone()], false), IDLE_WAIT);
        assert_eq!(next_wait(&[still.clone()], true), FRAME);
        let mut idle = still.clone();
        idle.pointer.as_mut().unwrap().idle = IDLE - Duration::from_millis(200);
        assert_eq!(next_wait(&[idle.clone()], false), Duration::from_millis(200));
        idle.pointer.as_mut().unwrap().idle = IDLE + FADE / 2;
        assert_eq!(next_wait(&[idle.clone()], false), FRAME);
        idle.pointer.as_mut().unwrap().idle = IDLE + FADE;
        assert_eq!(next_wait(&[idle], false), IDLE_WAIT);
        assert_eq!(next_wait(&[member(2, Some(pointer(0, None)), true)], false), FRAME);
        let mut hidden = member(3, Some(pointer(0, None)), true);
        hidden.pointer.as_mut().unwrap().visible = false;
        assert_eq!(next_wait(&[hidden, member(4, None, false)], false), IDLE_WAIT);
    }

    #[test]
    fn clicks_become_rings_where_they_happened() {
        let mut driver = Driver {
            screens: vec![screen(1, 0, 0, 1920, 1080, 125)],
            ..Default::default()
        };
        let now = Instant::now();
        let a = member(1, Some(pointer(1, Some((button::LEFT, phase::DOWN, 100)))), false);
        let b = member(2, Some(pointer(3, Some((button::LEFT, phase::DOWN, 10)))), true);
        let cursor = Place {
            at: (10, 20),
            bounds: [0, 0, 1920, 1080],
            scale: 1.25,
        };
        driver.observe_clicks(&[a.clone(), b.clone()], Some(cursor), now);
        assert_eq!(driver.rings.len(), 2);
        assert_eq!(driver.rings[0].place.at, (960, 540));
        assert_eq!(driver.rings[0].color, 0x3B82F6);
        assert!(now.duration_since(driver.rings[0].at) >= Duration::from_millis(100));
        // The controlling participant clicks where the real cursor is.
        assert_eq!(driver.rings[1].place.at, (10, 20));
        driver.observe_clicks(&[a, b], Some(cursor), now);
        assert_eq!(driver.rings.len(), 2);
        driver.observe_clicks(&[], None, now);
        assert!(driver.clicks.is_empty());
    }
}
