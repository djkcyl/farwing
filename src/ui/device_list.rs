//! The pieces every device list on the 设备 page shares, so the four tabs
//! look and behave the same: a toolbar (search on the left, the tab's own
//! controls on the right), group headings, one row per device, and one
//! empty state. Rows and groups are spaced by the item spacing alone.
use super::center_icon::{Icon, paint_icon};
use super::theme::{self, BG, LINE, MUTED, SURFACE};
use super::{controls, device_visuals};
use crate::account::api::{DeviceInfo, DeviceList};
use crate::application::wallpaper::Wallpapers;
use crate::i18n::t;
use egui::{Align, Color32, RichText, Sense, Stroke, vec2};

/// The picture at the start of a row: the same size in every list.
pub(crate) enum Tile<'a> {
    /// The device's desktop, with its system in the corner.
    Wallpaper {
        texture: Option<&'a egui::TextureHandle>,
        platform: farwing_protocol::Platform,
    },
    /// Its system's mark.
    Platform(farwing_protocol::Platform),
    /// A device whose system is not known.
    Generic,
}

/// A device's wallpaper and system, when this account can see it (this
/// device, its other devices, friends' shared ones).
pub(crate) type Picture = (Option<egui::TextureHandle>, farwing_protocol::Platform);

pub(crate) fn known_picture(
    devices: Option<&DeviceList>,
    wallpapers: &mut Wallpapers,
    ctx: &egui::Context,
    id: &str,
) -> Option<Picture> {
    let list = devices?;
    let device = std::iter::once(&list.current_device)
        .chain(&list.devices)
        .chain(&list.shared)
        .find(|d| d.device_id == id)?;
    Some((
        wallpapers.texture(ctx, &device.device_id, &device.wallpaper_url),
        device.platform,
    ))
}

/// Computers show their desktop; browsers and phones their system.
fn has_desktop(platform: farwing_protocol::Platform) -> bool {
    use farwing_protocol::Platform;
    matches!(platform, Platform::Windows | Platform::Macos | Platform::Linux)
}

impl<'a> Tile<'a> {
    /// The tile of a device with `texture` as its wallpaper.
    pub(crate) fn of(texture: Option<&'a egui::TextureHandle>, platform: farwing_protocol::Platform) -> Self {
        if has_desktop(platform) {
            Tile::Wallpaper { texture, platform }
        } else {
            Tile::Platform(platform)
        }
    }

    /// The tile of a [`known_picture`], or `fallback` for a device this
    /// account cannot see.
    pub(crate) fn known(picture: Option<&'a Picture>, fallback: Self) -> Self {
        match picture {
            Some((texture, platform)) => Self::of(texture.as_ref(), *platform),
            None => fallback,
        }
    }
}

/// What a row shows; its actions are laid out by the caller.
pub(crate) struct Row<'a> {
    pub title: &'a str,
    pub tile: Tile<'a>,
    /// Online, offline, being watched…
    pub status: Option<(&'a str, Color32)>,
    pub chips: &'a [(&'a str, Color32)],
    /// Additional badges, such as permissions.
    pub extra_badges: Option<&'a dyn Fn(&mut egui::Ui)>,
    /// One line under the name.
    pub detail: &'a str,
    /// None for informational rows; otherwise whether the row is selected.
    pub selected: Option<bool>,
}

const HEIGHT: f32 = 76.0;
const TILE: egui::Vec2 = egui::Vec2::new(96.0, 54.0);
const HEADING: f32 = 28.0;

/// One device. `actions` adds buttons from right to left (the main one
/// first). Returns whether the row itself (not a button) was clicked.
pub(crate) fn row(ui: &mut egui::Ui, row: &Row, actions: impl FnOnce(&mut egui::Ui)) -> bool {
    paint_row(ui, row, SURFACE, actions)
}

/// [`row`] inside a card.
pub(crate) fn inset_row(ui: &mut egui::Ui, row: &Row, actions: impl FnOnce(&mut egui::Ui)) -> bool {
    paint_row(ui, row, BG, actions)
}

fn paint_row(
    ui: &mut egui::Ui,
    row: &Row,
    background: Color32,
    actions: impl FnOnce(&mut egui::Ui),
) -> bool {
    let sense = if row.selected.is_some() { Sense::click() } else { Sense::hover() };
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), HEIGHT), sense);
    if ui.is_rect_visible(rect) {
        let selected = row.selected == Some(true);
        let fill = if selected {
            theme::SELECTED
        } else if row.selected.is_some() && response.hovered() {
            theme::HOVER
        } else {
            background
        };
        ui.painter().rect_filled(rect, 8.0, fill);
        ui.painter().rect_stroke(
            rect,
            8.0,
            Stroke::new(1.0, if selected { theme::BORDER_FOCUS } else { LINE }),
            egui::StrokeKind::Inside,
        );
        let tile = egui::Rect::from_min_size(
            egui::pos2(rect.left() + 12.0, rect.center().y - TILE.y / 2.0),
            TILE,
        );
        paint_tile(ui, tile, &row.tile);
        // Actions first: the text gets what they leave.
        let mut buttons = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(egui::Rect::from_min_max(
                    egui::pos2(tile.right() + 200.0, rect.top()),
                    egui::pos2(rect.right() - 14.0, rect.bottom()),
                ))
                .layout(egui::Layout::right_to_left(Align::Center)),
        );
        buttons.spacing_mut().item_spacing.x = 8.0;
        actions(&mut buttons);
        let text_right = buttons.min_rect().left() - 16.0;
        let mut text = ui.new_child(egui::UiBuilder::new().max_rect(egui::Rect::from_min_max(
            egui::pos2(tile.right() + 14.0, rect.top() + 14.0),
            egui::pos2(text_right.max(tile.right() + 120.0), rect.bottom() - 10.0),
        )));
        text.spacing_mut().item_spacing.y = 6.0;
        text.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            ui.add(egui::Label::new(RichText::new(row.title).strong()).truncate())
                .on_hover_text(row.title);
            if let Some((status, color)) = row.status {
                let (dot, _) = ui.allocate_exact_size(vec2(6.0, 6.0), Sense::hover());
                ui.painter().circle_filled(dot.center(), 3.0, color);
                ui.label(RichText::new(status).size(theme::SMALL).color(color));
            }
            for (text, color) in row.chips {
                controls::chip(ui, text, *color);
            }
            if let Some(badges) = row.extra_badges {
                badges(ui);
            }
        });
        text.add(
            egui::Label::new(RichText::new(row.detail).size(theme::SMALL).color(MUTED)).truncate(),
        );
    }
    response.clicked()
}

/// A group's title with its count; actions on the right. Groups after the
/// first are set apart from the rows above.
pub(crate) fn heading(
    ui: &mut egui::Ui,
    title: &str,
    count: Option<usize>,
    first: bool,
    actions: impl FnOnce(&mut egui::Ui),
) {
    if !first {
        ui.add_space(10.0);
    }
    ui.allocate_ui_with_layout(
        vec2(ui.available_width(), HEADING),
        egui::Layout::left_to_right(Align::Center),
        |ui| {
            ui.set_min_height(HEADING);
            ui.label(RichText::new(title).strong());
            if let Some(count) = count {
                ui.label(RichText::new(count.to_string()).size(theme::SMALL).color(MUTED));
            }
            ui.with_layout(egui::Layout::right_to_left(Align::Center), actions);
        },
    );
}

fn paint_tile(ui: &mut egui::Ui, rect: egui::Rect, tile: &Tile) {
    match tile {
        Tile::Wallpaper { texture, platform } => {
            device_visuals::wallpaper(ui, rect, *texture, *platform);
            let badge =
                egui::Rect::from_min_size(rect.right_bottom() - vec2(25.0, 25.0), vec2(22.0, 22.0));
            ui.painter().rect_filled(badge, 5.0, BG);
            device_visuals::system_icon(ui.painter(), badge.shrink(3.0), *platform);
        }
        Tile::Platform(platform) => {
            ui.painter().rect_filled(rect, 6.0, BG);
            let icon = egui::Rect::from_center_size(rect.center(), vec2(28.0, 28.0));
            device_visuals::system_icon(ui.painter(), icon, *platform);
        }
        Tile::Generic => {
            ui.painter().rect_filled(rect, 6.0, BG);
            let icon = egui::Rect::from_center_size(rect.center(), vec2(28.0, 28.0));
            paint_icon(ui.painter(), icon, Icon::Monitor, MUTED);
        }
    }
}

/// Search on the left, the tab's own controls on the right.
pub(crate) fn toolbar(
    ui: &mut egui::Ui,
    search: &mut String,
    controls: impl FnOnce(&mut egui::Ui),
) {
    ui.allocate_ui_with_layout(
        vec2(ui.available_width(), crate::ui::controls::HEIGHT),
        egui::Layout::right_to_left(Align::Center),
        |ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            controls(ui);
            ui.add_sized(
                [ui.available_width(), crate::ui::controls::HEIGHT],
                controls::singleline(search, controls::HEIGHT).hint_text(t().center.search_devices),
            );
        },
    );
    ui.add_space(4.0);
}

/// Whether `query` (already lower case) is in any of `fields`.
pub(crate) fn matches(query: &str, fields: &[&str]) -> bool {
    query.is_empty()
        || fields
            .iter()
            .any(|field| field.to_lowercase().contains(query))
}

/// Which devices a list shows by their state.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum StateFilter {
    #[default]
    All,
    Online,
    Offline,
}

impl StateFilter {
    fn label(self) -> &'static str {
        match self {
            Self::All => t().center.all,
            Self::Online => t().common.online,
            Self::Offline => t().common.offline,
        }
    }

    pub(crate) fn keeps(self, device: &DeviceInfo) -> bool {
        match self {
            Self::All => true,
            Self::Online => device.is_connected(),
            Self::Offline => device.is_offline(),
        }
    }

    /// A drop-down with how many devices each choice shows.
    pub(crate) fn combo(ui: &mut egui::Ui, id: &str, value: &mut Self, devices: &[&DeviceInfo]) {
        let count = |filter: Self| devices.iter().filter(|d| filter.keeps(d)).count();
        egui::ComboBox::from_id_salt(id)
            .width(110.0)
            .selected_text(format!("{}  {}", value.label(), count(*value)))
            .show_ui(ui, |ui| {
                for filter in [Self::All, Self::Online, Self::Offline] {
                    ui.selectable_value(
                        value,
                        filter,
                        format!("{}  {}", filter.label(), count(filter)),
                    );
                }
            });
    }
}

/// Name order, or online devices first.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum Order {
    OnlineFirst,
    #[default]
    Name,
}

impl Order {
    fn label(self) -> &'static str {
        match self {
            Self::OnlineFirst => t().center.online_first,
            Self::Name => t().center.by_name,
        }
    }

    pub(crate) fn compare(self, a: &DeviceInfo, b: &DeviceInfo) -> std::cmp::Ordering {
        (if self == Self::OnlineFirst {
            b.is_connected().cmp(&a.is_connected())
        } else {
            std::cmp::Ordering::Equal
        })
        .then_with(|| a.list_order(b))
    }

    pub(crate) fn combo(ui: &mut egui::Ui, id: &str, value: &mut Self) {
        egui::ComboBox::from_id_salt(id)
            .width(110.0)
            .selected_text(value.label())
            .show_ui(ui, |ui| {
                for order in [Self::Name, Self::OnlineFirst] {
                    ui.selectable_value(value, order, order.label());
                }
            });
    }
}

/// An empty list: the same card in every tab.
pub(crate) fn empty(ui: &mut egui::Ui, title: &str) {
    controls::card().show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.add_space(10.0);
        ui.vertical_centered(|ui| {
            ui.label(RichText::new(title).color(MUTED));
        });
        ui.add_space(10.0);
    });
}
