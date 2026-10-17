//! Capture through the XDG ScreenCast portal, Sunshine's `portalgrab` path:
//! the desktop's own compositor hands over each shared monitor as a PipeWire
//! stream. It is the sanctioned way to capture a Wayland session, and works
//! on X11 desktops whose portal backend offers it (GNOME, KDE).
//!
//! The first session asks the local user which monitors to share. Sharing is
//! requested as persistent, and the restore token the portal returns is kept,
//! so later sessions (including after a restart) start without asking again
//! until the user revokes it.
//!
//! One portal session serves every captured monitor of this process, and its
//! cursor mode is fixed when it starts; a desktop wanting the other mode gets
//! its own session.
//!
//! A shared monitor is paired with the screen the controller asked for by the
//! rectangle the compositor lays it out at, not by anything the portal names it
//! with: the portal's `id` is an opaque counter and the streams arrive in no
//! particular order.
use super::Picture;
use super::Screen;
use super::pipewire::Stream;
use crate::platform::CompositorLayout;
use anyhow::{Context, Result, anyhow, bail};
use ashpd::desktop::PersistMode;
use ashpd::desktop::screencast::{
    CursorMode, OpenPipeWireRemoteOptions, Screencast, SelectSourcesOptions, SourceType,
    StartCastOptions,
};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

/// One shared monitor as the portal described it.
#[derive(Clone, Debug)]
struct Source {
    node: u32,
    position: Option<(i32, i32)>,
    size: Option<(i32, i32)>,
}

struct Session {
    runtime: tokio::runtime::Runtime,
    proxy: Screencast,
    session: ashpd::desktop::Session<Screencast>,
    sources: Vec<Source>,
    cursor: bool,
}

/// Run portal I/O on a thread of its own, so it never blocks inside (or
/// panics because of) whatever runtime the caller may be part of.
fn on_portal<T: Send>(
    runtime: &tokio::runtime::Runtime,
    work: impl std::future::Future<Output = T> + Send,
) -> T {
    std::thread::scope(|scope| {
        scope
            .spawn(|| runtime.block_on(work))
            .join()
            .expect("portal thread panicked")
    })
}

fn token_path() -> Result<std::path::PathBuf> {
    Ok(crate::platform::paths::require_local_app_data()?.join("OpenUUYC/screencast-restore-token"))
}

/// Whether the user has granted a session that the portal can restore.
pub(super) fn remembered() -> bool {
    token_path()
        .and_then(|path| Ok(std::fs::read_to_string(path)?))
        .is_ok_and(|token| !token.trim().is_empty())
}

impl Session {
    fn start(cursor: bool) -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("screencast-portal")
            .enable_all()
            .build()?;
        let token = std::fs::read_to_string(token_path()?)
            .ok()
            .map(|token| token.trim().to_owned())
            .filter(|token| !token.is_empty());
        let started = on_portal(&runtime, async {
            let proxy = Screencast::new()
                .await
                .context("桌面没有提供 ScreenCast 门户")?;
            let session = proxy.create_session(Default::default()).await?;
            proxy
                .select_sources(
                    &session,
                    SelectSourcesOptions::default()
                        .set_sources(Some(SourceType::Monitor.into()))
                        .set_multiple(true)
                        .set_cursor_mode(if cursor {
                            CursorMode::Embedded
                        } else {
                            CursorMode::Hidden
                        })
                        .set_persist_mode(PersistMode::ExplicitlyRevoked)
                        .set_restore_token(token.as_deref()),
                )
                .await?
                .response()?;
            // Without a usable restore token this shows the sharing dialog on
            // this machine and waits for the local user.
            let streams = proxy
                .start(&session, None, StartCastOptions::default())
                .await?
                .response()
                .context("本机用户没有同意屏幕共享")?;
            let sources = streams
                .streams()
                .iter()
                .map(|stream| Source {
                    node: stream.pipe_wire_node_id(),
                    position: stream.position(),
                    size: stream.size(),
                })
                .collect::<Vec<_>>();
            let restore = streams.restore_token().map(str::to_owned);
            anyhow::Ok((proxy, session, sources, restore))
        })?;
        let (proxy, session, sources, restore) = started;
        if let Some(restore) = restore {
            let path = token_path()?;
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, restore)?;
        }
        if sources.is_empty() {
            bail!("屏幕共享没有包含任何显示器");
        }
        tracing::info!(?sources, cursor, "ScreenCast portal session started");
        Ok(Self {
            runtime,
            proxy,
            session,
            sources,
            cursor,
        })
    }

    /// The shared session with this cursor mode, started if none is alive.
    fn shared(cursor: bool) -> Result<Arc<Self>> {
        static SESSIONS: Mutex<Vec<Weak<Session>>> = Mutex::new(Vec::new());
        // Held across start: a restore token is single-use, so two sessions
        // starting together would leave one of them asking the user again.
        let mut sessions = SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
        sessions.retain(|session| session.strong_count() > 0);
        if let Some(session) = sessions
            .iter()
            .filter_map(Weak::upgrade)
            .find(|session| session.cursor == cursor)
        {
            return Ok(session);
        }
        let session = Arc::new(Self::start(cursor)?);
        sessions.push(Arc::downgrade(&session));
        Ok(session)
    }

    /// The shared monitor that is `screen`.
    fn source(&self, screen: &Screen) -> Result<&Source> {
        // Asked every time, not only when the display server's rectangle
        // misses: on a desktop with mixed scales that rectangle can be another
        // monitor's (see `pick`). Outside a Wayland session this returns at
        // once, and inside one only the process's first ask waits a round trip.
        let layout = crate::platform::display::compositor_layout_soon();
        if let Some(index) = pick(&self.sources, screen, layout.as_deref()) {
            return Ok(&self.sources[index]);
        }
        // Everything a report needs to tell which side had it wrong.
        tracing::warn!(
            screen = %screen.device_name,
            left = screen.left,
            top = screen.top,
            width = screen.width,
            height = screen.height,
            sources = ?self.sources,
            layout = layout.is_some(),
            layout_output = ?layout.as_deref().and_then(|layout| layout.output(&screen.device_name)),
            "门户共享的屏幕里找不到所选显示器"
        );
        Err(anyhow!(unmatched(
            &self.sources,
            screen,
            layout.as_deref(),
            super::wayland_session(),
        )))
    }
}

/// Why `pick` found no stream for `screen`, in words the user can act on.
///
/// On a scaled desktop a miss is rarely a monitor left out of the grant, and
/// telling the user so would send them to revoke and grant it again for
/// nothing; what was missing is the layout to pair by, or the name to look up.
fn unmatched(
    sources: &[Source],
    screen: &Screen,
    layout: Option<&CompositorLayout>,
    wayland: bool,
) -> String {
    if sources.len() > 1 && sources.iter().all(|s| s.position.is_none()) {
        return "门户没有给出共享屏幕的位置，无法区分共享的多块屏".to_owned();
    }
    match layout {
        None if wayland => "读不到合成器的输出布局（需要 Wayland 会话和 xdg-output），\
             缩放的桌面上无法确认门户共享的是哪块屏"
            .to_owned(),
        Some(layout) if layout.output(&screen.device_name).is_none() => format!(
            "合成器布局里没有名为 {} 的输出（XWayland 23.1 之前输出名不是连接器名），\
             缩放的桌面上无法确认门户共享的是哪块屏",
            screen.device_name
        ),
        _ => "所选显示器不在本机授权共享的屏幕中".to_owned(),
    }
}

/// Which of the portal's streams is `screen`, by index.
///
/// The portal describes a monitor the way the compositor lays it out, while
/// `Screen` carries the rectangle the display server gives the output. Those
/// are the same rectangle at scale 1 and disagree on a scaled desktop, and how
/// they disagree is the compositor's business: XWayland on Mutter and KWin
/// scales both the position and the size by the display's scale, while
/// xwayland-satellite keeps the logical position and sizes the output in its
/// own pixels. So the compositor's own layout — the space the portal describes
/// the monitor in — is what a monitor is paired by whenever it can be, through
/// the name the output is known by (`DP-1`, which RandR also reports as the
/// monitor name). The display server's rectangle is used only where there is
/// no layout that names the monitor.
///
/// Nothing here guesses: a monitor the layout cannot name, or a stream whose
/// rectangle is not the layout's, leaves the screen unmatched, and the caller
/// reports that rather than capturing a monitor nobody asked for.
fn pick(sources: &[Source], screen: &Screen, layout: Option<&CompositorLayout>) -> Option<usize> {
    // A layout that names the monitor is the only thing it is paired by. The
    // display server's rectangle lives in another space on a scaled desktop and
    // can be exactly another monitor's portal rectangle: XWayland on Mutter
    // scales a whole desktop of 200% and 100% monitors by 2, so the second of
    // two 200% monitors sits where the portal puts a 100% third one.
    if let Some(output) = layout.and_then(|layout| layout.output(&screen.device_name)) {
        if let Some(index) = sources.iter().position(|s| {
            s.position == Some((output.left, output.top))
                // The portal and xdg-output each make a whole number of a
                // fractional logical size, and not always the same way (niri's
                // portal truncates, smithay's xdg-output rounds), so a size may
                // be a unit off; a position is a whole layout coordinate and is
                // never rounded.
                && s.size.is_some_and(|(width, height)| {
                    (i64::from(width) - i64::from(output.width)).abs() <= 1
                        && (i64::from(height) - i64::from(output.height)).abs() <= 1
                })
        }) {
            return Some(index);
        }
    } else {
        // No layout to ask (an X11 session) or none that knows the name: the
        // two spaces agree only at scale 1, and that is all this can pair.
        let rectangle = (
            (screen.left, screen.top),
            (screen.width as i32, screen.height as i32),
        );
        if let Some(index) = sources
            .iter()
            .position(|s| s.position == Some(rectangle.0) && s.size == Some(rectangle.1))
        {
            return Some(index);
        }
    }
    match sources {
        // A single shared monitor without geometry can only be this one.
        [only] if only.position.is_none() => Some(0),
        _ => None,
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = on_portal(&self.runtime, self.session.close());
        let _ = &self.proxy;
    }
}

pub(super) struct Grab {
    stream: Stream,
    session: Arc<Session>,
}

impl Grab {
    pub fn open(screen: &Screen, cursor: bool) -> Result<Self> {
        let session = Session::shared(cursor)?;
        let node = session.source(screen)?.node;
        let fd = on_portal(
            &session.runtime,
            session
                .proxy
                .open_pipe_wire_remote(&session.session, OpenPipeWireRemoteOptions::default()),
        )
        .context("无法打开屏幕共享的 PipeWire 连接")?;
        let stream = Stream::connect(fd, node)?;
        Ok(Self { stream, session })
    }

    pub fn name(&self) -> &'static str {
        "XDG 门户 · PipeWire"
    }

    /// Whether the stream draws the cursor into its pictures.
    pub fn cursor(&self) -> bool {
        self.session.cursor
    }

    /// Output geometry request for the GPU converter: `(quality, maximum)`.
    pub fn set_target(&self, target: Option<(i32, (u32, u32))>) {
        self.stream.set_target(target);
    }

    pub fn next(&mut self, timeout: Duration) -> Result<Option<Picture>> {
        self.stream.next(timeout)
    }
}

#[cfg(test)]
mod tests {
    use super::{Source, pick, unmatched};
    use crate::media::capture::Screen;
    use crate::platform::{CompositorLayout, CompositorOutput, Rotation};

    /// A stream as the portal described it.
    fn stream(x: i32, y: i32, width: i32, height: i32) -> Source {
        Source {
            node: 0,
            position: Some((x, y)),
            size: Some((width, height)),
        }
    }

    /// A monitor as the display server gives it to `Screen`.
    fn screen(device_name: &str, left: i32, top: i32, width: u32, height: u32) -> Screen {
        Screen {
            id: 0,
            device_name: device_name.to_owned(),
            display_name: device_name.to_owned(),
            width,
            height,
            left,
            top,
            primary: false,
            fps: 60,
            dpi_scale: None,
            hdr: false,
            adapter: 0,
            render_adapter: None,
            identity: None,
        }
    }

    /// The same monitor in the compositor's own layout, upright.
    fn output(name: &str, left: i32, top: i32, width: u32, height: u32) -> CompositorOutput {
        CompositorOutput {
            name: Some(name.to_owned()),
            left,
            top,
            width,
            height,
            rotation: Rotation::Normal,
        }
    }

    fn layout(outputs: Vec<CompositorOutput>) -> CompositorLayout {
        CompositorLayout { outputs }
    }

    /// GNOME 50 with three 4K monitors side by side, the first two at 200% and
    /// the third at 100%: XWayland scales the whole desktop by 2, so the second
    /// monitor's RandR rectangle is exactly the third one's portal rectangle.
    /// The layout names every monitor, and each must find its own stream.
    #[test]
    fn mixed_scales_pair_by_the_layout_even_where_a_rectangle_collides() {
        let sources = [
            stream(0, 0, 1920, 1080),
            stream(1920, 0, 1920, 1080),
            stream(3840, 0, 3840, 2160),
        ];
        let layout = layout(vec![
            output("DP-1", 0, 0, 1920, 1080),
            output("DP-2", 1920, 0, 1920, 1080),
            output("DP-3", 3840, 0, 3840, 2160),
        ]);
        let screens = [
            screen("DP-1", 0, 0, 3840, 2160),
            screen("DP-2", 3840, 0, 3840, 2160),
            screen("DP-3", 7680, 0, 7680, 4320),
        ];
        for (index, wanted) in screens.iter().enumerate() {
            assert_eq!(
                pick(&sources, wanted, Some(&layout)),
                Some(index),
                "{}",
                wanted.device_name
            );
        }
    }

    /// A desktop at scale 1 is described with one rectangle by both sides.
    #[test]
    fn an_unscaled_desktop_matches_the_display_servers_rectangle() {
        let sources = [stream(0, 0, 1920, 1080), stream(1920, 0, 1920, 1080)];
        assert_eq!(
            pick(&sources, &screen("DP-2", 1920, 0, 1920, 1080), None),
            Some(1)
        );
    }

    /// Three 4K monitors side by side at scale 2, as GNOME 50 and KDE lay them
    /// out: the portal reports the logical rectangles, XWayland scales both the
    /// position and the size. Matching the second monitor must find the second
    /// stream — the third one has the rectangle the display server reports for
    /// it, which is where pairing by shape used to land.
    #[test]
    fn a_scaled_desktop_matches_the_stream_by_the_compositors_layout() {
        let sources = [
            stream(0, 0, 1920, 1080),
            stream(1920, 0, 1920, 1080),
            stream(3840, 0, 1920, 1080),
        ];
        let layout = layout(vec![
            output("DP-1", 0, 0, 1920, 1080),
            output("DP-2", 1920, 0, 1920, 1080),
            output("DP-3", 3840, 0, 1920, 1080),
        ]);
        assert_eq!(
            pick(
                &sources,
                &screen("DP-2", 3840, 0, 3840, 2160),
                Some(&layout)
            ),
            Some(1)
        );
    }

    /// The desktop this went wrong on: niri behind xwayland-satellite, with a
    /// 5120x2160 monitor at scale 1.35 beside a 2560x1600 one at scale 1.6
    /// turned upright. The display server places an output at the compositor's
    /// logical position and sizes it in the output's own pixels, so on the
    /// fractionally scaled monitor the size alone gives it away.
    ///
    /// 5120 / 1.35 = 3792.59: niri's portal truncates it to 3792 (its
    /// `logical_output()` casts), while smithay's xdg-output rounds it to 3793,
    /// so the two sides really do disagree by a unit here.
    #[test]
    fn a_fractionally_scaled_monitor_matches_by_its_name() {
        let sources = [stream(0, 0, 1000, 1600), stream(1000, 0, 3792, 1600)];
        let layout = layout(vec![
            CompositorOutput {
                rotation: Rotation::Rotate90,
                ..output("DP-3", 0, 0, 1000, 1600)
            },
            output("DP-1", 1000, 0, 3793, 1600),
        ]);
        assert_eq!(
            pick(
                &sources,
                &screen("DP-1", 1000, 0, 5120, 2160),
                Some(&layout)
            ),
            Some(1)
        );
        // The turned monitor is described with its sides swapped by the display
        // server, and the layout still names it: no second orientation to try.
        assert_eq!(
            pick(&sources, &screen("DP-3", 0, 0, 1600, 2560), Some(&layout)),
            Some(0)
        );
    }

    /// The portal and the layout may round a fractional size differently, so a
    /// unit either way still pairs; a position is exact, since two monitors can
    /// sit a unit apart.
    #[test]
    fn a_rounded_size_pairs_but_a_shifted_position_does_not() {
        let layout = layout(vec![output("DP-2", 1920, 0, 1920, 1080)]);
        let second = screen("DP-2", 1920, 0, 1920, 1080);
        assert_eq!(
            pick(&[stream(1920, 0, 1919, 1079)], &second, Some(&layout)),
            Some(0)
        );
        assert_eq!(
            pick(&[stream(1921, 0, 1920, 1080)], &second, Some(&layout)),
            None
        );
    }

    /// A desktop the two spaces disagree about, with no layout to settle it,
    /// stays unmatched: the caller reports it instead of capturing a monitor
    /// that may be someone else's screen.
    #[test]
    fn a_scaled_desktop_without_a_layout_is_not_guessed_at() {
        let sources = [stream(0, 0, 1920, 1080), stream(3840, 0, 1920, 1080)];
        let wanted = screen("DP-2", 3840, 0, 3840, 2160);
        assert_eq!(pick(&sources, &wanted, None), None);
        // XWayland before 23.1 names every output `XWAYLAND0…` instead of the
        // connector, so there is nothing to look up.
        let layout = layout(vec![output("DP-1", 0, 0, 1920, 1080)]);
        assert_eq!(
            pick(
                &sources,
                &screen("XWAYLAND1", 3840, 0, 3840, 2160),
                Some(&layout)
            ),
            None
        );
    }

    /// A single shared monitor the portal left without geometry can only be the
    /// one that was asked for; two of them are ambiguous.
    #[test]
    fn a_lone_stream_without_geometry_is_the_monitor_asked_for() {
        let wanted = screen("DP-1", 1000, 0, 5120, 2160);
        let only = [Source {
            node: 0,
            position: None,
            size: None,
        }];
        assert_eq!(pick(&only, &wanted, None), Some(0));
        let two = [
            Source {
                node: 0,
                position: None,
                size: None,
            },
            Source {
                node: 1,
                position: None,
                size: None,
            },
        ];
        assert_eq!(pick(&two, &wanted, None), None);
    }

    /// Each way of missing is told apart, so the user is not sent to grant a
    /// monitor again when the grant was never the problem.
    #[test]
    fn a_miss_says_what_was_missing() {
        let sources = [stream(0, 0, 1920, 1080)];
        let wanted = screen("DP-2", 3840, 0, 3840, 2160);
        assert!(unmatched(&sources, &wanted, None, true).contains("读不到合成器的输出布局"));
        let other = layout(vec![output("DP-1", 0, 0, 1920, 1080)]);
        let named = unmatched(&sources, &wanted, Some(&other), true);
        assert!(named.contains("没有名为 DP-2 的输出"), "{named}");
        // An X11 session has no layout to read, and the display server's
        // rectangle is the portal's: a miss there is a monitor not shared.
        assert_eq!(
            unmatched(&sources, &wanted, None, false),
            "所选显示器不在本机授权共享的屏幕中"
        );
        let knows = layout(vec![output("DP-2", 1920, 0, 1920, 1080)]);
        assert_eq!(
            unmatched(&sources, &wanted, Some(&knows), true),
            "所选显示器不在本机授权共享的屏幕中"
        );
        let blind = [
            Source {
                node: 0,
                position: None,
                size: None,
            },
            Source {
                node: 1,
                position: None,
                size: None,
            },
        ];
        assert!(unmatched(&blind, &wanted, Some(&knows), true).contains("没有给出共享屏幕的位置"));
    }
}
