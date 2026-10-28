//! Shared annotation editor: strokes, shapes, whiteboards and history,
//! applied once the host confirms them (`ANNOTATION` requests).
use super::*;
use crate::protocol::annotation::{self as ann, Op};
use board::{Board, BoardEdit};
use std::collections::{BTreeSet, HashMap};
use std::time::Duration;

mod board;
mod transport;
use transport::InFlight;

const MAX_POINTS: usize = 32_768;
const MAX_STROKES: usize = 256;
const MAX_PENDING: usize = 512;
const MAX_HISTORY: usize = 32;
const BOARD_SLOT_IDS: u32 = 128;
const BOARD_IDS: u32 = 256 * BOARD_SLOT_IDS;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Point {
    pub x: f32,
    pub y: f32,
}
impl Point {
    fn valid(self) -> bool {
        self.x.is_finite()
            && self.y.is_finite()
            && (0.0..=1.0).contains(&self.x)
            && (0.0..=1.0).contains(&self.y)
    }
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Style {
    pub argb: u32,
    pub width: f32,
}
impl Default for Style {
    fn default() -> Self {
        Self {
            argb: 0xffff4444,
            width: 3.0,
        }
    }
}
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Stroke {
    pub id: u32,
    pub screen: i32,
    pub points: Vec<Point>,
    pub style: Style,
}
#[derive(Clone)]
enum Edit {
    Add(Stroke),
    Remove(Stroke),
    Clear(Vec<Stroke>),
}
#[derive(Clone, Copy)]
enum Direction {
    Forward,
    Undo,
    Redo,
}
struct Editing {
    edit: Edit,
    direction: Direction,
    remaining: usize,
    clears_boards: bool,
}
struct Drawing {
    owner: u64,
    stroke: Stroke,
    sent: usize,
    finished: bool,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum DrawingKind {
    Stroke,
    Shape,
}
struct LiveShape {
    owner: u64,
    stroke: Stroke,
    revision: u64,
    sent_revision: u64,
    shown: bool,
    finish: Option<bool>,
    preview_submissions: u32,
}
#[derive(Clone, Copy)]
enum Pending {
    Toggle(bool),
    Stroke(u32),
    Edit,
    Shape,
    Board,
}

pub(super) struct Annotation {
    owners: BTreeSet<u64>,
    generation: u64,
    next_id: u32,
    pub(super) enabled: bool,
    pending: HashMap<u32, InFlight>,
    drawing: Option<Drawing>,
    live_shape: Option<LiveShape>,
    finishing: VecDeque<Stroke>,
    strokes: Vec<Stroke>,
    undo: Vec<Edit>,
    redo: Vec<Edit>,
    editing: Option<Editing>,
    boards: HashMap<i32, Board>,
    board_edit: Option<BoardEdit>,
    uncertain: bool,
    error: Option<String>,
}
impl Default for Annotation {
    fn default() -> Self {
        Self {
            owners: BTreeSet::new(),
            generation: 0,
            next_id: BOARD_IDS + 1,
            enabled: false,
            pending: HashMap::new(),
            drawing: None,
            live_shape: None,
            finishing: VecDeque::new(),
            strokes: Vec::new(),
            undo: Vec::new(),
            redo: Vec::new(),
            editing: None,
            boards: HashMap::new(),
            board_edit: None,
            uncertain: false,
            error: None,
        }
    }
}
impl Annotation {
    pub(super) fn disconnect(&mut self) {
        self.next_id = BOARD_IDS + 1;
        self.generation = self.generation.wrapping_add(1);
        self.enabled = false;
        self.pending.clear();
        self.drawing = None;
        self.live_shape = None;
        self.finishing.clear();
        self.editing = None;
        self.boards.clear();
        self.board_edit = None;
        self.strokes.clear();
        self.undo.clear();
        self.redo.clear();
        self.uncertain = false;
        self.error = None;
    }
    pub(super) fn toggling(&self) -> bool {
        self.pending
            .values()
            .any(|p| matches!(p.effect, Pending::Toggle(..)))
    }
    fn busy(&self) -> bool {
        self.drawing.is_some()
            || self.live_shape.is_some()
            || self.editing.is_some()
            || !self.pending.is_empty()
    }
    fn push_history(&mut self, edit: Edit) {
        if matches!(&edit, Edit::Clear(strokes) if strokes.is_empty()) {
            return;
        }
        if self.undo.len() == MAX_HISTORY {
            self.undo.remove(0);
        }
        self.undo.push(edit);
        self.redo.clear();
    }
    fn commit_drawing(&mut self) {
        if self
            .drawing
            .as_ref()
            .is_some_and(|d| d.finished && d.sent == d.stroke.points.len())
        {
            self.finishing
                .push_back(self.drawing.take().unwrap().stroke);
        }
        while self.finishing.front().is_some_and(|stroke| {
            !self
                .pending
                .values()
                .any(|p| matches!(p.effect, Pending::Stroke(id) if id == stroke.id))
        }) {
            let stroke = self.finishing.pop_front().unwrap();
            self.push_history(Edit::Add(stroke.clone()));
            self.strokes.push(stroke);
        }
    }
    fn finish_live_shape(&mut self) {
        if self
            .pending
            .values()
            .any(|p| matches!(p.effect, Pending::Shape))
        {
            return;
        }
        let done = self
            .live_shape
            .as_ref()
            .is_some_and(|shape| match shape.finish {
                Some(false) => !shape.shown,
                Some(true) => shape.sent_revision == shape.revision,
                None => false,
            });
        if !done {
            return;
        }
        let shape = self.live_shape.take().unwrap();
        if shape.finish == Some(true) {
            tracing::info!(
                screen = shape.stroke.screen,
                stroke = shape.stroke.id,
                preview_submissions = shape.preview_submissions,
                "live annotation shape committed"
            );
            self.finishing.push_back(shape.stroke);
            self.commit_drawing();
        }
    }
    fn uncertain(&mut self, message: String) {
        self.generation = self.generation.wrapping_add(1);
        self.pending.clear();
        self.drawing = None;
        self.live_shape = None;
        self.finishing.clear();
        self.editing = None;
        self.board_edit = None;
        self.undo.clear();
        self.redo.clear();
        self.uncertain = true;
        self.error = Some(message);
    }
    fn apply_edit(&mut self, edit: &Edit, reverse: bool) {
        match (edit, reverse) {
            (Edit::Add(s), false) | (Edit::Remove(s), true) => self.strokes.push(s.clone()),
            (Edit::Add(s), true) | (Edit::Remove(s), false) => {
                self.strokes.retain(|v| v.id != s.id)
            }
            (Edit::Clear(_), false) => self.strokes.clear(),
            (Edit::Clear(strokes), true) => self.strokes = strokes.clone(),
        }
        self.strokes.sort_by_key(|s| s.id);
    }
    /// The host's answer to request `id`: done, or its refusal reason.
    fn complete(&mut self, id: u32, result: std::result::Result<(), String>) {
        let Some(request) = self.pending.remove(&id) else {
            return;
        };
        let p = request.effect;
        if let Err(reason) = result {
            let signed_out = reason == ann::reason::SIGNED_OUT.1;
            self.enabled = match p {
                Pending::Toggle(enable) => !enable && !signed_out,
                _ => self.enabled && !signed_out,
            };
            self.uncertain(transport::refusal_text(&reason));
            return;
        }
        if let Pending::Toggle(enable) = p {
            self.enabled = enable;
            self.error = None;
            self.uncertain = false;
            return;
        }
        match p {
            Pending::Board => board::complete(self),
            Pending::Stroke(_) => self.commit_drawing(),
            Pending::Shape => self.finish_live_shape(),
            Pending::Edit => {
                if let Some(edit) = &mut self.editing {
                    edit.remaining = edit.remaining.saturating_sub(1);
                    if edit.remaining == 0 {
                        let edit = self.editing.take().unwrap();
                        if edit.clears_boards {
                            self.boards.clear();
                        }
                        self.apply_edit(&edit.edit, matches!(edit.direction, Direction::Undo));
                        match edit.direction {
                            Direction::Forward => self.push_history(edit.edit),
                            Direction::Undo => {
                                self.undo.pop();
                                self.redo.push(edit.edit);
                            }
                            Direction::Redo => {
                                self.redo.pop();
                                self.undo.push(edit.edit);
                            }
                        }
                        self.uncertain = false;
                        self.error = None;
                    }
                }
            }
            Pending::Toggle(..) => unreachable!(),
        }
    }
}
pub(crate) struct Snapshot {
    pub supported: bool,
    pub enabled: bool,
    pub toggling: bool,
    pub busy: bool,
    pub board_busy: bool,
    pub can_undo: bool,
    pub can_redo: bool,
    pub uncertain: bool,
    pub error: Option<String>,
    pub generation: u64,
}

impl StreamControlHandle {
    pub(crate) fn annotation_register(&self, owner: u64) {
        lock(&self.shared).annotation.owners.insert(owner);
    }
    pub(crate) fn annotation_same_session(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.shared, &other.shared)
    }
    pub(crate) fn annotation_leave(&self, owner: u64) {
        self.annotation_shape_end(owner, false);
        self.annotation_finish(owner);
        let mut s = lock(&self.shared);
        s.annotation.owners.remove(&owner);
        if s.annotation.owners.is_empty()
            && (s.annotation.enabled || s.annotation.toggling() || s.annotation.uncertain)
        {
            let _ = self.annotation_toggle_locked(&mut s, false);
        }
    }
    pub(crate) fn annotation_snapshot(&self) -> Snapshot {
        let s = lock(&self.shared);
        let a = &s.annotation;
        Snapshot {
            supported: annotation_supported(&s),
            enabled: a.enabled,
            toggling: a.toggling(),
            busy: a.busy(),
            board_busy: a.board_edit.is_some(),
            can_undo: !a.busy() && !a.uncertain && !a.undo.is_empty(),
            can_redo: !a.busy() && !a.uncertain && !a.redo.is_empty(),
            uncertain: a.uncertain,
            error: a.error.clone(),
            generation: a.generation,
        }
    }
    pub(crate) fn annotation_line_scale(&self, screen: i32, rendered_width: f32) -> f32 {
        let s = lock(&self.shared);
        s.screens.iter().find(|v| v.id == screen).map_or(1.0, |v| {
            let physical = v.width.max(1) as f32;
            let dpi = if v.display.current_dpi == 0 {
                100
            } else {
                v.display.current_dpi
            };
            dpi as f32 / 100.0 * rendered_width / physical
        })
    }
    pub(crate) fn annotation_strokes(&self, screen: i32) -> Vec<Stroke> {
        lock(&self.shared)
            .annotation
            .strokes
            .iter()
            .filter(|s| s.screen == screen)
            .cloned()
            .collect()
    }
    pub(crate) fn annotation_toggle(&self, enable: bool) -> Result<()> {
        // Explicit annotation entry revokes remote input, including already-held keys.
        if enable {
            self.suspend_mouse_control()?;
        }
        let mut s = lock(&self.shared);
        if s.annotation.toggling() {
            bail!("{}", t().features.annotation_toggling);
        }
        if s.annotation.enabled == enable && !s.annotation.uncertain {
            return Ok(());
        }
        if enable && s.annotation.uncertain {
            bail!("{}", t().features.annotation_state_unconfirmed);
        }
        self.annotation_toggle_locked(&mut s, enable)
    }
    fn annotation_toggle_locked(&self, s: &mut StreamControlState, enable: bool) -> Result<()> {
        ensure_ready(s)?;
        if !annotation_supported(s) {
            bail!("{}", t().features.annotation_unsupported);
        }
        s.annotation.disconnect();
        s.annotation.enabled = enable;
        self.send_op(s, Op::Show(enable), Pending::Toggle(enable))
    }
    pub(crate) fn annotation_begin(
        &self,
        owner: u64,
        screen: i32,
        points: Vec<Point>,
        style: Style,
    ) -> Result<()> {
        self.begin_annotation(owner, screen, points, style, DrawingKind::Stroke)
    }
    pub(crate) fn annotation_shape_begin(
        &self,
        owner: u64,
        screen: i32,
        points: Vec<Point>,
        style: Style,
    ) -> Result<()> {
        self.begin_annotation(owner, screen, points, style, DrawingKind::Shape)
    }
    fn begin_annotation(
        &self,
        owner: u64,
        screen: i32,
        points: Vec<Point>,
        style: Style,
        kind: DrawingKind,
    ) -> Result<()> {
        let live = kind != DrawingKind::Stroke;
        let mut s = lock(&self.shared);
        ensure_ready(&s)?;
        if !s.annotation.enabled || s.annotation.toggling() || s.annotation.uncertain {
            bail!("{}", t().features.annotation_not_ready);
        }
        if s.annotation.drawing.is_some()
            || s.annotation.live_shape.is_some()
            || s.annotation.editing.is_some()
            || s.annotation.board_edit.is_some()
        {
            bail!("{}", t().features.annotation_stroke_in_progress);
        }
        if !s.annotation.owners.contains(&owner)
            || !s.screens.iter().any(|v| v.id == screen && screen >= 0)
        {
            bail!("{}", t().features.annotation_screen_changed);
        }
        if points.is_empty()
            || !points.iter().all(|p| p.valid())
            || !style.width.is_finite()
            || style.width <= 0.0
            || style.width > 64.0
        {
            bail!("{}", t().features.annotation_invalid_parameters);
        }
        let a = &mut s.annotation;
        if a.strokes.len() + a.finishing.len() >= MAX_STROKES
            || a.strokes
                .iter()
                .chain(a.finishing.iter())
                .map(|s| s.points.len())
                .sum::<usize>()
                + points.len()
                > MAX_POINTS
        {
            bail!("{}", t().features.annotation_limit_reached);
        }
        let id = a.next_id;
        a.next_id = a
            .next_id
            .checked_add(1)
            .ok_or_else(|| anyhow!("{}", t().features.annotation_ids_exhausted))?;
        a.drawing = Some(Drawing {
            owner,
            stroke: Stroke {
                id,
                screen,
                points,
                style,
            },
            sent: 0,
            finished: false,
        });
        if live {
            let drawing = a.drawing.take().unwrap();
            a.live_shape = Some(LiveShape {
                owner,
                stroke: drawing.stroke,
                revision: 1,
                sent_revision: 0,
                shown: false,
                finish: None,
                preview_submissions: 0,
            });
        }
        a.error = None;
        Ok(())
    }
    pub(crate) fn annotation_shape_update(&self, owner: u64, points: Vec<Point>) -> Result<()> {
        let mut s = lock(&self.shared);
        let stored = s
            .annotation
            .strokes
            .iter()
            .chain(s.annotation.finishing.iter())
            .map(|s| s.points.len())
            .sum::<usize>();
        if points.is_empty()
            || points.len() > 4096
            || stored + points.len() > MAX_POINTS
            || !points.iter().all(|p| p.valid())
        {
            bail!("{}", t().features.annotation_shape_out_of_range);
        }
        if let Some(shape) = &mut s.annotation.live_shape {
            if shape.owner == owner && shape.finish.is_none() && shape.stroke.points != points {
                shape.stroke.points = points;
                shape.revision = shape.revision.wrapping_add(1);
            }
        }
        Ok(())
    }
    pub(crate) fn annotation_shape_end(&self, owner: u64, commit: bool) {
        let mut s = lock(&self.shared);
        if let Some(shape) = &mut s.annotation.live_shape {
            if shape.owner != owner {
                return;
            }
            if shape.finish != Some(false) {
                shape.finish = Some(commit);
            }
        }
        self.drive_live_shape(&mut s);
    }
    fn drive_live_shape(&self, s: &mut StreamControlState) {
        let Some(shape) = s.annotation.live_shape.as_ref() else {
            return;
        };
        if !s
            .screens
            .iter()
            .any(|screen| screen.id == shape.stroke.screen)
        {
            s.annotation
                .uncertain(t().features.annotation_screen_removed.into());
            return;
        }
        // Up to four replacements in flight; later pointer updates replace
        // the desired geometry.
        if s.annotation
            .pending
            .values()
            .filter(|p| matches!(p.effect, Pending::Shape))
            .count()
            >= 4
        {
            return;
        }
        let cancel = shape.finish == Some(false);
        if (cancel || shape.sent_revision != shape.revision)
            && s.annotation.pending.len() < MAX_PENDING
        {
            let op = if cancel {
                shape.shown.then(|| Op::Erase(stroke_id(&shape.stroke)))
            } else {
                Some(Op::Put(encoded_stroke(&shape.stroke, &shape.stroke.points)))
            };
            if let Some(op) = op
                && let Err(error) = self.send_op(s, op, Pending::Shape)
            {
                s.annotation.uncertain(error.to_string());
                return;
            }
            if let Some(shape) = &mut s.annotation.live_shape {
                shape.shown = !cancel;
                shape.sent_revision = shape.revision;
                if shape.finish.is_none() {
                    shape.preview_submissions = shape.preview_submissions.saturating_add(1);
                }
            }
        }
        s.annotation.finish_live_shape();
    }
    pub(crate) fn annotation_append(&self, owner: u64, point: Point) {
        if !point.valid() {
            return;
        }
        let mut s = lock(&self.shared);
        let stored = s
            .annotation
            .strokes
            .iter()
            .chain(s.annotation.finishing.iter())
            .map(|s| s.points.len())
            .sum::<usize>();
        if let Some(d) = &mut s.annotation.drawing {
            if d.owner == owner && !d.finished && d.stroke.points.last() != Some(&point) {
                if d.stroke.points.len() + stored < MAX_POINTS {
                    d.stroke.points.push(point);
                } else {
                    d.finished = true;
                    s.annotation.error = Some(t().features.annotation_stroke_limit_reached.into());
                }
            }
        }
    }
    pub(crate) fn annotation_finish(&self, owner: u64) {
        let mut s = lock(&self.shared);
        if let Some(d) = &mut s.annotation.drawing {
            if d.owner != owner {
                return;
            }
            d.finished = true;
            self.flush_annotation(&mut s);
        }
    }
    fn flush_annotation(&self, s: &mut StreamControlState) {
        if s.annotation
            .drawing
            .as_ref()
            .is_some_and(|d| !s.screens.iter().any(|v| v.id == d.stroke.screen))
        {
            s.annotation
                .uncertain(t().features.annotation_screen_removed.into());
            return;
        }
        if let Some(d) = &s.annotation.drawing {
            if d.sent < d.stroke.points.len() {
                let end = (d.sent + ann::MAX_DRAW_POINTS).min(d.stroke.points.len());
                let op = Op::Draw(encoded_stroke(&d.stroke, &d.stroke.points[d.sent..end]));
                let id = d.stroke.id;
                match self.send_op(s, op, Pending::Stroke(id)) {
                    Ok(()) => {
                        if let Some(d) = &mut s.annotation.drawing {
                            d.sent = end;
                        }
                    }
                    Err(e) => s.annotation.uncertain(e.to_string()),
                }
            }
        }
        s.annotation.commit_drawing();
    }
    pub(crate) fn annotation_tick(&self) {
        let mut s = lock(&self.shared);
        self.expire_requests(&mut s);
        let timed_out = s
            .annotation
            .pending
            .values()
            .find(|p| p.sent.elapsed() >= Duration::from_secs(3));
        if let Some(request) = timed_out {
            if let Pending::Toggle(enable) = request.effect {
                s.annotation.enabled = !enable;
            }
            s.annotation
                .uncertain(t().features.annotation_unconfirmed.into());
        }
        self.flush_annotation(&mut s);
        self.drive_live_shape(&mut s);
        self.refresh_board(&mut s);
    }
    pub(crate) fn annotation_undo(&self) -> Result<()> {
        self.annotation_history(false)
    }
    pub(crate) fn annotation_redo(&self) -> Result<()> {
        self.annotation_history(true)
    }
    fn annotation_history(&self, redo: bool) -> Result<()> {
        let mut s = lock(&self.shared);
        if s.annotation.uncertain {
            bail!("{}", t().features.annotation_strokes_unconfirmed);
        }
        let edit = if redo {
            s.annotation.redo.last()
        } else {
            s.annotation.undo.last()
        }
        .cloned();
        if let Some(edit) = edit {
            self.edit_annotation(
                &mut s,
                edit,
                if redo {
                    Direction::Redo
                } else {
                    Direction::Undo
                },
            )?;
        }
        Ok(())
    }
    pub(crate) fn annotation_erase(&self, id: u32) -> Result<()> {
        let mut s = lock(&self.shared);
        if s.annotation.uncertain {
            bail!("{}", t().features.annotation_strokes_unconfirmed);
        }
        if let Some(stroke) = s.annotation.strokes.iter().find(|v| v.id == id).cloned() {
            self.edit_annotation(&mut s, Edit::Remove(stroke), Direction::Forward)?;
        }
        Ok(())
    }
    pub(crate) fn annotation_clear(&self) -> Result<()> {
        let mut s = lock(&self.shared);
        let old = if s.annotation.uncertain {
            Vec::new()
        } else {
            s.annotation.strokes.clone()
        };
        self.edit_annotation(&mut s, Edit::Clear(old), Direction::Forward)
    }
    fn edit_annotation(
        &self,
        s: &mut StreamControlState,
        edit: Edit,
        direction: Direction,
    ) -> Result<()> {
        ensure_ready(s)?;
        if !s.annotation.enabled || s.annotation.busy() {
            bail!("{}", t().features.annotation_not_ready);
        }
        let reverse = matches!(direction, Direction::Undo);
        let clears_boards = matches!(&edit, Edit::Clear(_)) && s.annotation.uncertain;
        let put = |stroke: &Stroke| encoded_stroke(stroke, &stroke.points);
        let op = match (&edit, reverse) {
            (Edit::Clear(_), false) => Op::Clear(clears_boards),
            (Edit::Add(stroke), true) | (Edit::Remove(stroke), false) => {
                Op::Erase(stroke_id(stroke))
            }
            (Edit::Add(stroke), false) | (Edit::Remove(stroke), true) => Op::Put(put(stroke)),
            (Edit::Clear(strokes), true) => {
                if strokes.is_empty() {
                    return Ok(());
                }
                Op::Restore(ann::Strokes {
                    strokes: strokes.iter().map(put).collect(),
                })
            }
        };
        s.annotation.editing = Some(Editing {
            clears_boards,
            edit,
            direction,
            remaining: 1,
        });
        s.annotation.error = None;
        if let Err(error) = self.send_op(s, op, Pending::Edit) {
            s.annotation.uncertain(error.to_string());
            return Err(error);
        }
        Ok(())
    }
}
fn annotation_supported(s: &StreamControlState) -> bool {
    s.peer.has(crate::protocol::peer::feature::ANNOTATION)
        && negotiated(s)
        && s.text_channel_open
}
fn encoded_stroke(stroke: &Stroke, points: &[Point]) -> ann::Stroke {
    ann::Stroke {
        id: stroke.id,
        screen: stroke.screen,
        points: points.iter().map(|p| ann::Point { x: p.x, y: p.y }).collect(),
        width: stroke.style.width,
        color: stroke.style.argb,
    }
}
fn stroke_id(stroke: &Stroke) -> ann::StrokeId {
    ann::StrokeId {
        id: stroke.id,
        screen: stroke.screen,
    }
}
