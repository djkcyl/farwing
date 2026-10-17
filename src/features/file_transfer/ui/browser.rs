use super::{Action, FileEntry, Listing, controls, theme};
use crate::ui::controls::files::{self, Icon};
use egui::{Key, Rect, Ui, UiBuilder, pos2, vec2};
use std::{collections::HashSet, sync::Arc};

pub(super) struct Pane {
    pub path: String,
    pub selection: HashSet<String>,
    revision: u64,
    history: Vec<String>,
    history_index: usize,
    history_target: Option<usize>,
    address: String,
    editing: bool,
    focus_address: bool,
    search: String,
    searching: bool,
    focus_search: bool,
    anchor: Option<String>,
    cursor: Option<String>,
    scroll_to: Option<usize>,
    places: Vec<(String, String)>,
    cache: Arc<Vec<FileEntry>>,
    visible: Vec<usize>,
    filter: String,
    sort: usize,
    descending: bool,
    dirty: bool,
}

impl Pane {
    pub fn new(path: String, remote: bool) -> Self {
        let places = if remote {
            Vec::new()
        } else {
            use crate::features::file_transfer::storage::KnownFolder;
            [
                ("桌面", KnownFolder::Desktop),
                ("下载", KnownFolder::Downloads),
                ("文档", KnownFolder::Documents),
            ]
            .into_iter()
            .filter_map(|(name, id)| {
                crate::features::file_transfer::storage::known_folder(id)
                    .map(|p| (name.into(), p.to_string_lossy().into_owned()))
            })
            .collect()
        };
        Self {
            address: path.clone(),
            path,
            selection: HashSet::new(),
            revision: u64::MAX,
            history: Vec::new(),
            history_index: 0,
            history_target: None,
            editing: false,
            focus_address: false,
            search: String::new(),
            searching: false,
            focus_search: false,
            anchor: None,
            cursor: None,
            scroll_to: None,
            places,
            cache: Arc::default(),
            visible: Vec::new(),
            filter: String::new(),
            sort: 0,
            descending: false,
            dirty: true,
        }
    }

    pub fn visit(&mut self, path: &str) {
        self.history_target = None;
        self.address = path.into();
        self.editing = false;
    }

    fn sync(&mut self, list: &Listing, remote: bool) {
        if list.path.is_empty() {
            return;
        }
        if self.revision != list.revision {
            self.revision = list.revision;
            if self.path != list.path {
                self.selection.clear();
                self.cursor = None;
                self.anchor = None;
                self.scroll_to = Some(0);
                self.search.clear();
                self.searching = false;
            }
            self.path = list.path.clone();
            if !self.editing {
                self.address = self.path.clone();
            }
        }
        if !list.busy && list.error.is_none() {
            if let Some(index) = self.history_target
                && self.history.get(index) == Some(&list.path)
            {
                self.history_index = index;
                self.history_target = None;
            }
            if self.history.get(self.history_index) != Some(&list.path) {
                self.history.truncate(self.history_index + 1);
                self.history.push(list.path.clone());
                self.history_index = self.history.len() - 1;
            }
            if remote && list.path == ":/" {
                self.places = [
                    ("桌面", ["桌面", "desktop"]),
                    ("下载", ["下载", "downloads"]),
                    ("文档", ["文档", "documents"]),
                ]
                .into_iter()
                .filter_map(|(label, names)| {
                    list.entries
                        .iter()
                        .find(|e| {
                            e.entry_type < 4 && names.iter().any(|n| e.name.eq_ignore_ascii_case(n))
                        })
                        .map(|e| (label.into(), e.full_path.clone()))
                })
                .collect();
            }
        } else if !list.busy {
            self.history_target = None;
        }
        let needle = self.search.to_lowercase();
        if !Arc::ptr_eq(&self.cache, &list.entries) || self.filter != needle || self.dirty {
            self.cache = list.entries.clone();
            self.filter = needle;
            self.dirty = false;
            self.visible = list
                .entries
                .iter()
                .enumerate()
                .filter(|(_, e)| {
                    self.filter.is_empty() || e.name.to_lowercase().contains(&self.filter)
                })
                .map(|(i, _)| i)
                .collect();
            self.visible.sort_by_cached_key(|&i| {
                let e = &list.entries[i];
                (e.entry_type >= 4, e.name.to_lowercase())
            });
            // Stable sorting retains folder-first and name order for equal dates/sizes.
            if self.sort != 0 {
                self.visible.sort_by(|&a, &b| {
                    let (a, b) = (&list.entries[a], &list.entries[b]);
                    (a.entry_type >= 4).cmp(&(b.entry_type >= 4)).then_with(|| {
                        let ord = if self.sort == 1 {
                            a.modified_time.cmp(&b.modified_time)
                        } else {
                            a.size.cmp(&b.size)
                        };
                        if self.descending { ord.reverse() } else { ord }
                    })
                });
            } else if self.descending {
                self.visible.sort_by_cached_key(|&i| {
                    let e = &list.entries[i];
                    (e.entry_type >= 4, std::cmp::Reverse(e.name.to_lowercase()))
                });
            }
            if !list.busy {
                self.selection
                    .retain(|p| list.entries.iter().any(|e| &e.full_path == p));
            }
        }
    }

    fn select(&mut self, index: usize, list: &Listing, ctrl: bool, shift: bool) {
        let path = list.entries[self.visible[index]].full_path.clone();
        if shift {
            let anchor = self
                .anchor
                .as_ref()
                .and_then(|p| {
                    self.visible
                        .iter()
                        .position(|&i| &list.entries[i].full_path == p)
                })
                .unwrap_or(index);
            if !ctrl {
                self.selection.clear();
            }
            for i in anchor.min(index)..=anchor.max(index) {
                self.selection
                    .insert(list.entries[self.visible[i]].full_path.clone());
            }
        } else {
            if !ctrl {
                self.selection.clear();
            }
            if ctrl && self.selection.contains(&path) {
                self.selection.remove(&path);
            } else {
                self.selection.insert(path.clone());
            }
            self.anchor = Some(path.clone());
        }
        self.cursor = Some(path);
    }
}

pub(super) fn show(
    ui: &mut Ui,
    rect: Rect,
    p: &mut Pane,
    list: &Listing,
    remote: bool,
    alias: &str,
    connected: bool,
    mutation_enabled: bool,
    active: &mut bool,
    keyboard: bool,
) -> Option<Action> {
    p.sync(list, remote);
    crate::ui::controls::observe_notice(
        ui.ctx(),
        ("file-directory", remote),
        "读取目录失败",
        crate::ui::controls::DialogIcon::Error,
        list.error.as_deref(),
    );
    let mut action = None;
    if ui.input(|i| {
        i.pointer.any_pressed() && i.pointer.interact_pos().is_some_and(|v| rect.contains(v))
    }) {
        *active = remote;
    }
    let enabled = mutation_enabled && !list.busy && list.error.is_none();
    let nav_enabled = (!remote || connected) && !list.busy;
    let id = ui.id().with(remote);
    let mut pane = ui.new_child(UiBuilder::new().id_salt(remote).max_rect(rect.shrink(1.)));
    pane.set_clip_rect(rect.intersect(ui.clip_rect()));
    let ui = &mut pane;
    ui.spacing_mut().item_spacing = vec2(6., 4.);
    let inner = rect.shrink2(vec2(8., 4.));
    let row = |y: f32, h: f32| {
        Rect::from_min_size(pos2(inner.left(), inner.top() + y), vec2(inner.width(), h))
    };
    use files::ButtonStyle;
    let title = row(0., 24.);
    ui.painter().text(
        title.left_center(),
        egui::Align2::LEFT_CENTER,
        if remote { "远端" } else { "本机" },
        egui::FontId::proportional(theme::SECTION),
        theme::TEXT,
    );
    files::text(
        ui,
        Rect::from_min_max(title.min + vec2(44., 0.), title.max),
        if remote { alias } else { "此电脑" },
        theme::MUTED,
        false,
    );

    let mut history_action = None;
    let nav = row(30., theme::FILES_NAV_HEIGHT);
    let step = theme::FILES_NAV_HEIGHT + theme::FILES_NAV_GAP;
    let slot = |index: usize| {
        Rect::from_min_size(
            nav.min + vec2(index as f32 * step, 0.),
            vec2(theme::FILES_NAV_HEIGHT, nav.height()),
        )
    };
    if files::icon_at(
        ui,
        slot(0),
        Icon::Back,
        "后退 · Alt+←",
        nav_enabled && p.history_index > 0,
    )
    .clicked()
    {
        history_action = Some(p.history_index - 1);
    }
    if files::icon_at(
        ui,
        slot(1),
        Icon::Forward,
        "前进 · Alt+→",
        nav_enabled && p.history_index + 1 < p.history.len(),
    )
    .clicked()
    {
        history_action = Some(p.history_index + 1);
    }
    if files::icon_at(
        ui,
        slot(2),
        Icon::Up,
        "上级目录 · Backspace",
        nav_enabled && p.path != ":/",
    )
    .clicked()
    {
        action = Some(Action::Browse(parent(&p.path)));
    }
    let refresh_rect =
        Rect::from_min_max(nav.right_top() - vec2(theme::FILES_NAV_HEIGHT, 0.), nav.max);
    let search_rect = refresh_rect.translate(vec2(-step, 0.));
    let address_rect = Rect::from_min_max(
        nav.min + vec2(3. * step, 0.),
        search_rect.left_bottom() - vec2(theme::FILES_NAV_GAP, 0.),
    );
    if files::icon_at(ui, search_rect, Icon::Search, "筛选文件 · Ctrl+F", true).clicked() {
        p.searching = !p.searching;
        p.focus_search = p.searching;
        if !p.searching {
            p.search.clear();
        }
    }
    if files::icon_at(ui, refresh_rect, Icon::Refresh, "刷新 · F5", nav_enabled).clicked() {
        action = Some(Action::Browse(p.path.clone()));
    }
    if p.editing {
        let response = ui.put(
            address_rect,
            controls::singleline(&mut p.address, theme::FILES_NAV_HEIGHT),
        );
        if p.focus_address {
            response.request_focus();
            p.focus_address = false;
        }
        if response.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) && nav_enabled {
            action = Some(Action::Browse(p.address.clone()));
        }
        if ui.input(|i| i.key_pressed(Key::Escape)) || response.lost_focus() {
            p.editing = false;
        }
    } else {
        files::panel(ui, address_rect, false);
        let edit = ui.interact(address_rect, id.with("address"), egui::Sense::click());
        if edit.double_clicked() {
            p.editing = true;
            p.focus_address = true;
            p.address = p.path.clone();
        }
        edit.on_hover_text(&p.path);
        let crumbs = breadcrumbs(&p.path, if remote { alias } else { "此电脑" });
        let content = address_rect.shrink(theme::FILES_PATH_INSET);
        let widths = crumbs
            .iter()
            .map(|(label, _)| {
                ui.painter()
                    .layout_no_wrap(
                        label.clone(),
                        egui::FontId::proportional(theme::COMPACT_TEXT),
                        theme::TEXT,
                    )
                    .size()
                    .x
                    + 12.
            })
            .collect::<Vec<_>>();
        let total = widths.iter().sum::<f32>() + (crumbs.len().saturating_sub(1) as f32 * 14.);
        let mut start = 0;
        if total > content.width() {
            start = crumbs.len().saturating_sub(1);
            let mut used = widths[start].min(content.width() - 30.) + 30.;
            while start > 0 && used + widths[start - 1] + 14. <= content.width() {
                start -= 1;
                used += widths[start] + 14.;
            }
        }
        let mut x = content.left();
        if start > 0 {
            let r = Rect::from_min_size(pos2(x, content.top()), vec2(26., content.height()));
            if files::button_at(ui, r, "…", true, ButtonStyle::Quiet).clicked() {
                p.editing = true;
                p.focus_address = true;
                p.address = p.path.clone();
            }
            x += 30.;
        }
        for (index, (label, path)) in crumbs.iter().enumerate().skip(start) {
            let width = widths[index].min((content.right() - x).max(0.));
            if width <= 0. {
                break;
            }
            let r = Rect::from_min_size(pos2(x, content.top()), vec2(width, content.height()));
            let response = files::button_at(ui, r, label, nav_enabled, ButtonStyle::Quiet);
            if response.double_clicked() {
                p.editing = true;
                p.focus_address = true;
                p.address = p.path.clone();
            } else if response.clicked() {
                action = Some(Action::Browse(path.clone()));
            }
            x += width;
            if index + 1 < crumbs.len() {
                files::text(
                    ui,
                    Rect::from_min_size(pos2(x, content.top()), vec2(14., content.height())),
                    "›",
                    theme::MUTED,
                    false,
                );
                x += 14.;
            }
        }
    }

    let places = row(66., 28.);
    if p.searching {
        let close = Rect::from_min_max(places.right_top() - vec2(28., 0.), places.max);
        let input = Rect::from_min_max(places.min, close.left_bottom() - vec2(4., 0.));
        let response = ui.put(
            input,
            controls::singleline(&mut p.search, theme::COMPACT_HEIGHT).hint_text("筛选当前文件夹"),
        );
        if p.focus_search {
            response.request_focus();
            p.focus_search = false;
        }
        if files::icon_at(ui, close, Icon::Close, "关闭筛选", true).clicked()
            || (response.has_focus() && ui.input(|i| i.key_pressed(Key::Escape)))
        {
            p.searching = false;
            p.search.clear();
        }
    } else {
        for (index, (name, path)) in p.places.iter().enumerate() {
            let r = Rect::from_min_size(
                places.min + vec2(index as f32 * 60., 0.),
                vec2(56., places.height()),
            );
            if files::button_at(ui, r, name, nav_enabled, ButtonStyle::Quiet).clicked() {
                action = Some(Action::Browse(path.clone()));
            }
        }
    }
    let one = list
        .entries
        .iter()
        .find(|e| p.selection.contains(&e.full_path));
    let single = enabled && p.selection.len() == 1 && one.is_some_and(|e| e.entry_type != 3);
    let actions = row(100., theme::CONTROL_HEIGHT);
    let create = Rect::from_min_size(actions.min, vec2(90., actions.height()));
    let rename = Rect::from_min_size(actions.min + vec2(96., 0.), vec2(62., actions.height()));
    let delete = Rect::from_min_size(actions.min + vec2(164., 0.), vec2(50., actions.height()));
    let transfer = Rect::from_min_max(actions.right_top() - vec2(78., 0.), actions.max);
    if files::button_at(
        ui,
        create,
        "新建文件夹",
        enabled && p.path != ":/",
        ButtonStyle::Secondary,
    )
    .clicked()
    {
        action = Some(Action::Create);
    }
    if files::button_at(ui, rename, "重命名", single, ButtonStyle::Quiet).clicked() {
        action = one.cloned().map(Action::Rename);
    }
    if files::button_at(ui, delete, "删除", single, ButtonStyle::Quiet).clicked() {
        action = one.cloned().map(Action::Delete);
    }
    if files::button_at(
        ui,
        transfer,
        if remote { "← 下载" } else { "上传 →" },
        enabled && connected && !p.selection.is_empty(),
        if remote {
            ButtonStyle::Secondary
        } else {
            ButtonStyle::Primary
        },
    )
    .clicked()
    {
        action = Some(Action::Transfer);
    }
    let table = Rect::from_min_max(
        row(theme::FILES_BROWSER_HEADER, 0.).min,
        pos2(inner.right(), inner.bottom() - 28.),
    );
    files::panel(ui, table, *active == remote);
    let header = Rect::from_min_size(table.min + vec2(1., 1.), vec2(table.width() - 2., 30.));
    for (index, col) in files::file_columns(header).iter().enumerate() {
        let label = ["名称", "修改时间", "大小"][index];
        let label = if p.sort == index {
            format!("{label} {}", if p.descending { "↓" } else { "↑" })
        } else {
            label.into()
        };
        if files::table_heading(ui, *col, &label, index == 2).clicked() {
            if p.sort == index {
                p.descending = !p.descending;
            } else {
                p.sort = index;
                p.descending = false;
            }
            p.dirty = true;
        }
    }
    ui.painter().line_segment(
        [header.left_bottom(), header.right_bottom()],
        egui::Stroke::new(1., theme::LINE),
    );
    let body = Rect::from_min_max(
        header.left_bottom() + vec2(1., 2.),
        table.max - vec2(2., 2.),
    );
    if keyboard && *active == remote && !ui.ctx().egui_wants_keyboard_input() {
        ui.input(|i| {
            if i.modifiers.ctrl && i.key_pressed(Key::L) {
                p.editing = true;
                p.focus_address = true;
                p.address = p.path.clone();
            }
            if i.modifiers.ctrl && i.key_pressed(Key::F) {
                p.searching = true;
                p.focus_search = true;
            }
            if i.modifiers.ctrl && i.key_pressed(Key::A) {
                p.selection = p
                    .visible
                    .iter()
                    .map(|&n| list.entries[n].full_path.clone())
                    .collect();
            }
            if nav_enabled {
                if i.key_pressed(Key::F5) {
                    action = Some(Action::Browse(p.path.clone()));
                }
                if i.key_pressed(Key::Backspace) {
                    action = Some(Action::Browse(parent(&p.path)));
                }
                if i.modifiers.alt && i.key_pressed(Key::ArrowLeft) && p.history_index > 0 {
                    history_action = Some(p.history_index - 1);
                }
                if i.modifiers.alt
                    && i.key_pressed(Key::ArrowRight)
                    && p.history_index + 1 < p.history.len()
                {
                    history_action = Some(p.history_index + 1);
                }
                if i.key_pressed(Key::Enter)
                    && let Some(e) = one
                    && e.entry_type < 4
                {
                    action = Some(Action::Browse(e.full_path.clone()));
                }
            }
            if single {
                if i.key_pressed(Key::F2) {
                    action = one.cloned().map(Action::Rename);
                }
                if i.key_pressed(Key::Delete) {
                    action = one.cloned().map(Action::Delete);
                }
            }
        });
        if !p.visible.is_empty() {
            let current = p.cursor.as_ref().and_then(|path| {
                p.visible
                    .iter()
                    .position(|&n| &list.entries[n].full_path == path)
            });
            let next = ui.input(|i| {
                if i.key_pressed(Key::Home) {
                    Some(0)
                } else if i.key_pressed(Key::End) {
                    Some(p.visible.len() - 1)
                } else if i.key_pressed(Key::ArrowDown) {
                    Some(current.map_or(0, |n| (n + 1).min(p.visible.len() - 1)))
                } else if i.key_pressed(Key::ArrowUp) {
                    Some(current.unwrap_or(0).saturating_sub(1))
                } else {
                    None
                }
            });
            if let Some(next) = next {
                p.select(next, list, false, ui.input(|i| i.modifiers.shift));
                p.scroll_to = Some(next);
            }
        }
    }
    ui.scope_builder(UiBuilder::new().max_rect(body), |ui| {
        ui.spacing_mut().item_spacing.y = 0.;
        let mut scroll = egui::ScrollArea::vertical()
            .id_salt("entries")
            .max_height(body.height())
            .auto_shrink([false, false]);
        if let Some(index) = p.scroll_to.take() {
            scroll = scroll.vertical_scroll_offset(index as f32 * theme::FILES_ROW_HEIGHT);
        }
        scroll.show_rows(ui, theme::FILES_ROW_HEIGHT, p.visible.len(), |ui, range| {
            for index in range {
                let e = &list.entries[p.visible[index]];
                let modified = if e.modified_time == 0 {
                    "—".into()
                } else {
                    chrono::DateTime::from_timestamp(e.modified_time as i64, 0)
                        .map(|v| {
                            v.with_timezone(&chrono::Local)
                                .format("%Y-%m-%d %H:%M")
                                .to_string()
                        })
                        .unwrap_or_else(|| "—".into())
                };
                let response = files::file_row(
                    ui,
                    &e.name,
                    &modified,
                    &if e.entry_type < 4 {
                        "—".into()
                    } else {
                        super::size(e.size)
                    },
                    e.entry_type,
                    &e.icon_type,
                    p.selection.contains(&e.full_path),
                );
                if response.double_clicked() && e.entry_type < 4 && nav_enabled {
                    action = Some(Action::Browse(e.full_path.clone()));
                } else if response.clicked() {
                    let m = ui.input(|i| i.modifiers);
                    p.select(index, list, m.ctrl, m.shift);
                    response.surrender_focus();
                }
            }
        });
    });
    if list.busy {
        files::empty(ui, body, "正在读取…");
    } else if list.error.is_some() {
        files::empty(ui, body, "无法读取目录");
    } else if p.visible.is_empty() {
        files::empty(
            ui,
            body,
            if p.search.is_empty() {
                "文件夹为空"
            } else {
                "没有匹配的文件"
            },
        );
    }
    let footer = Rect::from_min_max(pos2(inner.left(), inner.bottom() - 24.), inner.max);
    let status = if p.selection.is_empty() {
        format!("{} 个项目", p.visible.len())
    } else {
        format!(
            "{} 个项目 · 已选择 {} 个",
            p.visible.len(),
            p.selection.len()
        )
    };
    files::text(ui, footer, &status, theme::MUTED, false);
    if let Some(index) = history_action {
        p.history_target = Some(index);
        action = Some(Action::History(p.history[index].clone()));
    }
    action
}

fn parent(path: &str) -> String {
    // A Unix host's root is `\` in wire shape (a Linux host, see
    // `storage::wire_path`) or `/` (a macOS host, an older Linux one).
    if path == ":/" || path == "/" || path == "\\" {
        return ":/".into();
    }
    let path = path.trim_end_matches(['\\', '/']);
    match path.rfind(['\\', '/']) {
        Some(0) if path.starts_with(['/', '\\']) => path[..1].into(),
        Some(i) if i == 2 && path.as_bytes().get(1) == Some(&b':') => path[..=i].into(),
        Some(i) if i > 1 => path[..i].into(),
        _ => ":/".into(),
    }
}

fn breadcrumbs(path: &str, root: &str) -> Vec<(String, String)> {
    let mut result = vec![(root.into(), ":/".into())];
    if path == ":/" {
        return result;
    }
    let sep = if path.contains('\\') { '\\' } else { '/' };
    let mut current = if path.starts_with("\\\\") {
        "\\\\".to_string()
    } else if path.starts_with(['/', '\\']) {
        // A single leading separator is a Unix host's root, in either shape.
        path[..1].to_string()
    } else {
        String::new()
    };
    if path == "/" || path == "\\" {
        result.push((path.into(), path.into()));
    }
    for segment in path.split(['\\', '/']).filter(|s| !s.is_empty()) {
        if !current.is_empty() && !current.ends_with(['\\', '/']) {
            current.push(sep);
        }
        current.push_str(segment);
        if current.ends_with(':') {
            current.push(sep);
        }
        result.push((segment.into(), current.clone()));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn crumbs(path: &str) -> Vec<(String, String)> {
        breadcrumbs(path, "R")
    }
    fn pairs(v: &[(&str, &str)]) -> Vec<(String, String)> {
        v.iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect()
    }

    #[test]
    fn unix_host_in_wire_shape() {
        assert_eq!(parent("\\home\\u"), "\\home");
        assert_eq!(parent("\\home\\u\\"), "\\home");
        assert_eq!(parent("\\home"), "\\");
        assert_eq!(parent("\\"), ":/");
        assert_eq!(
            crumbs("\\home\\u"),
            pairs(&[("R", ":/"), ("home", "\\home"), ("u", "\\home\\u")])
        );
        assert_eq!(crumbs("\\"), pairs(&[("R", ":/"), ("\\", "\\")]));
    }

    #[test]
    fn posix_paths_unchanged() {
        assert_eq!(parent("/home/u"), "/home");
        assert_eq!(parent("/home"), "/");
        assert_eq!(parent("/"), ":/");
        assert_eq!(
            crumbs("/home/u"),
            pairs(&[("R", ":/"), ("home", "/home"), ("u", "/home/u")])
        );
        assert_eq!(crumbs("/"), pairs(&[("R", ":/"), ("/", "/")]));
    }

    #[test]
    fn windows_paths_unchanged() {
        assert_eq!(parent("C:\\Users\\u"), "C:\\Users");
        assert_eq!(parent("C:\\Users"), "C:\\");
        assert_eq!(parent("C:\\"), ":/");
        assert_eq!(parent("\\\\srv\\share\\d"), "\\\\srv\\share");
        assert_eq!(
            crumbs("C:\\Users\\u"),
            pairs(&[
                ("R", ":/"),
                ("C:", "C:\\"),
                ("Users", "C:\\Users"),
                ("u", "C:\\Users\\u")
            ])
        );
        assert_eq!(
            crumbs("\\\\srv\\share"),
            pairs(&[("R", ":/"), ("srv", "\\\\srv"), ("share", "\\\\srv\\share")])
        );
        assert_eq!(crumbs(":/"), pairs(&[("R", ":/")]));
    }
}
