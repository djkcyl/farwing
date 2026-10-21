//! Visible remote-access indicator, driven by the actual host session.
use super::DeviceCenterApp;
use crate::features::host::Status;
use crate::ui::{chrome::TitleBarAlert, controls, theme};

impl DeviceCenterApp {
    pub(super) fn controlled_caption(&self) -> Option<TitleBarAlert> {
        caption(
            &self.host.as_ref()?.status(),
            self.devices.as_ref(),
            self.catalog.as_ref(),
        )
    }
}

pub(super) fn caption(
    status: &Status,
    devices: Option<&crate::account::api::DeviceList>,
    catalog: Option<&super::catalog::Catalog>,
) -> Option<TitleBarAlert> {
    if status.session_active && status.connected && status.connections.len() > 1 {
        let seconds = status
            .connections
            .iter()
            .filter_map(|c| c.elapsed_seconds)
            .max()?;
        let names: Vec<_> = status
            .connections
            .iter()
            .map(|c| name(&c.device_id, &c.nickname, devices, catalog))
            .collect();
        return Some(TitleBarAlert {
            source: format!(
                "{} 人正在访问 · {}{}",
                names.len(),
                names[..2].join("、"),
                if names.len() > 2 { " 等" } else { "" }
            ),
            duration: format!("已连接 {}", duration(seconds)),
        });
    }
    let Some(connection) = &status.connection else {
        return status.connected.then(|| TitleBarAlert {
            source: if status.assistance {
                "访客正在访问"
            } else {
                "正在被远程访问"
            }
            .into(),
            duration: "已连接".into(),
        });
    };
    let seconds = connection.elapsed_seconds?;
    if !status.session_active && !connection.observation_lost {
        return None;
    }
    let name = name(&connection.device_id, &connection.nickname, devices, catalog);
    let kind = if connection.observation_lost {
        "被控状态确认中"
    } else if !status.connected {
        "连接暂时中断"
    } else if status.assistance || connection.relation == "assist" {
        "访客"
    } else if connection.relation == "friend" {
        "好友设备"
    } else if connection.relation == "trusted" {
        "受信任设备"
    } else {
        "自有设备"
    };
    Some(TitleBarAlert {
        source: format!("{kind} · {name}"),
        duration: if connection.observation_lost {
            format!("上次已连接 {}", duration(seconds))
        } else {
            format!("已连接 {}", duration(seconds))
        },
    })
}

/// The status of one participant on its own, for per-participant captions.
pub(super) fn participant_status(status: &Status, index: usize) -> Option<Status> {
    let connection = status.connections.get(index)?;
    Some(Status {
        connected: status.connected,
        session_active: status.session_active,
        assistance: connection.assistance,
        connection: Some(connection.clone()),
        ..Default::default()
    })
}

/// A remote participant's name.
fn name(
    device_id: &str,
    nickname: &str,
    devices: Option<&crate::account::api::DeviceList>,
    catalog: Option<&super::catalog::Catalog>,
) -> String {
    // Prefer the server participant matching this session's controller,
    // never the first participant or another device with the same alias.
    let participant = devices.and_then(|l| {
        l.current_device
            .participants_info
            .iter()
            .find(|p| p.device_id == device_id)
    });
    [
        participant.map(|p| p.nickname.as_str()),
        Some(nickname),
        catalog.and_then(|c| {
            c.groups
                .entries()
                .find(|(_, d)| d.device_id == device_id)
                .map(|(_, d)| d.alias.as_str())
        }),
    ]
    .into_iter()
    .flatten()
    .map(clean_name)
    .find(|s| !s.is_empty())
    .unwrap_or_else(|| {
        if device_id.is_empty() {
            "远程设备".into()
        } else {
            format!(
                "设备 {}",
                device_id
                    .chars()
                    .rev()
                    .take(6)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect::<String>()
            )
        }
    })
}

/// How the participant relates to this host's account.
pub(super) fn relation(relation: &str, assistance: bool) -> Option<&'static str> {
    match relation {
        "own" => Some("本账号"),
        "friend" => Some("好友"),
        "trusted" => Some("受信任"),
        "assist" => Some("访客"),
        _ if assistance => Some("访客"),
        _ => None,
    }
}

/// One connected participant in the host GUI.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Participant {
    pub session_id: String,
    pub name: String,
    pub device_name: String,
    /// The participant's device ID, for display.
    pub device_id: String,
    pub relation: Option<&'static str>,
    pub duration: String,
    /// The session can be ended now.
    pub disconnectable: bool,
    /// The host owner withdrew keyboard and mouse control.
    pub control_blocked: bool,
    /// A visitor this host could trust from now on: its device ID and key.
    pub trust: Option<(String, String)>,
}

/// What the host owner chose for one participant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ParticipantAction {
    Disconnect(String),
    /// Withdraw (`true`) or restore (`false`) keyboard and mouse control.
    BlockControl(String, bool),
    /// Trust a visitor's device from now on: its device ID, key and name.
    Trust {
        device_id: String,
        public_key: String,
        name: String,
    },
}

impl ParticipantAction {
    /// Apply what this host does by itself; trusting goes through the
    /// device center (the background service may hold the records).
    pub(super) fn apply(self, host: &crate::features::host::Handle) {
        match self {
            Self::Disconnect(session) => {
                host.disconnect_session(&session);
            }
            Self::BlockControl(session, blocked) => {
                host.block_control(&session, blocked);
            }
            Self::Trust { .. } => {}
        }
    }
}

pub(super) fn participants(
    status: &Status,
    devices: Option<&crate::account::api::DeviceList>,
    catalog: Option<&super::catalog::Catalog>,
) -> Vec<Participant> {
    status
        .connections
        .iter()
        .map(|c| Participant {
            session_id: c.session_id.clone(),
            name: name(&c.device_id, &c.nickname, devices, catalog),
            device_name: clean_name(&c.device_name),
            device_id: crate::account::api::key_id_text(&c.public_key),
            relation: relation(&c.relation, c.assistance),
            duration: if c.observation_lost {
                "状态确认中".into()
            } else {
                c.elapsed_seconds
                    .map(|s| format!("已连接 {}", duration(s)))
                    .unwrap_or_else(|| "正在连接".into())
            },
            disconnectable: status.session_active
                && !c.observation_lost
                && !c.session_id.is_empty(),
            control_blocked: c.control_blocked,
            trust: (c.relation == "assist" || (c.assistance && c.relation.is_empty()))
                .then(|| crate::transport::direct::store::device_id_of(&c.public_key))
                .flatten()
                .map(|id| (id, c.public_key.clone())),
        })
        .collect()
}

/// Every participant with its own "断开" and "禁止控制" buttons.
pub(super) fn participants_list(
    ui: &mut egui::Ui,
    participants: &[Participant],
) -> Option<ParticipantAction> {
    let mut ended = None;
    for participant in participants {
        ui.push_id(("participant", &participant.session_id), |ui| {
            let (rect, _) = ui.allocate_exact_size(
                egui::vec2(ui.available_width(), theme::MENU_HEIGHT),
                egui::Sense::hover(),
            );
            let mut row = ui.new_child(
                egui::UiBuilder::new()
                    .max_rect(rect)
                    .layout(egui::Layout::right_to_left(egui::Align::Center)),
            );
            if row
                .add_enabled(participant.disconnectable, controls::secondary("断开"))
                .clicked()
            {
                ended = Some(ParticipantAction::Disconnect(participant.session_id.clone()));
            }
            let label = if participant.control_blocked {
                "允许控制"
            } else {
                "禁止控制"
            };
            if row
                .add_enabled(participant.disconnectable, controls::secondary(label))
                .clicked()
            {
                ended = Some(ParticipantAction::BlockControl(
                    participant.session_id.clone(),
                    !participant.control_blocked,
                ));
            }
            if let Some((device_id, public_key)) = &participant.trust
                && row.add(controls::secondary("信任")).clicked()
            {
                ended = Some(ParticipantAction::Trust {
                    device_id: device_id.clone(),
                    public_key: public_key.clone(),
                    name: if participant.device_name.is_empty() {
                        participant.name.clone()
                    } else {
                        participant.device_name.clone()
                    },
                });
            }
            row.label(
                egui::RichText::new(&participant.duration)
                    .size(theme::SMALL)
                    .color(theme::MUTED),
            );
            row.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                if let Some(relation) = participant.relation {
                    let color = match relation {
                        "好友" => theme::GREEN,
                        "访客" => theme::AMBER,
                        "受信任" => theme::ACCENT,
                        _ => theme::ACCENT,
                    };
                    egui::Frame::new()
                        .fill(color.gamma_multiply(0.15))
                        .corner_radius(theme::CONTROL_RADIUS)
                        .inner_margin(egui::Margin::symmetric(6, 1))
                        .show(ui, |ui| {
                            ui.label(
                                egui::RichText::new(relation)
                                    .size(theme::TINY)
                                    .color(color),
                            );
                        });
                }
                ui.add(
                    egui::Label::new(egui::RichText::new(&participant.name).color(theme::TEXT))
                        .truncate(),
                );
                let device = if participant.device_name.is_empty() {
                    format!("ID {}", participant.device_id)
                } else {
                    format!("{} · ID {}", participant.device_name, participant.device_id)
                };
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(device)
                            .size(theme::SMALL)
                            .color(theme::MUTED),
                    )
                    .truncate(),
                );
            });
        });
    }
    ended
}

fn clean_name(name: &str) -> String {
    name.chars()
        .filter(|c| {
            !c.is_control() && !matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        })
        .take(64)
        .collect::<String>()
        .trim()
        .to_owned()
}

pub(super) fn duration(seconds: u64) -> String {
    if seconds >= 3600 {
        format!(
            "{}:{:02}:{:02}",
            seconds / 3600,
            seconds / 60 % 60,
            seconds % 60
        )
    } else {
        format!("{:02}:{:02}", seconds / 60, seconds % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(connections: &[(&str, &str, &str, Option<u64>)]) -> Status {
        let mut status = Status {
            connected: true,
            session_active: true,
            ..Default::default()
        };
        for (session, nickname, relation, seconds) in connections {
            status.connections.push(Default::default());
            let connection = status.connections.last_mut().unwrap();
            connection.session_id = (*session).into();
            connection.device_id = format!("device-{session}-123456");
            connection.nickname = (*nickname).into();
            connection.relation = (*relation).into();
            connection.device_name = "办公室电脑".into();
            connection.elapsed_seconds = *seconds;
        }
        status.connection = status.connections.first().cloned();
        status
    }

    #[test]
    fn each_participant_has_its_own_row() {
        let mut status = status(&[
            ("s1", "Alice", "own", Some(65)),
            ("s2", "", "friend", Some(3700)),
            ("", "Carol", "assist", None),
        ]);
        let rows = participants(&status, None, None);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].name, "Alice");
        assert_eq!(rows[0].relation, Some("本账号"));
        assert_eq!(rows[0].duration, "已连接 01:05");
        assert!(rows[0].disconnectable);
        // Without a nickname, the device id.
        assert_eq!(rows[1].name, "设备 123456");
        assert_eq!(rows[1].relation, Some("好友"));
        assert_eq!(rows[1].duration, "已连接 1:01:40");
        // No server session yet: nothing to end.
        assert!(!rows[2].disconnectable);
        assert_eq!(rows[2].relation, Some("访客"));
        status.session_active = false;
        assert!(participants(&status, None, None).iter().all(|r| !r.disconnectable));
        assert_eq!(relation("", true), Some("访客"));
        assert_eq!(relation("trusted", false), Some("受信任"));
        assert_eq!(relation("", false), None);
    }

    #[test]
    fn captions_cover_one_or_several_participants() {
        let single = status(&[("s1", "Alice", "friend", Some(5))]);
        let alert = caption(&single, None, None).unwrap();
        assert_eq!(alert.source, "好友设备 · Alice");
        assert_eq!(alert.duration, "已连接 00:05");
        let several = status(&[
            ("s1", "Alice", "own", Some(5)),
            ("s2", "Bob", "friend", Some(90)),
            ("s3", "Carol", "assist", Some(1)),
        ]);
        let alert = caption(&several, None, None).unwrap();
        assert_eq!(alert.source, "3 人正在访问 · Alice、Bob 等");
        assert_eq!(alert.duration, "已连接 01:30");
        let one = participant_status(&several, 1).unwrap();
        assert_eq!(caption(&one, None, None).unwrap().source, "好友设备 · Bob");
        assert!(participant_status(&several, 3).is_none());
    }
}
