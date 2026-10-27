//! Per-screen whiteboard background, sent as a native annotation command.
use super::*;

#[derive(Clone)]
pub(super) struct Board {
    color: u32,
    metrics: Metrics,
}
pub(super) struct BoardEdit {
    screen: i32,
    value: Option<Board>,
    remaining: usize,
}

pub(super) fn complete(a: &mut Annotation) {
    let Some(edit) = &mut a.board_edit else {
        return;
    };
    edit.remaining = edit.remaining.saturating_sub(1);
    if edit.remaining == 0 {
        let edit = a.board_edit.take().unwrap();
        if let Some(board) = edit.value {
            a.boards.insert(edit.screen, board);
        } else {
            a.boards.remove(&edit.screen);
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Metrics {
    width: u32,
    height: u32,
    dpi: u32,
}
impl Metrics {
    fn from_display(display: &ScreenBaseline) -> Self {
        Self {
            width: display.width.max(1),
            height: display.height.max(1),
            dpi: if display.display.current_dpi == 0 {
                100
            } else {
                display.display.current_dpi
            },
        }
    }
}

impl StreamControlHandle {
    pub(crate) fn annotation_board_color(&self, screen: i32) -> Option<[u8; 3]> {
        lock(&self.shared)
            .annotation
            .boards
            .get(&screen)
            .map(|board| {
                let [_, r, g, b] = board.color.to_be_bytes();
                [r, g, b]
            })
    }

    pub(crate) fn annotation_board(
        &self,
        owner: u64,
        screen: i32,
        color: Option<[u8; 3]>,
    ) -> Result<()> {
        let mut s = lock(&self.shared);
        ensure_ready(&s)?;
        if !s.annotation.owners.contains(&owner) {
            bail!("{}", t().features.annotation_window_closed);
        }
        self.set_board_locked(&mut s, screen, color)
    }

    fn set_board_locked(
        &self,
        s: &mut StreamControlState,
        screen: i32,
        color: Option<[u8; 3]>,
    ) -> Result<()> {
        if !s.annotation.enabled || s.annotation.toggling() || s.annotation.uncertain {
            bail!("{}", t().features.annotation_not_ready);
        }
        if s.annotation.busy() {
            bail!("{}", t().features.annotation_busy);
        }
        let display = s
            .screens
            .iter()
            .find(|v| v.id == screen && screen >= 0)
            .ok_or_else(|| anyhow!("{}", t().features.annotation_screen_changed))?;
        let metrics = Metrics::from_display(display);
        let wanted = color.map(|rgb| u32::from_be_bytes([255, rgb[0], rgb[1], rgb[2]]));
        let old = s.annotation.boards.get(&screen);
        if s.annotation.native_token.is_some() {
            if old.map(|b| b.color) == wanted {
                if let Some(board) = s.annotation.boards.get_mut(&screen) {
                    board.metrics = metrics;
                }
                return Ok(());
            }
            s.annotation.board_edit = Some(BoardEdit {
                screen,
                value: wanted.map(|color| Board { color, metrics }),
                remaining: 1,
            });
            let command = crate::protocol::annotation::Command::new(
                crate::protocol::annotation::Operation::Board(crate::protocol::annotation::Board {
                    screen,
                    color: wanted,
                }),
            );
            if let Err(error) = self.send_native_draw(s, command, Pending::Board) {
                s.annotation.uncertain(error.to_string());
                return Err(error);
            }
            return Ok(());
        }
        bail!("{}", t().features.annotation_remote_not_ready)
    }

    pub(super) fn refresh_board(&self, s: &mut StreamControlState) {
        if !s.annotation.enabled || s.annotation.uncertain || s.annotation.busy() {
            return;
        }
        s.annotation
            .boards
            .retain(|screen, _| s.screens.iter().any(|v| v.id == *screen));
        let changed = s.annotation.boards.iter().find_map(|(screen, board)| {
            s.screens
                .iter()
                .find(|v| v.id == *screen)
                .filter(|v| Metrics::from_display(v) != board.metrics)
                .map(|_| {
                    let [_, r, g, b] = board.color.to_be_bytes();
                    (*screen, [r, g, b])
                })
        });
        if let Some((screen, color)) = changed
            && let Err(e) = self.set_board_locked(s, screen, Some(color))
        {
            s.annotation.uncertain(e.to_string());
        }
    }
}
