//! Validated remote input messages (peer `INPUT` frames, Input v2).
//! Never retain or log the original payload.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

use crate::platform::windows::input::keyboard;

pub const MAX_MESSAGE: usize = 4 * 1024;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) enum Event {
    Command(Command),
    Heartbeat,
    /// A physical key by HID usage `(page << 16) | id`.
    Key {
        usage: u32,
        down: bool,
        repeat: bool,
    },
    /// Caps/Num/Scroll Lock state wanted by the controller; `seq` ties it to
    /// the participant's lock-key presses (checked on receipt).
    LockState {
        caps: bool,
        num: bool,
        scroll: bool,
        seq: u32,
    },
    Button {
        button: u8,
        down: bool,
    },
    Absolute {
        screen: Option<i32>,
        x: f64,
        y: f64,
    },
    Relative {
        x: i32,
        y: i32,
    },
    /// Both axes in 1/120 of a notch.
    Wheel {
        x: i32,
        y: i32,
    },
    /// Every active contact of the owner on `screen` (TOUCH); none cancels
    /// all.
    Touch {
        screen: i32,
        contacts: Vec<TouchContact>,
    },
    /// 1–16 pen samples, oldest first (PEN).
    Pen {
        screen: i32,
        samples: Vec<PenSample>,
    },
    /// Cancel every contact and lift the pen (a malformed TOUCH/PEN frame
    /// must not leave a finger down).
    PointerCancel,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum TouchPhase {
    Down,
    Move,
    Up,
    Cancel,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct TouchContact {
    pub id: u8,
    pub phase: TouchPhase,
    pub x: f64,
    pub y: f64,
    /// 0 = not reported.
    pub pressure: f32,
    /// Normalized to the screen; 0 = not reported.
    pub width: f32,
    pub height: f32,
    pub orientation: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum PenPhase {
    Hover,
    Down,
    Move,
    Up,
    Leave,
    Cancel,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct PenSample {
    pub phase: PenPhase,
    pub x: f64,
    pub y: f64,
    pub pressure: f32,
    pub tilt_x: i8,
    pub tilt_y: i8,
    pub twist: u16,
    pub barrel: bool,
    pub eraser: bool,
}

/// At most this many contacts per TOUCH frame (Windows synthetic touch).
pub(crate) const MAX_CONTACTS: usize = 10;
/// At most this many samples per PEN frame.
pub(crate) const MAX_PEN_SAMPLES: usize = 16;

fn unit(v: f64) -> bool {
    v.is_finite() && (0.0..=1.0).contains(&v)
}

fn unit32(v: f32) -> bool {
    v.is_finite() && (0.0..=1.0).contains(&v)
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Command {
    /// Ctrl+Alt+Del through the installed host service.
    SecureAttention,
    Desktop,
    TaskView,
    Lock,
    TaskManager,
}

impl Event {
    pub(crate) fn validate(&self) -> Result<()> {
        match self {
            Self::Key { usage, .. } => {
                ensure!(keyboard::scan(*usage).is_some(), "无效键码");
            }
            Self::Button { button: b, .. } => {
                button(i32::from(*b))?;
            }
            Self::Relative { x, y } => ensure!(
                x.unsigned_abs() <= 1_000_000 && y.unsigned_abs() <= 1_000_000,
                "相对位移过大"
            ),
            Self::Absolute { x, y, .. } => ensure!(
                x.is_finite()
                    && y.is_finite()
                    && (0.0..=1.0).contains(x)
                    && (0.0..=1.0).contains(y),
                "绝对坐标越界"
            ),
            Self::Wheel { x, y } => ensure!(
                x.unsigned_abs() <= WHEEL_LIMIT && y.unsigned_abs() <= WHEEL_LIMIT,
                "滚轮幅度过大"
            ),
            Self::Touch { contacts, .. } => {
                ensure!(contacts.len() <= MAX_CONTACTS, "触点过多");
                let mut ids = std::collections::BTreeSet::new();
                for c in contacts {
                    ensure!(c.id != 0 && ids.insert(c.id), "触点编号无效");
                    ensure!(
                        unit(c.x)
                            && unit(c.y)
                            && unit32(c.pressure)
                            && unit32(c.width)
                            && unit32(c.height)
                            && c.orientation < 360,
                        "触点数据越界"
                    );
                }
            }
            Self::Pen { samples, .. } => {
                ensure!(
                    !samples.is_empty() && samples.len() <= MAX_PEN_SAMPLES,
                    "笔样本数量无效"
                );
                for p in samples {
                    ensure!(
                        unit(p.x)
                            && unit(p.y)
                            && unit32(p.pressure)
                            && (-90..=90).contains(&p.tilt_x)
                            && (-90..=90).contains(&p.tilt_y)
                            && p.twist < 360,
                        "笔数据越界"
                    );
                }
            }
            Self::Heartbeat | Self::LockState { .. } | Self::Command(_) | Self::PointerCancel => {}
        }
        Ok(())
    }
}

/// Validate a decoded TOUCH or PEN frame (`docs/protocol/touch-pen-brief.md`
/// §5.5). An error means the caller cancels every contact.
pub(crate) fn pointer(message: crate::protocol::pointer::Message) -> Result<Event> {
    use crate::protocol::pointer::{Message, PenPhase as WirePen, TouchPhase as WireTouch};
    let event = match message {
        Message::Touch(t) => Event::Touch {
            screen: t.screen_id.ok_or_else(|| anyhow::anyhow!("触摸缺少屏幕"))?,
            contacts: t
                .contacts
                .into_iter()
                .map(|c| {
                    Ok(TouchContact {
                        id: u8::try_from(c.id).map_err(|_| anyhow::anyhow!("触点编号无效"))?,
                        phase: match WireTouch::try_from(c.phase) {
                            Ok(WireTouch::Down) => TouchPhase::Down,
                            Ok(WireTouch::Move) => TouchPhase::Move,
                            Ok(WireTouch::Up) => TouchPhase::Up,
                            Ok(WireTouch::Cancel) => TouchPhase::Cancel,
                            _ => anyhow::bail!("触点阶段无效"),
                        },
                        x: f64::from(c.x),
                        y: f64::from(c.y),
                        pressure: c.pressure,
                        width: c.width,
                        height: c.height,
                        orientation: u16::try_from(c.orientation).unwrap_or(u16::MAX),
                    })
                })
                .collect::<Result<_>>()?,
        },
        Message::Pen(p) => Event::Pen {
            screen: p.screen_id.ok_or_else(|| anyhow::anyhow!("笔缺少屏幕"))?,
            samples: p
                .samples
                .into_iter()
                .map(|s| {
                    ensure!(s.buttons & !3 == 0, "笔按键无效");
                    Ok(PenSample {
                        phase: match WirePen::try_from(s.phase) {
                            Ok(WirePen::Hover) => PenPhase::Hover,
                            Ok(WirePen::Down) => PenPhase::Down,
                            Ok(WirePen::Move) => PenPhase::Move,
                            Ok(WirePen::Up) => PenPhase::Up,
                            Ok(WirePen::Leave) => PenPhase::Leave,
                            Ok(WirePen::Cancel) => PenPhase::Cancel,
                            _ => anyhow::bail!("笔阶段无效"),
                        },
                        x: f64::from(s.x),
                        y: f64::from(s.y),
                        pressure: s.pressure,
                        tilt_x: i8::try_from(s.tilt_x).unwrap_or(i8::MAX),
                        tilt_y: i8::try_from(s.tilt_y).unwrap_or(i8::MAX),
                        twist: u16::try_from(s.twist).unwrap_or(u16::MAX),
                        barrel: s.buttons & 1 != 0,
                        eraser: s.buttons & 2 != 0,
                    })
                })
                .collect::<Result<_>>()?,
        },
    };
    event.validate()?;
    Ok(event)
}

/// One message never scrolls more than 1000 notches.
const WHEEL_LIMIT: u32 = 120_000;

fn button(v: i32) -> Result<u8> {
    ensure!(matches!(v, 1 | 2 | 16 | 32 | 64), "无效鼠标按键");
    Ok(v as u8)
}

/// Validate a decoded `INPUT` message. `None` means an empty or unknown event
/// (including a key this build cannot inject); callers still consume the frame.
pub(crate) fn input(message: crate::protocol::input::InputMessage) -> Result<Option<Event>> {
    use crate::protocol::input::{InputKind, SystemCommand};
    let code = |value: u32| i32::try_from(value).map_err(|_| anyhow::anyhow!("输入编码无效"));
    Ok(Some(match message.kind {
        None => return Ok(None),
        Some(InputKind::Heartbeat(_)) => Event::Heartbeat,
        Some(InputKind::Key(k)) => {
            ensure!(
                matches!(k.usage >> 16, 0x07 | 0x0c) && k.usage & 0xffff != 0,
                "无效键码"
            );
            if keyboard::scan(k.usage).is_none() {
                return Ok(None);
            }
            Event::Key {
                usage: k.usage,
                down: k.down,
                repeat: k.repeat && k.down,
            }
        }
        Some(InputKind::LockState(l)) => Event::LockState {
            caps: l.caps,
            num: l.num,
            scroll: l.scroll,
            seq: l.seq,
        },
        Some(InputKind::SystemCommand(value)) => Event::Command(
            match SystemCommand::try_from(value).map_err(|_| anyhow::anyhow!("未知系统命令"))?
            {
                SystemCommand::Sas => Command::SecureAttention,
                SystemCommand::Lock => Command::Lock,
                SystemCommand::TaskManager => Command::TaskManager,
                SystemCommand::ShowDesktop => Command::Desktop,
                SystemCommand::TaskView => Command::TaskView,
                SystemCommand::Unspecified => anyhow::bail!("未知系统命令"),
            },
        ),
        Some(InputKind::MouseButton(b)) => Event::Button {
            button: button(code(b.button)?)?,
            down: b.down,
        },
        Some(InputKind::MouseAbsolute(m)) => {
            ensure!(
                m.x.is_finite()
                    && m.y.is_finite()
                    && (0.0..=1.0).contains(&m.x)
                    && (0.0..=1.0).contains(&m.y),
                "绝对坐标越界"
            );
            Event::Absolute {
                screen: m.screen_id,
                x: m.x,
                y: m.y,
            }
        }
        Some(InputKind::MouseRelative(m)) => Event::Relative { x: m.dx, y: m.dy },
        Some(InputKind::MouseWheel(w)) => {
            let event = Event::Wheel { x: w.dx, y: w.dy };
            event.validate()?;
            event
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::input::{self as protocol, InputKind};

    fn decode(kind: InputKind) -> Result<Option<Event>> {
        let bytes = protocol::encode(kind);
        input(protocol::decode(&bytes)?.expect("input frame"))
    }

    #[test]
    fn keys_decode_by_usage() {
        let event = decode(InputKind::Key(protocol::Key {
            usage: 0x0007_0004,
            down: true,
            repeat: false,
        }))
        .unwrap();
        assert_eq!(
            event,
            Some(Event::Key {
                usage: 0x0007_0004,
                down: true,
                repeat: false
            })
        );
        // A release is never a repeat.
        let event = decode(InputKind::Key(protocol::Key {
            usage: 0x000c_00e9,
            down: false,
            repeat: true,
        }))
        .unwrap();
        assert_eq!(
            event,
            Some(Event::Key {
                usage: 0x000c_00e9,
                down: false,
                repeat: false
            })
        );
    }

    #[test]
    fn invalid_and_unknown_keys() {
        // Old virtual-key codes are not usages.
        assert!(
            decode(InputKind::Key(protocol::Key {
                usage: 0x41,
                down: true,
                repeat: false,
            }))
            .is_err()
        );
        // A valid page with a key this build cannot inject is ignored.
        assert_eq!(
            decode(InputKind::Key(protocol::Key {
                usage: 0x0007_00a5,
                down: true,
                repeat: false,
            }))
            .unwrap(),
            None
        );
    }

    #[test]
    fn lock_state_and_commands() {
        assert_eq!(
            decode(InputKind::LockState(protocol::LockState {
                caps: true,
                num: true,
                scroll: false,
                seq: 7,
            }))
            .unwrap(),
            Some(Event::LockState {
                caps: true,
                num: true,
                scroll: false,
                seq: 7
            })
        );
        for (command, expected) in [
            (protocol::SystemCommand::Sas, Command::SecureAttention),
            (protocol::SystemCommand::Lock, Command::Lock),
            (protocol::SystemCommand::TaskManager, Command::TaskManager),
            (protocol::SystemCommand::ShowDesktop, Command::Desktop),
            (protocol::SystemCommand::TaskView, Command::TaskView),
        ] {
            assert_eq!(
                decode(InputKind::SystemCommand(command as i32)).unwrap(),
                Some(Event::Command(expected))
            );
        }
        assert!(decode(InputKind::SystemCommand(0)).is_err());
        assert!(decode(InputKind::SystemCommand(99)).is_err());
    }

    #[test]
    fn pointer_events_are_bounded() {
        assert_eq!(
            decode(InputKind::MouseWheel(protocol::MouseWheel {
                dx: 120,
                dy: -240
            }))
            .unwrap(),
            Some(Event::Wheel { x: 120, y: -240 })
        );
        assert!(
            decode(InputKind::MouseWheel(protocol::MouseWheel {
                dx: i32::MIN,
                dy: 0
            }))
            .is_err()
        );
        assert!(
            decode(InputKind::MouseAbsolute(protocol::MouseAbsolute {
                screen_id: None,
                x: 1.5,
                y: 0.0,
            }))
            .is_err()
        );
        assert!(
            decode(InputKind::MouseButton(protocol::MouseButton {
                button: 4,
                down: true,
            }))
            .is_err()
        );
        assert_eq!(
            decode(InputKind::Heartbeat(protocol::Heartbeat {})).unwrap(),
            Some(Event::Heartbeat)
        );
    }

    #[test]
    fn touch_and_pen_are_validated() {
        use crate::protocol::pointer as p;
        let touch = |contacts: Vec<p::Contact>, screen: Option<i32>| {
            pointer(p::Message::Touch(p::Touch {
                screen_id: screen,
                contacts,
                time_us: 0,
            }))
        };
        let c = |id: u32, phase: p::TouchPhase, x: f32| p::Contact {
            id,
            phase: phase as i32,
            x,
            y: 0.5,
            ..Default::default()
        };
        assert!(matches!(
            touch(vec![c(1, p::TouchPhase::Down, 0.5)], Some(0)).unwrap(),
            Event::Touch { screen: 0, ref contacts } if contacts.len() == 1 && contacts[0].phase == TouchPhase::Down
        ));
        // No screen, duplicate ids, id 0, bad phase, out of range.
        assert!(touch(vec![c(1, p::TouchPhase::Down, 0.5)], None).is_err());
        assert!(touch(vec![c(1, p::TouchPhase::Down, 0.5), c(1, p::TouchPhase::Move, 0.5)], Some(0)).is_err());
        assert!(touch(vec![c(0, p::TouchPhase::Down, 0.5)], Some(0)).is_err());
        assert!(touch(vec![c(1, p::TouchPhase::Unspecified, 0.5)], Some(0)).is_err());
        assert!(touch(vec![c(1, p::TouchPhase::Down, 1.5)], Some(0)).is_err());
        assert!(touch((1..=11).map(|i| c(i, p::TouchPhase::Down, 0.5)).collect(), Some(0)).is_err());
        // An empty frame cancels everything.
        assert!(matches!(touch(vec![], Some(0)).unwrap(), Event::Touch { ref contacts, .. } if contacts.is_empty()));

        let pen = |samples: Vec<p::PenSample>| {
            pointer(p::Message::Pen(p::Pen {
                screen_id: Some(0),
                samples,
                time_us: 0,
            }))
        };
        let s = |phase: p::PenPhase| p::PenSample {
            phase: phase as i32,
            x: 0.5,
            y: 0.5,
            ..Default::default()
        };
        let ok = pen(vec![p::PenSample {
            buttons: 3,
            tilt_x: -90,
            ..s(p::PenPhase::Down)
        }])
        .unwrap();
        assert!(matches!(ok, Event::Pen { ref samples, .. } if samples[0].barrel && samples[0].eraser));
        assert!(pen(vec![]).is_err());
        assert!(pen(vec![p::PenSample { buttons: 4, ..s(p::PenPhase::Down) }]).is_err());
        assert!(pen(vec![p::PenSample { tilt_y: 91, ..s(p::PenPhase::Move) }]).is_err());
        assert!(pen((0..17).map(|_| s(p::PenPhase::Move)).collect()).is_err());
    }

    #[test]
    fn service_events_round_trip() {
        // The service pipe carries validated events as JSON.
        for event in [
            Event::Key {
                usage: 0x0007_00e3,
                down: true,
                repeat: false,
            },
            Event::LockState {
                caps: false,
                num: true,
                scroll: false,
                seq: 3,
            },
            Event::Command(Command::SecureAttention),
            Event::Wheel { x: -120, y: 0 },
            Event::Touch {
                screen: 0,
                contacts: vec![TouchContact {
                    id: 1,
                    phase: TouchPhase::Down,
                    x: 0.5,
                    y: 0.25,
                    pressure: 0.0,
                    width: 0.0,
                    height: 0.0,
                    orientation: 0,
                }],
            },
            Event::Pen {
                screen: 1,
                samples: vec![PenSample {
                    phase: PenPhase::Hover,
                    x: 0.5,
                    y: 0.5,
                    pressure: 0.0,
                    tilt_x: 30,
                    tilt_y: -15,
                    twist: 0,
                    barrel: true,
                    eraser: false,
                }],
            },
            Event::PointerCancel,
        ] {
            let json = serde_json::to_vec(&event).unwrap();
            let decoded: Event = serde_json::from_slice(&json).unwrap();
            decoded.validate().unwrap();
            assert_eq!(decoded, event);
        }
    }
}
