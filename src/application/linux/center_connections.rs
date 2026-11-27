//! Recent connections and the account's server-synced favorites.
#[path = "../app/recents.rs"]
mod recents;

use super::{DeviceCenter, Update, actions, modal, time};
use crate::account::{
    api::{device_id_text, key_id_text, list_order},
    assist::{SavedDevice, SavedKind, SavedLists, normalize_connect_id},
    client::AuthenticatedClient,
};
use crate::i18n::t;
use crate::transport::direct::store;
use crate::ui::{controls, device_list, theme};
use anyhow::Result;
use egui::RichText;
use std::{future::Future, sync::Arc};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum DeviceTab {
    Mine,
    Nearby,
    Recent,
}

impl DeviceTab {
    pub(super) fn title(self) -> &'static str {
        match self {
            Self::Mine => t().center.tab_mine,
            Self::Nearby => t().center.tab_nearby,
            Self::Recent => t().center.tab_recent,
        }
    }
}

#[derive(Default)]
pub(super) struct State {
    lists: Option<SavedLists>,
    load_error: Option<String>,
    search: String,
    dialog: Option<FavoriteDialog>,
}

enum FavoriteDialog {
    Edit(FavoriteEditor),
    Remove(SavedDevice),
}

#[derive(Default)]
struct FavoriteEditor {
    id: String,
    remark: String,
    code: String,
    code_changed: bool,
    editing: bool,
}

impl FavoriteEditor {
    fn for_device(device: &SavedDevice) -> Self {
        Self {
            id: device.connect_id.clone(),
            remark: device.remark.clone(),
            code: device.saved_code.clone(),
            editing: device.is_favorite,
            ..Self::default()
        }
    }
}

enum Pick {
    Recent(recents::Recent),
    Favorite(SavedDevice),
    Dialog(FavoriteDialog),
}

impl DeviceCenter {
    pub(super) fn refresh_saved(&mut self) {
        self.request_center(|client| async move {
            Ok(Update::Saved {
                lists: client.assist_lists().await,
                mutation: None,
            })
        });
    }

    fn change_saved<F, Fut>(&mut self, change: F)
    where
        F: FnOnce(Arc<AuthenticatedClient>) -> Fut + Send + 'static,
        Fut: Future<Output = Result<&'static str>> + Send + 'static,
    {
        self.request_center(move |client| async move {
            let mutation = change(Arc::clone(&client)).await;
            // An update can succeed on the server even if its local code write fails.
            Ok(Update::Saved {
                lists: client.assist_lists().await,
                mutation: Some(mutation),
            })
        });
    }

    pub(super) fn finish_saved(
        &mut self,
        lists: Result<SavedLists>,
        mutation: Option<Result<&'static str>>,
    ) {
        let connections = &mut self.center.connections;
        match lists {
            Ok(lists) => {
                connections.load_error = None;
                connections.lists = Some(lists);
            }
            Err(error) => connections.load_error = Some(format!("{error:#}")),
        }
        match mutation {
            Some(Ok(message)) => self.center.message = Some(message.into()),
            Some(Err(error)) => {
                self.center.message = None;
                self.show_error(error);
            }
            None => {}
        }
    }

    pub(super) fn recent_connections(&mut self, ui: &mut egui::Ui) {
        let text = &t().center;
        let signed_in = self.account.is_some();
        let can_edit = self
            .account
            .as_ref()
            .is_some_and(|(client, _)| !client.server_unreachable())
            && !self.busy;
        let connections = &mut self.center.connections;
        device_list::toolbar(ui, &mut connections.search, |ui| {
            if signed_in
                && ui
                    .add_enabled(can_edit, controls::secondary(text.add_favorite))
                    .clicked()
            {
                connections.dialog = Some(FavoriteDialog::Edit(FavoriteEditor::default()));
            }
        });
        let query = connections.search.trim().to_lowercase();
        let lists = connections.lists.as_ref().filter(|_| signed_in);
        let mut picked = None;
        if signed_in {
            let mut favorites = lists.map_or_else(Vec::new, |lists| {
                lists.favorites.iter().collect::<Vec<_>>()
            });
            let total = favorites.len();
            favorites.retain(|device| {
                device_list::matches(
                    &query,
                    &[device.title(), &device.publisher_device_id, &device.connect_id],
                )
            });
            favorites.sort_by(|a, b| {
                list_order(a.title(), &a.publisher_device_id, b.title(), &b.publisher_device_id)
                    .then_with(|| a.connect_id.cmp(&b.connect_id))
            });
            device_list::heading(ui, text.favorites, Some(total), true, |_| {});
            if let Some(error) = &connections.load_error {
                ui.colored_label(theme::AMBER, error);
            }
            if let Some(error) = lists.and_then(|lists| lists.code_error.as_deref()) {
                ui.colored_label(theme::AMBER, error);
            }
            if favorites.is_empty() && connections.load_error.is_none() {
                device_list::empty(ui, if lists.is_none() {
                    text.loading_favorites
                } else if total == 0 {
                    text.no_favorites
                } else {
                    text.no_matching_devices
                });
            }
            for device in favorites {
                let detail = (text.favorite_line)(
                    &device.connect_id,
                    &key_id_text(&device.public_key),
                );
                ui.push_id(("favorite", &device.connect_id), |ui| {
                    device_list::row(
                        ui,
                        &device_list::Row {
                            title: device.title(),
                            tile: device_list::Tile::Generic,
                            status: None,
                            chips: &[],
                            extra_badges: None,
                            detail: &detail,
                            selected: None,
                        },
                        |ui| {
                            if ui.add(controls::primary(t().common.connect)).clicked() {
                                picked = Some(Pick::Favorite(device.clone()));
                            }
                            if ui.add_enabled(can_edit, controls::secondary(text.edit)).clicked() {
                                picked = Some(Pick::Dialog(FavoriteDialog::Edit(
                                    FavoriteEditor::for_device(device),
                                )));
                            }
                            if ui.add_enabled(can_edit, controls::secondary(text.remove_favorite)).clicked() {
                                picked = Some(Pick::Dialog(FavoriteDialog::Remove(device.clone())));
                            }
                        },
                    );
                });
            }
        }
        let recent = recents::merge(store::known_hosts(), lists.map_or(&[], |lists| &lists.recent));
        let total = recent.len();
        device_list::heading(ui, text.recent_connections, Some(total), !signed_in, |_| {});
        let mut shown = false;
        for row in recent {
            let mut parts = Vec::new();
            if let Some(id) = &row.device_id {
                parts.push(format!("ID {}", device_id_text(id)));
            }
            if let Some(address) = &row.address {
                parts.push((text.address_line)(address));
            }
            if let Some(number) = &row.short_number {
                parts.push((text.short_number_line)(number));
            }
            if !device_list::matches(
                &query,
                &[
                    &row.title,
                    row.device_id.as_deref().unwrap_or_default(),
                    row.short_number.as_deref().unwrap_or_default(),
                    row.address.as_deref().unwrap_or_default(),
                ],
            ) {
                continue;
            }
            shown = true;
            parts.push((text.last_connected)(&time(row.last)));
            let detail = parts.join(" · ");
            let title = if row.title.is_empty() {
                row.short_number.as_deref().unwrap_or(text.unnamed_device)
            } else {
                &row.title
            };
            let mut chips = Vec::new();
            if row.trusted {
                chips.push((text.trusts_this_device, theme::ACCENT));
            }
            if row.saved.as_ref().is_some_and(|saved| saved.is_favorite) {
                chips.push((text.favorite, theme::AMBER));
            }
            let mut connect = false;
            ui.push_id(("recent", &row.device_id, &row.short_number), |ui| {
                device_list::row(
                    ui,
                    &device_list::Row {
                        title,
                        tile: device_list::Tile::Generic,
                        status: None,
                        chips: &chips,
                        extra_badges: None,
                        detail: &detail,
                        selected: None,
                    },
                    |ui| {
                        connect = ui.add(controls::primary(t().common.connect)).clicked();
                        if let Some(saved) = row.saved.as_ref().filter(|saved| !saved.is_favorite)
                            && ui
                                .add_enabled(can_edit, controls::secondary(text.add_to_favorites))
                                .clicked()
                        {
                            picked = Some(Pick::Dialog(FavoriteDialog::Edit(
                                FavoriteEditor::for_device(saved),
                            )));
                        }
                    },
                );
            });
            if connect {
                picked = Some(Pick::Recent(row));
            }
        }
        if !shown {
            device_list::empty(ui, if total == 0 { text.no_history } else { text.no_matching_devices });
        }
        if picked.is_some() {
            ui.ctx().request_repaint();
        }
        match picked {
            Some(Pick::Recent(row)) => self.open_recent(row),
            Some(Pick::Favorite(device)) => self.joins.open_saved(&device),
            Some(Pick::Dialog(dialog)) => self.center.connections.dialog = Some(dialog),
            None => {}
        }
    }

    fn open_recent(&mut self, row: recents::Recent) {
        match (row.device_id, row.address, row.short_number) {
            (id, Some(address), None) if !row.trusted => {
                self.joins.open_target(address, id, Some(row.title));
            }
            (Some(id), address, number) if address.is_some() || number.is_none() => {
                self.joins.open_target(device_id_text(&id), None, None);
            }
            (_, _, Some(number)) => {
                if let Some(saved) = row.saved {
                    self.joins.open_saved(&saved);
                } else {
                    self.joins.open_target(number, None, None);
                }
            }
            (_, Some(address), _) => self.joins.open_target(address, None, Some(row.title)),
            _ => {}
        }
    }

    pub(super) fn saved_dialog(&mut self, ctx: &egui::Context) {
        let Some(dialog) = self.center.connections.dialog.take() else {
            return;
        };
        let text = t();
        match dialog {
            FavoriteDialog::Edit(mut editor) => {
                let answer = modal(
                    ctx,
                    "linux-favorite-editor",
                    if editor.editing { text.center.edit_favorite } else { text.center.add_favorite },
                    controls::DialogIcon::Edit,
                    380.0,
                    |ui| {
                        ui.label(RichText::new(text.center.short_number).color(theme::MUTED));
                        ui.add_enabled_ui(!editor.editing, |ui| {
                            if favorite_field(ui, &mut editor.id, 24, false, "").changed() {
                                editor.code.clear();
                                editor.code_changed = false;
                            }
                        });
                        ui.add_space(8.0);
                        ui.label(RichText::new(text.center.note).color(theme::MUTED));
                        favorite_field(ui, &mut editor.remark, 128, false, text.common.optional);
                        ui.add_space(8.0);
                        ui.label(RichText::new(text.center.access_code).color(theme::MUTED));
                        editor.code_changed |= favorite_field(
                            ui,
                            &mut editor.code,
                            256,
                            true,
                            text.common.optional,
                        )
                        .changed();
                        actions(ui, text.common.save, !self.busy && normalize_connect_id(&editor.id).is_ok(), false)
                    },
                );
                match answer {
                    Some(true) => self.change_saved(move |client| async move {
                        client
                            .save_assist_favorite(
                                &editor.id,
                                &editor.remark,
                                editor.code_changed.then_some(editor.code),
                            )
                            .await?;
                        Ok(t().app.favorite_updated)
                    }),
                    Some(false) => {}
                    None => self.center.connections.dialog = Some(FavoriteDialog::Edit(editor)),
                }
            }
            FavoriteDialog::Remove(device) => {
                let answer = modal(
                    ctx,
                    "linux-remove-favorite",
                    text.center.remove_favorite_title,
                    controls::DialogIcon::Warning,
                    380.0,
                    |ui| {
                        ui.label(device.title());
                        ui.label(RichText::new(&device.connect_id).color(theme::MUTED));
                        actions(ui, text.common.ok, !self.busy, true)
                    },
                );
                match answer {
                    Some(true) => self.change_saved(move |client| async move {
                        client.delete_assist_saved(SavedKind::Favorites, device.key()).await?;
                        Ok(t().app.record_removed)
                    }),
                    Some(false) => {}
                    None => self.center.connections.dialog = Some(FavoriteDialog::Remove(device)),
                }
            }
        }
    }
}

fn favorite_field(
    ui: &mut egui::Ui,
    input: &mut String,
    limit: usize,
    secret: bool,
    hint: &str,
) -> egui::Response {
    ui.add_sized(
        [ui.available_width(), controls::HEIGHT],
        controls::singleline(input, controls::HEIGHT)
            .hint_text(hint)
            .password(secret)
            .char_limit(limit),
    )
}
