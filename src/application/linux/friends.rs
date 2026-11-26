use super::*;
use farwing_protocol::{
    api::{Friend, FriendRequest, FriendRequestOutcome, Grants, UserSummary},
    caps,
};

#[derive(Default)]
pub(super) struct State {
    data: Option<Friends>,
    username: String,
    search: String,
}

pub(in crate::application) struct Friends {
    friends: Vec<Friend>,
    incoming: Vec<FriendRequest>,
    outgoing: Vec<FriendRequest>,
}

pub(in crate::application) enum Update {
    Data(Friends, DeviceList),
    Grants(String, Grants),
    Saved,
    Changed(String),
}

pub(super) enum Dialog {
    Accept(FriendRequest, Vec<String>),
    Remove(UserSummary),
    Grants(UserSummary, Option<(Grants, Grants)>),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Default,
    Custom,
    Hidden,
}

impl Mode {
    fn label(self) -> &'static str {
        match self {
            Self::Default => t().center.mode_default,
            Self::Custom => t().center.mode_custom,
            Self::Hidden => t().center.mode_hidden,
        }
    }
}

fn capabilities(ui: &mut egui::Ui, list: &mut Vec<String>) {
    ui.horizontal_wrapped(|ui| {
        for &cap in caps::ALL {
            let mut on = list.iter().any(|c| c == cap);
            if ui.checkbox(&mut on, crate::i18n::cap_label(cap)).changed() {
                if on {
                    list.push(cap.into());
                } else {
                    list.retain(|c| c != cap);
                }
                *list = caps::normalize(list);
            }
        }
    });
    if !list.is_empty() && !list.iter().any(|c| c == caps::VIEW) {
        ui.colored_label(
            theme::AMBER,
            (t().center.view_required)(&crate::i18n::cap_label(caps::VIEW)),
        );
    }
}

impl DeviceCenter {
    pub(super) fn refresh_friends(&mut self) {
        self.request_center(|client| async move {
            let (friends, requests, mut devices) = tokio::try_join!(
                client.friends(),
                client.friend_requests(),
                client.list_devices()
            )?;
            devices.sort();
            Ok(super::Update::Friends(Update::Data(
                Friends {
                    friends: friends.friends,
                    incoming: requests.incoming,
                    outgoing: requests.outgoing,
                },
                devices,
            )))
        });
    }

    pub(super) fn finish_friends(&mut self, update: Update) {
        match update {
            Update::Data(data, devices) => {
                self.center.friends.data = Some(data);
                if let Some((_, list)) = &mut self.account {
                    *list = devices;
                }
            }
            Update::Grants(id, grants) => {
                if let Some(super::Dialog::Friend(Dialog::Grants(user, loaded))) =
                    &mut self.center.dialog
                    && user.user_id == id
                {
                    *loaded = Some((grants.clone(), grants));
                }
            }
            Update::Saved => {
                self.center.dialog = None;
                self.center.message = Some(t().app.permissions_saved.into());
            }
            Update::Changed(message) => {
                self.center.message = Some(message);
                self.center.load = true;
            }
        }
    }

    fn load_grants(&mut self, id: String) {
        self.request_center(move |client| async move {
            let grants = client.grants(&id).await?;
            Ok(super::Update::Friends(Update::Grants(id, grants)))
        });
    }

    pub(super) fn friends_page(
        &mut self,
        ui: &mut egui::Ui,
        client: &Arc<AuthenticatedClient>,
        devices: &DeviceList,
    ) {
        let text = &t().center;
        let mut add = false;
        ui.horizontal(|ui| {
            ui.add_enabled_ui(!self.busy, |ui| {
                let input = ui.add(
                    controls::singleline(&mut self.center.friends.username, controls::HEIGHT)
                        .hint_text(text.username)
                        .char_limit(128),
                );
                let ready = !self.center.friends.username.trim().is_empty();
                add = ui
                    .add_enabled(ready, controls::primary(text.send_request))
                    .clicked()
                    || (ready
                        && input.lost_focus()
                        && ui.input(|i| i.key_pressed(egui::Key::Enter)));
            });
        });
        if add {
            let username = self.center.friends.username.trim().to_owned();
            self.request_center(move |client| async move {
                let message = match client.send_friend_request(&username).await? {
                    FriendRequestOutcome::Sent { .. } => t().app.friend_request_sent.into(),
                    FriendRequestOutcome::Accepted { friend } => {
                        (t().app.now_friends)(friend.user.display_name())
                    }
                };
                Ok(super::Update::Friends(Update::Changed(message)))
            });
        }
        let Some(data) = self.center.friends.data.as_ref() else {
            return;
        };
        let (friends, incoming, outgoing) = (
            data.friends.clone(),
            data.incoming.clone(),
            data.outgoing.clone(),
        );
        if !incoming.is_empty() || !outgoing.is_empty() {
            device_list::heading(
                ui,
                text.pending_requests,
                Some(incoming.len() + outgoing.len()),
                true,
                |_| {},
            );
            for request in incoming {
                self.friend_request(ui, request, true);
            }
            for request in outgoing {
                self.friend_request(ui, request, false);
            }
        }
        device_list::heading(ui, text.my_friends, Some(friends.len()), true, |_| {});
        if friends.is_empty() {
            device_list::empty(ui, text.no_friends);
            return;
        }
        ui.add(
            controls::singleline(&mut self.center.friends.search, controls::HEIGHT)
                .hint_text(text.search_friends),
        );
        let query = self.center.friends.search.trim().to_lowercase();
        let mut shown = false;
        for friend in friends {
            let user = &friend.user;
            if !device_list::matches(&query, &[user.display_name(), &user.username]) {
                continue;
            }
            shown = true;
            ui.push_id(&user.user_id, |ui| {
                controls::card().show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal_wrapped(|ui| {
                        ui.label(RichText::new(user.display_name()).strong());
                        ui.label(
                            RichText::new((text.friends_since)(&time(friend.since)))
                                .small()
                                .color(theme::MUTED),
                        );
                        if ui
                            .add_enabled(!self.busy, controls::secondary(text.set_permissions))
                            .clicked()
                        {
                            self.load_grants(user.user_id.clone());
                            self.center.dialog =
                                Some(super::Dialog::Friend(Dialog::Grants(user.clone(), None)));
                        }
                        if ui
                            .add_enabled(!self.busy, controls::secondary(text.remove_friend))
                            .clicked()
                        {
                            self.center.dialog =
                                Some(super::Dialog::Friend(Dialog::Remove(user.clone())));
                        }
                    });
                    let mut shared = devices
                        .shared
                        .iter()
                        .filter(|device| {
                            device
                                .owner
                                .as_ref()
                                .is_some_and(|owner| owner.user_id == user.user_id)
                        })
                        .peekable();
                    if shared.peek().is_none() {
                        ui.label(
                            RichText::new(text.no_devices_shared)
                                .small()
                                .color(theme::MUTED),
                        );
                    }
                    for device in shared {
                        self.device_row(ui, client, device, false);
                    }
                });
            });
        }
        if !shown {
            device_list::empty(ui, text.no_matching_friends);
        }
    }

    fn friend_request(&mut self, ui: &mut egui::Ui, request: FriendRequest, incoming: bool) {
        let text = &t().center;
        controls::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(request.user.display_name()).strong());
                let date = time(request.created_at);
                ui.label(if incoming {
                    (text.request_received)(&date)
                } else {
                    (text.request_sent)(&date)
                });
                if incoming
                    && ui
                        .add_enabled(!self.busy, controls::primary(text.accept))
                        .clicked()
                {
                    self.center.dialog = Some(super::Dialog::Friend(Dialog::Accept(
                        request.clone(),
                        caps::normalize(caps::DEFAULT_FRIEND),
                    )));
                }
                if ui
                    .add_enabled(
                        !self.busy,
                        controls::secondary(if incoming {
                            text.decline
                        } else {
                            text.withdraw
                        }),
                    )
                    .clicked()
                {
                    let id = request.id.clone();
                    self.request_center(move |client| async move {
                        let message = if incoming {
                            client.answer_friend_request(&id, None).await?;
                            t().app.friend_request_declined
                        } else {
                            client.cancel_friend_request(&id).await?;
                            t().app.friend_request_withdrawn
                        };
                        Ok(super::Update::Friends(Update::Changed(message.into())))
                    });
                }
            });
        });
    }

    pub(super) fn friend_dialog(
        &mut self,
        ctx: &egui::Context,
        devices: &DeviceList,
        mut dialog: Dialog,
    ) {
        let text = &t().center;
        let answer = match &mut dialog {
            Dialog::Accept(request, caps) => modal(
                ctx,
                "linux-friend-accept",
                &(text.accept_friend)(request.user.display_name()),
                DialogIcon::Info,
                520.0,
                |ui| {
                    ui.label(RichText::new(text.they_may).strong());
                    capabilities(ui, caps);
                    actions(ui, text.accept, !self.busy, false)
                },
            ),
            Dialog::Remove(user) => modal(
                ctx,
                "linux-remove-friend",
                text.remove_friend_title,
                DialogIcon::Warning,
                430.0,
                |ui| {
                    ui.label((text.remove_friend_body)(user.display_name()));
                    actions(ui, text.remove_friend, !self.busy, true)
                },
            ),
            Dialog::Grants(user, grants) => modal(
                ctx,
                "linux-friend-grants",
                &(text.permissions_of)(user.display_name()),
                DialogIcon::Edit,
                620.0,
                |ui| {
                    if let Some(error) = &self.error {
                        ui.colored_label(theme::RED, error);
                    }
                    let Some((original, draft)) = grants else {
                        if !self.busy && ui.add(controls::secondary(t().common.retry)).clicked() {
                            self.load_grants(user.user_id.clone());
                        }
                        if self.busy {
                            ui.spinner();
                            ui.label(text.loading_permissions);
                        }
                        return actions(ui, t().common.save, false, false);
                    };
                    ui.add_enabled_ui(!self.busy, |ui| {
                        ui.label(RichText::new(text.all_devices).strong());
                        if ui
                            .add(controls::secondary(text.share_nothing_by_default))
                            .clicked()
                        {
                            draft.default.clear();
                        }
                        capabilities(ui, &mut draft.default);
                        ui.separator();
                        ui.label(RichText::new(text.per_device).strong());
                        egui::ScrollArea::vertical()
                            .max_height(260.0)
                            .show(ui, |ui| {
                                let groups = DeviceGroups::from_list(devices);
                                if groups.desktop_devices.is_empty() {
                                    ui.label(text.no_other_devices);
                                }
                                for device in &groups.desktop_devices {
                                    ui.push_id(&device.device_id, |ui| {
                                        device_grant(ui, device, draft)
                                    });
                                }
                            });
                    });
                    actions(
                        ui,
                        if self.busy {
                            text.saving
                        } else {
                            t().common.save
                        },
                        !self.busy && original != draft,
                        false,
                    )
                },
            ),
        };
        match (answer, dialog) {
            (None, dialog) => self.center.dialog = Some(super::Dialog::Friend(dialog)),
            (Some(true), Dialog::Accept(request, caps)) => {
                self.request_center(move |client| async move {
                    client
                        .answer_friend_request(&request.id, Some(caps))
                        .await?;
                    Ok(super::Update::Friends(Update::Changed(
                        t().app.friend_request_accepted.into(),
                    )))
                })
            }
            (Some(true), Dialog::Remove(user)) => self.request_center(move |client| async move {
                client.remove_friend(&user.user_id).await?;
                Ok(super::Update::Friends(Update::Changed(
                    t().app.friend_removed.into(),
                )))
            }),
            (Some(true), Dialog::Grants(user, Some((original, draft)))) => {
                let (id, grants) = (user.user_id.clone(), draft.clone());
                self.request_center(move |client| async move {
                    client.set_grants(&id, grants).await?;
                    Ok(super::Update::Friends(Update::Saved))
                });
                self.center.dialog = Some(super::Dialog::Friend(Dialog::Grants(
                    user,
                    Some((original, draft)),
                )));
            }
            _ => {}
        }
    }
}

fn device_grant(ui: &mut egui::Ui, device: &DeviceInfo, grants: &mut Grants) {
    let id = &device.device_id;
    let mode = match grants.devices.get(id) {
        None => Mode::Default,
        Some(caps) if caps.is_empty() => Mode::Hidden,
        Some(_) => Mode::Custom,
    };
    let mut chosen = mode;
    ui.horizontal_wrapped(|ui| {
        ui.label(format!("{} · {}", alias(device), device_id_text(id)));
        egui::ComboBox::from_id_salt("grant-mode")
            .selected_text(mode.label())
            .show_ui(ui, |ui| {
                for mode in [Mode::Default, Mode::Custom, Mode::Hidden] {
                    ui.selectable_value(&mut chosen, mode, mode.label());
                }
            });
    });
    if chosen != mode {
        match chosen {
            Mode::Default => {
                grants.devices.remove(id);
            }
            Mode::Custom => {
                let caps = if grants.default.is_empty() {
                    caps::normalize(caps::DEFAULT_FRIEND)
                } else {
                    grants.default.clone()
                };
                grants.devices.insert(id.clone(), caps);
            }
            Mode::Hidden => {
                grants.devices.insert(id.clone(), Vec::new());
            }
        }
    }
    if let Some(caps) = grants.devices.get_mut(id).filter(|caps| !caps.is_empty()) {
        capabilities(ui, caps);
    }
}
