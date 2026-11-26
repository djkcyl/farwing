//! Validated UU input messages. Never retain or log the original JSON payload.
use anyhow::{Result, bail, ensure};
use prost::Message;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const MAX_MESSAGE: usize = 256 * 1024;
/// Contacts one touch frame may carry, as the mobile client sends them.
pub(crate) const MAX_TOUCH_POINTS: usize = 10;
pub const MAX_TEXT: usize = 64 * 1024;
#[derive(Clone, Copy, Serialize, Deserialize)]
pub(crate) enum Policy {
    Windows,
    Mobile,
    Other,
}
impl Policy {
    pub fn from_client_type(value: i32) -> Self {
        match value {
            3 => Self::Windows,
            1 | 2 => Self::Mobile,
            _ => Self::Other,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) enum Event {
    Command(Command),
    Heartbeat,
    Key {
        key: u16,
        down: bool,
        interruptible: bool,
        toggle: Option<bool>,
    },
    Button {
        button: u8,
        down: bool,
    },
    Click {
        button: u8,
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
    RelativeScaled {
        screen: Option<i32>,
        x: f64,
        y: f64,
    },
    Wheel {
        x: i32,
        y: i32,
    },
    Text(String),
    Combination {
        down: bool,
        keys: Vec<KeyOrButton>,
    },
    Touch {
        kind: u8,
        points: Vec<Point>,
    },
}
#[derive(Clone, Copy, Serialize, Deserialize)]
pub(crate) enum Command {
    Desktop,
    TaskView,
    Lock,
    TaskManager,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) enum KeyOrButton {
    Key(u16),
    Button(u8),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct Point {
    pub id: u32,
    pub x: f64,
    pub y: f64,
    pub radius_x: f64,
    pub radius_y: f64,
    pub angle: f64,
    pub pressure: f64,
}

impl Event {
    pub(crate) fn validate(&self) -> Result<()> {
        match self {
            Self::Key { key: k, .. } => {
                key(i32::from(*k))?;
            }
            Self::Button { button: b, .. } | Self::Click { button: b } => {
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
            Self::RelativeScaled { x, y, .. } => ensure!(
                x.is_finite() && y.is_finite() && x.abs() <= 1.0 && y.abs() <= 1.0,
                "相对坐标越界"
            ),
            Self::Text(text) => ensure!(text.len() <= MAX_TEXT, "输入文字过长"),
            Self::Combination { keys, .. } => {
                ensure!(keys.len() <= 64, "组合键过长");
                for k in keys {
                    match k {
                        KeyOrButton::Key(k) => {
                            key(i32::from(*k))?;
                        }
                        KeyOrButton::Button(b) => {
                            button(i32::from(*b))?;
                        }
                    }
                }
            }
            Self::Touch { kind, points } => {
                ensure!(
                    (1..=4).contains(kind) && points.len() <= 10,
                    "触摸类型或触点数量无效"
                );
                let mut ids = std::collections::BTreeSet::new();
                for p in points {
                    ensure!(
                        ids.insert(p.id)
                            && [p.x, p.y, p.radius_x, p.radius_y, p.angle, p.pressure]
                                .iter()
                                .all(|n| n.is_finite()),
                        "触摸属性无效"
                    );
                    ensure!(
                        (0.0..=1.0).contains(&p.x)
                            && (0.0..=1.0).contains(&p.y)
                            && (0.0..=65536.0).contains(&p.radius_x)
                            && (0.0..=65536.0).contains(&p.radius_y)
                            && (0.0..=360.0).contains(&p.angle)
                            && (0.0..=1.0).contains(&p.pressure),
                        "触摸属性越界"
                    );
                }
            }
            Self::Heartbeat | Self::Wheel { .. } | Self::Command(_) => {}
        }
        Ok(())
    }
}

fn integer(v: &Value, name: &str) -> Result<i32> {
    v.get(name)
        .and_then(Value::as_i64)
        .and_then(|n| n.try_into().ok())
        .ok_or_else(|| anyhow::anyhow!("输入整数字段无效"))
}
fn number(v: &Value, name: &str) -> Result<f64> {
    v.get(name)
        .and_then(Value::as_f64)
        .filter(|n| n.is_finite())
        .ok_or_else(|| anyhow::anyhow!("输入坐标字段无效"))
}
fn screen(v: &Value) -> Result<Option<i32>> {
    v.get("screen_id")
        .map(|_| integer(v, "screen_id"))
        .transpose()
}
fn key(v: i32) -> Result<u16> {
    ensure!((1..=254).contains(&v), "无效键码");
    Ok(v as u16)
}
fn button(v: i32) -> Result<u8> {
    ensure!(matches!(v, 1 | 2 | 16 | 32 | 64), "无效鼠标按键");
    Ok(v as u8)
}

/// None means an unknown JSON action; callers still consume the JSON envelope.
pub(crate) fn json(bytes: &[u8]) -> Result<Option<Event>> {
    ensure!(bytes.len() <= MAX_MESSAGE, "输入消息过大");
    let v: Value = serde_json::from_slice(bytes).map_err(|_| anyhow::anyhow!("输入JSON无效"))?;
    ensure!(v.is_object(), "输入必须为对象");
    if v.get("heartbeat").is_some() {
        return Ok(Some(Event::Heartbeat));
    }
    let Some(action) = v.get("action").and_then(Value::as_str) else {
        return Ok(None);
    };
    Ok(Some(match action {
        "kbd_press" | "kbd_release" => {
            let k = key(integer(&v, "key")?)?;
            let toggle_key = match k {
                144 => Some("lockkeysstatus"),
                20 | 145 => Some("status_key_value"),
                _ => None,
            };
            let toggle = toggle_key
                .filter(|n| v.get(*n).is_some())
                .map(|n| integer(&v, n))
                .transpose()?
                .map(|n| match n {
                    0 => Ok(false),
                    128 => Ok(true),
                    _ => Err(anyhow::anyhow!("锁定键状态无效")),
                })
                .transpose()?;
            Event::Key {
                key: k,
                down: action == "kbd_press",
                interruptible: v.get("interrept").is_some(),
                toggle,
            }
        }
        "mouse_press" | "mouse_release" => Event::Button {
            button: button(integer(&v, "button")?)?,
            down: action == "mouse_press",
        },
        "mouse_click" => Event::Click {
            button: button(integer(&v, "button")?)?,
        },
        "mouse_move_absolute" => {
            let (x, y) = (number(&v, "abs_x")?, number(&v, "abs_y")?);
            ensure!(
                (0.0..=1.0).contains(&x) && (0.0..=1.0).contains(&y),
                "绝对坐标越界"
            );
            Event::Absolute {
                screen: screen(&v)?,
                x,
                y,
            }
        }
        "mouse_move_relative" => Event::Relative {
            x: integer(&v, "delta_x")?,
            y: integer(&v, "delta_y")?,
        },
        "mouse_move_variety_relative" => {
            let (x, y) = (number(&v, "rel_x")?, number(&v, "rel_y")?);
            ensure!(x.abs() <= 1.0 && y.abs() <= 1.0, "相对坐标越界");
            Event::RelativeScaled {
                screen: screen(&v)?,
                x,
                y,
            }
        }
        "mouse_scroll" => Event::Wheel {
            x: if v.get("delta_x").is_some() {
                integer(&v, "delta_x")?
            } else {
                0
            },
            y: if v.get("delta_y").is_some() {
                integer(&v, "delta_y")?
            } else {
                0
            },
        },
        "text_input" => {
            let text = v
                .get("content")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("缺少输入文字"))?;
            ensure!(text.len() <= MAX_TEXT, "输入文字过长");
            Event::Text(text.to_owned())
        }
        "combination_keys_press" | "combination_keys_release" => {
            let encoded = v
                .get("input_keys")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("组合键格式无效"))?;
            let entries: Vec<Value> =
                serde_json::from_str(encoded).map_err(|_| anyhow::anyhow!("组合键格式无效"))?;
            ensure!(entries.len() <= 64, "组合键过长");
            let mut keys = Vec::with_capacity(entries.len());
            for item in entries {
                let code = integer(&item, "input_code")?;
                keys.push(match item.get("input_device").and_then(Value::as_str) {
                    Some("keyboard") => KeyOrButton::Key(key(code)?),
                    Some("mouse") => KeyOrButton::Button(button(code)?),
                    _ => bail!("组合键设备无效"),
                });
            }
            Event::Combination {
                down: action == "combination_keys_press",
                keys,
            }
        }
        _ => return Ok(None),
    }))
}

#[derive(Clone, PartialEq, Message)]
struct Input {
    #[prost(message, optional, tag = "1")]
    touch: Option<Touch>,
}
#[derive(Clone, PartialEq, Message)]
struct Touch {
    #[prost(int32, tag = "1")]
    kind: i32,
    #[prost(message, repeated, tag = "2")]
    points: Vec<TouchPoint>,
}
#[derive(Clone, PartialEq, Message)]
struct TouchPoint {
    #[prost(uint32, tag = "1")]
    id: u32,
    #[prost(float, tag = "2")]
    x: f32,
    #[prost(float, tag = "3")]
    y: f32,
    #[prost(float, tag = "4")]
    rx: f32,
    #[prost(float, tag = "5")]
    ry: f32,
    #[prost(float, tag = "6")]
    angle: f32,
    #[prost(float, tag = "7")]
    pressure: f32,
}
pub(crate) fn touch(bytes: &[u8]) -> Result<Option<Event>> {
    ensure!(bytes.len() <= MAX_MESSAGE, "触摸消息过大");
    let input = Input::decode(bytes).map_err(|_| anyhow::anyhow!("触摸消息无效"))?;
    let Some(touch) = input.touch else {
        return Ok(None);
    };
    ensure!(
        (1..=4).contains(&touch.kind) && touch.points.len() <= MAX_TOUCH_POINTS,
        "触摸类型或触点数量无效"
    );
    let mut ids = std::collections::BTreeSet::new();
    let mut points = Vec::with_capacity(touch.points.len());
    for p in touch.points {
        ensure!(ids.insert(p.id), "重复触点ID");
        ensure!(
            [p.x, p.y, p.rx, p.ry, p.angle, p.pressure]
                .iter()
                .all(|n| n.is_finite()),
            "触摸属性无效"
        );
        ensure!(
            (0.0..=1.0).contains(&p.x)
                && (0.0..=1.0).contains(&p.y)
                && p.rx >= 0.0
                && p.ry >= 0.0
                && p.rx <= 65536.0
                && p.ry <= 65536.0
                && (0.0..=360.0).contains(&p.angle)
                && (0.0..=1.0).contains(&p.pressure),
            "触摸属性越界"
        );
        points.push(Point {
            id: p.id,
            x: p.x.into(),
            y: p.y.into(),
            radius_x: p.rx.into(),
            radius_y: p.ry.into(),
            angle: p.angle.into(),
            pressure: p.pressure.into(),
        });
    }
    Ok(Some(Event::Touch {
        kind: touch.kind as u8,
        points,
    }))
}

/// The inverse of [`touch`]: encodes touch for the binary input channel the
/// mobile client uses. Callers pre-validated with [`Event::validate`].
pub(crate) fn encode_touch(kind: u8, points: &[Point]) -> Vec<u8> {
    Input {
        touch: Some(Touch {
            kind: i32::from(kind),
            points: points
                .iter()
                .map(|p| TouchPoint {
                    id: p.id,
                    x: p.x as f32,
                    y: p.y as f32,
                    rx: p.radius_x as f32,
                    ry: p.radius_y as f32,
                    angle: p.angle as f32,
                    pressure: p.pressure as f32,
                })
                .collect(),
        }),
    }
    .encode_to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn touch_round_trip() {
        let points = vec![
            Point {
                id: 7,
                x: 0.25,
                y: 0.5,
                radius_x: 0.0,
                radius_y: 0.0,
                angle: 0.0,
                pressure: 0.0,
            },
            Point {
                id: 3,
                x: 0.75,
                y: 1.0,
                radius_x: 12.0,
                radius_y: 14.0,
                angle: 90.0,
                pressure: 0.5,
            },
        ];
        for kind in 1..=4 {
            let Event::Touch { kind: k, points: p } = touch(&encode_touch(kind, &points))
                .expect("decodes")
                .expect("is a touch event")
            else {
                panic!("expected a touch event");
            };
            assert_eq!((k, p), (kind, points.clone()));
        }
    }

    #[test]
    fn touch_lift_frame_has_no_points() {
        let Event::Touch { kind, points } = touch(&encode_touch(3, &[]))
            .expect("decodes")
            .expect("is a touch event")
        else {
            panic!("expected a touch event");
        };
        assert_eq!((kind, points), (3, vec![]));
    }
}
