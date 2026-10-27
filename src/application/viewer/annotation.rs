use crate::features::stream_control::{
    StreamControlHandle,
    annotation::{Point, Stroke as InkStroke, Style},
};
use crate::i18n::t;
use crate::ui::{controls, theme};
use controls::AnnotationIcon as Tool;
use egui::{Pos2, Rect, Sense, vec2};

struct Gesture {
    tool: Tool,
    screen: i32,
    rect: Rect,
    start: Pos2,
    end: Pos2,
    button: egui::PointerButton,
    raw_end: Pos2,
}
pub(super) struct AnnotationUi {
    control: StreamControlHandle,
    owner: u64,
    open: bool,
    tool: Tool,
    rgb: [u8; 3],
    opacity: u8,
    width: f32,
    board_color: [u8; 3],
    gesture: Option<Gesture>,
    generation: u64,
    error: Option<String>,
    clear_confirm: bool,
    panel_position: Option<Pos2>,
    panel_drag: Option<(Pos2, Pos2)>,
    panel_size: egui::Vec2,
}
impl AnnotationUi {
    pub fn new(control: StreamControlHandle, owner: u64) -> Self {
        control.annotation_register(owner);
        Self {
            control,
            owner,
            open: false,
            tool: Tool::Pen,
            rgb: [255, 68, 68],
            opacity: 100,
            width: 3.,
            board_color: theme::ANNOTATION_BOARD_COLORS[0],
            gesture: None,
            generation: 0,
            error: None,
            clear_confirm: false,
            panel_position: None,
            panel_drag: None,
            panel_size: vec2(
                theme::ANNOTATION_WIDTH + 2. * theme::ANNOTATION_PANEL_MARGIN as f32,
                124.,
            ),
        }
    }
    pub fn bind(&mut self, control: StreamControlHandle) {
        self.finish();
        if !self.control.annotation_same_session(&control) {
            self.control.annotation_leave(self.owner);
            control.annotation_register(self.owner);
            self.control = control;
            self.open = false;
            self.error = None;
        }
    }
    pub fn toggle(&mut self) {
        self.finish();
        self.clear_confirm = false;
        self.panel_drag = None;
        self.open = !self.open;
        if self.open && !self.control.annotation_snapshot().enabled {
            self.error = self
                .control
                .annotation_toggle(true)
                .err()
                .map(|e| e.to_string());
        }
    }
    pub fn finish(&mut self) {
        if let Some(gesture) = self.gesture.take() {
            if is_shape(gesture.tool) {
                self.control.annotation_shape_end(self.owner, false);
            } else if gesture.tool == Tool::Pen {
                self.control.annotation_finish(self.owner);
            }
        }
    }
    fn style(&self) -> Style {
        Style {
            argb: ((u32::from(self.opacity) * 255 / 100) << 24)
                | (u32::from(self.rgb[0]) << 16)
                | (u32::from(self.rgb[1]) << 8)
                | u32::from(self.rgb[2]),
            width: self.width,
        }
    }
    pub fn draw(
        &mut self,
        ui: &mut egui::Ui,
        screen: i32,
        rect: Option<Rect>,
        blocked: bool,
        focused: bool,
    ) {
        let ctx = ui.ctx().clone();
        let snapshot = self.control.annotation_snapshot();
        crate::ui::controls::observe_notice(
            ui.ctx(),
            "annotation-error",
            t().viewer.annotation_failed,
            crate::ui::controls::DialogIcon::Error,
            self.error
                .as_deref()
                .or(snapshot.error.as_deref().filter(|_| self.open)),
        );
        crate::ui::controls::progress_notice(
            &ctx,
            "annotation-progress",
            t().viewer.annotation_operation,
            if snapshot.board_busy {
                Some(t().viewer.updating_whiteboard)
            } else if snapshot.toggling {
                Some(t().viewer.switching_annotation)
            } else {
                None
            },
        );
        let board_color = self.control.annotation_board_color(screen);
        if snapshot.generation != self.generation {
            self.gesture = None;
            self.generation = snapshot.generation;
            self.clear_confirm = false;
        }
        if !snapshot.enabled {
            self.finish();
        }
        if snapshot.toggling || snapshot.busy {
            ctx.request_repaint_after(std::time::Duration::from_millis(16));
        }
        if !self.open && !snapshot.enabled {
            return;
        }
        let panel_was_open = self.open;
        let popup_was_open = ctx.any_popup_open();
        if !focused {
            self.panel_drag = None;
        }
        if self.open {
            let bounds = ui.available_rect_before_wrap().shrink(8.0);
            let initial = pos2(
                bounds.center().x - self.panel_size.x / 2.,
                bounds.bottom() - self.panel_size.y - 10.,
            );
            let position = panel_position(
                self.panel_position.unwrap_or(initial),
                self.panel_size,
                bounds,
            );
            let mut next_position = position;
            let panel = egui::Area::new(egui::Id::new("annotation-tools"))
                .order(egui::Order::Foreground)
                .fixed_pos(position)
                .movable(false)
                .constrain_to(bounds)
                .show(&ctx, |ui| {
                    controls::annotation_frame().show(ui, |ui| {
                        ui.set_width(theme::ANNOTATION_WIDTH);
                        ui.spacing_mut().interact_size.y = theme::ANNOTATION_HEADER_HEIGHT;
                        ui.spacing_mut().item_spacing.y = theme::ANNOTATION_ROW_GAP;
                        let (drag, close, undo, redo, end) =
                            controls::annotation_header(ui, snapshot.can_undo, snapshot.can_redo);
                        for event in ctx.input(|i| i.events.clone()) {
                            match event {
                                egui::Event::PointerButton {
                                    pos,
                                    button: egui::PointerButton::Primary,
                                    pressed: true,
                                    ..
                                } if focused
                                    && drag.rect.contains(pos)
                                    && ctx.layer_id_at(pos) == Some(drag.layer_id) =>
                                {
                                    self.finish();
                                    self.panel_drag = Some((pos, position));
                                }
                                egui::Event::PointerMoved(pos) => {
                                    if let Some((start, origin)) = self.panel_drag {
                                        next_position = origin + (pos - start);
                                    }
                                }
                                egui::Event::PointerButton {
                                    pos,
                                    button: egui::PointerButton::Primary,
                                    pressed: false,
                                    ..
                                } => {
                                    if let Some((start, origin)) = self.panel_drag.take() {
                                        next_position = origin + (pos - start);
                                    }
                                }
                                egui::Event::PointerGone => self.panel_drag = None,
                                _ => {}
                            }
                        }
                        if self.panel_drag.is_some() {
                            ctx.set_cursor_icon(egui::CursorIcon::Grabbing);
                        }
                        if close {
                            self.finish();
                            self.open = false;
                            self.clear_confirm = false;
                            self.panel_drag = None;
                        }
                        if end {
                            self.finish();
                            self.error = self
                                .control
                                .annotation_toggle(false)
                                .err()
                                .map(|e| e.to_string());
                            self.open = self.error.is_some();
                        }
                        if undo {
                            self.error =
                                self.control.annotation_undo().err().map(|e| e.to_string());
                        }
                        if redo {
                            self.error =
                                self.control.annotation_redo().err().map(|e| e.to_string());
                        }
                        controls::annotation_tool_frame().show(ui, |ui| {
                            ui.spacing_mut().item_spacing.x = theme::ANNOTATION_TOOL_GAP;
                            let t = &t().viewer;
                            let tools = [
                                (Tool::Pen, t.tool_pen),
                                (Tool::Line, t.tool_line),
                                (Tool::Arrow, t.tool_arrow),
                                (Tool::Rectangle, t.tool_rectangle),
                                (Tool::Ellipse, t.tool_ellipse),
                                (Tool::Eraser, t.tool_eraser),
                            ];
                            // Equal cells span the whole row.
                            let width = (ui.available_width()
                                - theme::ANNOTATION_TOOL_GAP * (tools.len() - 1) as f32)
                                / tools.len() as f32;
                            ui.horizontal(|ui| {
                                for (tool, name) in tools {
                                    if controls::annotation_tool_button(
                                        ui,
                                        tool,
                                        self.tool == tool,
                                        name,
                                        width,
                                    )
                                    .clicked()
                                    {
                                        self.finish();
                                        self.tool = tool;
                                    }
                                }
                            });
                        });
                        ui.horizontal(|ui| {
                            controls::annotation_color(ui, &mut self.rgb, &mut self.opacity);
                            controls::annotation_width(ui, &mut self.width);
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui
                                        .add_enabled_ui(snapshot.enabled && !snapshot.busy, |ui| {
                                            controls::annotation_button(
                                                ui,
                                                Tool::Clear,
                                                false,
                                                t().viewer.clear_annotations,
                                            )
                                        })
                                        .inner
                                        .clicked()
                                    {
                                        self.clear_confirm = true;
                                    }
                                    let action = ui
                                        .add_enabled_ui(
                                            snapshot.enabled
                                                && !snapshot.busy
                                                && !snapshot.uncertain,
                                            |ui| {
                                                controls::annotation_board(
                                                    ui,
                                                    board_color,
                                                    &mut self.board_color,
                                                )
                                            },
                                        )
                                        .inner;
                                    if let Some(color) = action {
                                        self.finish();
                                        self.error = self
                                            .control
                                            .annotation_board(self.owner, screen, color)
                                            .err()
                                            .map(|e| e.to_string());
                                    }
                                },
                            );
                        });
                    });
                });
            self.panel_size = panel.response.rect.size();
            self.panel_position = Some(panel_position(next_position, self.panel_size, bounds));
            if next_position != position {
                ctx.request_repaint();
            }
        }
        // The click that hides or ends the panel must not also reach the canvas.
        // On following frames visibility does not gate the active drawing tool.
        if panel_was_open && !self.open {
            return;
        }
        if self.clear_confirm
            && let Some(clear) = controls::annotation_clear_dialog(&ctx, snapshot.uncertain)
        {
            if clear {
                self.error = self.control.annotation_clear().err().map(|e| e.to_string());
            }
            self.clear_confirm = false;
        }
        let input_blocked = popup_was_open
            || self.panel_drag.is_some()
            || blocked
            || !focused
            || self.clear_confirm
            || ctx.any_popup_open()
            || ctx.text_edit_focused();
        if input_blocked
            || !snapshot.enabled
            || snapshot.toggling
            || snapshot.uncertain
            || snapshot.board_busy
        {
            self.finish();
            return;
        }
        let Some(rect) = rect.filter(|r| r.width() > 0. && r.height() > 0.) else {
            self.finish();
            return;
        };
        if self
            .gesture
            .as_ref()
            .is_some_and(|g| g.screen != screen || g.rect != rect)
        {
            self.finish();
        }
        let response = ui.interact(
            rect,
            ui.id().with("annotation-canvas"),
            Sense::click_and_drag(),
        );
        if response.hovered() {
            ctx.set_cursor_icon(egui::CursorIcon::Crosshair);
        }
        let events = ctx.input(|i| i.events.clone());
        for event in events {
            match event {
                egui::Event::Key {
                    key,
                    pressed: true,
                    repeat: false,
                    modifiers,
                    ..
                } if modifiers.ctrl && !modifiers.alt => {
                    if self.gesture.is_none() {
                        let result = match key {
                            egui::Key::Z if modifiers.shift => Some(self.control.annotation_redo()),
                            egui::Key::Z => Some(self.control.annotation_undo()),
                            egui::Key::Y => Some(self.control.annotation_redo()),
                            _ => None,
                        };
                        if let Some(result) = result {
                            self.error = result.err().map(|e| e.to_string());
                        }
                    }
                }
                egui::Event::PointerButton {
                    pos,
                    button,
                    pressed: true,
                    ..
                } if matches!(
                    button,
                    egui::PointerButton::Primary | egui::PointerButton::Secondary
                ) && rect.contains(pos)
                    && ctx.layer_id_at(pos) == Some(ui.layer_id()) =>
                {
                    if button == egui::PointerButton::Primary
                        && self
                            .gesture
                            .as_ref()
                            .is_some_and(|g| g.button == egui::PointerButton::Secondary)
                    {
                        continue;
                    }
                    self.finish();
                    self.error = None;
                    let style = self.style();
                    let tool = if button == egui::PointerButton::Secondary {
                        Tool::Eraser
                    } else {
                        self.tool
                    };
                    let result = if tool == Tool::Pen {
                        self.control.annotation_begin(
                            self.owner,
                            screen,
                            vec![normalize(rect, pos)],
                            style,
                        )
                    } else if is_shape(tool) {
                        self.control.annotation_shape_begin(
                            self.owner,
                            screen,
                            vec![normalize(rect, pos)],
                            style,
                        )
                    } else {
                        Ok(())
                    };
                    if let Err(e) = result {
                        self.error = Some(e.to_string());
                        continue;
                    }
                    self.gesture = Some(Gesture {
                        tool,
                        screen,
                        rect,
                        start: pos,
                        end: pos,
                        raw_end: pos,
                        button,
                    });
                    if tool == Tool::Eraser {
                        self.erase(screen, rect, pos);
                    }
                }
                egui::Event::PointerMoved(pos) => {
                    if let Some(g) = &mut self.gesture {
                        g.raw_end = pos;
                        g.end = constrain(g, pos, ctx.input(|i| i.modifiers.shift));
                        if g.tool == Tool::Pen {
                            self.control
                                .annotation_append(self.owner, normalize(rect, g.end));
                        }
                        if g.tool == Tool::Eraser {
                            self.erase(screen, rect, pos);
                        }
                    }
                }
                egui::Event::PointerButton {
                    pos,
                    button,
                    pressed: false,
                    modifiers,
                } => {
                    if self.gesture.as_ref().is_some_and(|g| g.button == button) {
                        let mut g = self.gesture.take().unwrap();
                        g.raw_end = pos;
                        g.end = constrain(&g, pos, modifiers.shift);
                        match g.tool {
                            Tool::Pen => {
                                self.control
                                    .annotation_append(self.owner, normalize(rect, g.end));
                                self.control.annotation_finish(self.owner);
                            }
                            Tool::Eraser => {}
                            _ => {
                                self.error = self
                                    .control
                                    .annotation_shape_update(self.owner, shape_points(&g))
                                    .err()
                                    .map(|e| e.to_string());
                                self.control
                                    .annotation_shape_end(self.owner, self.error.is_none());
                            }
                        }
                    }
                }
                egui::Event::Key {
                    key: egui::Key::Escape,
                    pressed: true,
                    ..
                }
                | egui::Event::PointerGone => self.finish(),
                _ => {}
            }
        }
        if let Some(g) = &mut self.gesture {
            if is_shape(g.tool) {
                // Recompute even without mouse motion when Shift is pressed or released.
                g.end = constrain(g, g.raw_end, ctx.input(|i| i.modifiers.shift));
                if let Err(e) = self
                    .control
                    .annotation_shape_update(self.owner, shape_points(g))
                {
                    self.error = Some(e.to_string());
                }
            }
        }
    }
    fn erase(&mut self, screen: i32, rect: Rect, pos: Pos2) {
        if self.control.annotation_snapshot().busy {
            return;
        }
        let strokes = self.control.annotation_strokes(screen);
        if let Some(stroke) = strokes.iter().rev().find(|s| {
            hit(
                s,
                rect,
                pos,
                self.control.annotation_line_scale(screen, rect.width()),
            )
        }) {
            self.error = self
                .control
                .annotation_erase(stroke.id)
                .err()
                .map(|e| e.to_string());
        }
    }
}
impl Drop for AnnotationUi {
    fn drop(&mut self) {
        self.control.annotation_leave(self.owner);
    }
}
fn is_shape(tool: Tool) -> bool {
    matches!(
        tool,
        Tool::Line | Tool::Arrow | Tool::Rectangle | Tool::Ellipse
    )
}
fn shape_points(g: &Gesture) -> Vec<Point> {
    shape(g.tool, g.start, g.end)
        .into_iter()
        .map(|p| normalize(g.rect, p))
        .collect()
}
fn constrain(g: &Gesture, p: Pos2, shift: bool) -> Pos2 {
    let p = clamp(g.rect, p);
    if shift && matches!(g.tool, Tool::Line | Tool::Arrow) {
        let delta = p - g.start;
        let step = std::f32::consts::FRAC_PI_4;
        let angle = (delta.y.atan2(delta.x) / step).round() * step;
        let direction = vec2(angle.cos(), angle.sin());
        let mut length = delta.dot(direction).max(0.);
        for (d, start, min, max) in [
            (direction.x, g.start.x, g.rect.left(), g.rect.right()),
            (direction.y, g.start.y, g.rect.top(), g.rect.bottom()),
        ] {
            if d > 0.00001 {
                length = length.min((max - start) / d);
            } else if d < -0.00001 {
                length = length.min((min - start) / d);
            }
        }
        return clamp(g.rect, g.start + direction * length);
    }
    if shift && matches!(g.tool, Tool::Rectangle | Tool::Ellipse) {
        let delta = p - g.start;
        let side = delta.x.abs().max(delta.y.abs());
        let available_x = if delta.x < 0.0 {
            g.start.x - g.rect.left()
        } else {
            g.rect.right() - g.start.x
        };
        let available_y = if delta.y < 0.0 {
            g.start.y - g.rect.top()
        } else {
            g.rect.bottom() - g.start.y
        };
        let side = side.min(available_x).min(available_y);
        return g.start
            + vec2(
                if delta.x < 0.0 { -side } else { side },
                if delta.y < 0.0 { -side } else { side },
            );
    }
    p
}
fn clamp(rect: Rect, p: Pos2) -> Pos2 {
    pos2(
        p.x.clamp(rect.left(), rect.right()),
        p.y.clamp(rect.top(), rect.bottom()),
    )
}
fn pos2(x: f32, y: f32) -> Pos2 {
    Pos2::new(x, y)
}
fn normalize(rect: Rect, p: Pos2) -> Point {
    Point {
        x: ((p.x - rect.left()) / rect.width()).clamp(0., 1.),
        y: ((p.y - rect.top()) / rect.height()).clamp(0., 1.),
    }
}
fn pixel(rect: Rect, p: Point) -> Pos2 {
    pos2(
        rect.left() + p.x * rect.width(),
        rect.top() + p.y * rect.height(),
    )
}
fn hit(s: &InkStroke, rect: Rect, pos: Pos2, scale: f32) -> bool {
    let tolerance = 6.0 + s.style.width * scale * 0.5;
    if s.points.len() == 1 {
        return pixel(rect, s.points[0]).distance(pos) <= tolerance;
    }
    s.points.windows(2).any(|pair| {
        let a = pixel(rect, pair[0]);
        let b = pixel(rect, pair[1]);
        let v = b - a;
        let t = ((pos - a).dot(v) / v.length_sq().max(0.0001)).clamp(0., 1.);
        (a + v * t).distance(pos) <= tolerance
    })
}
fn shape(tool: Tool, a: Pos2, b: Pos2) -> Vec<Pos2> {
    let corners = match tool {
        Tool::Arrow => {
            let v = b - a;
            let len = v.length();
            if len < 1. {
                return vec![a];
            }
            let d = v / len;
            let n = vec2(-d.y, d.x);
            let head = (len * 0.25).clamp(6., 22.).min(len * 0.5);
            vec![
                a,
                b,
                b - d * head + n * head * 0.5,
                b,
                b - d * head - n * head * 0.5,
            ]
        }
        Tool::Rectangle => vec![a, pos2(b.x, a.y), b, pos2(a.x, b.y), a],
        Tool::Ellipse => {
            let center = a + (b - a) * 0.5;
            let radii = (b - a) * 0.5;
            return (0..=96)
                .map(|i| {
                    let t = i as f32 / 96. * std::f32::consts::TAU;
                    center + vec2(t.cos() * radii.x, t.sin() * radii.y)
                })
                .collect();
        }
        _ => vec![a, b],
    };
    // Repeated vertices preserve corners under the host's midpoint smoothing.
    corners.into_iter().flat_map(|p| [p, p, p]).collect()
}

// Keep the drag handle reachable after resizing, DPI changes and fullscreen changes.
fn panel_position(position: Pos2, size: egui::Vec2, bounds: Rect) -> Pos2 {
    pos2(
        position
            .x
            .clamp(bounds.left(), (bounds.right() - size.x).max(bounds.left())),
        position
            .y
            .clamp(bounds.top(), (bounds.bottom() - size.y).max(bounds.top())),
    )
}
