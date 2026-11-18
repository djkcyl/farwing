//! Official UU CONTROL gamepad messages, using XInput button and axis units.
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};

pub(crate) const MAX_GAMEPADS: u8 = 4;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct GamepadState {
    pub buttons: u16,
    pub left_trigger: u8,
    pub right_trigger: u8,
    pub left_thumb_x: i16,
    pub left_thumb_y: i16,
    pub right_thumb_x: i16,
    pub right_thumb_y: i16,
}

impl GamepadState {
    /// Analog samples may be replaced, but button and trigger edges must survive.
    pub(crate) fn coalesces_with(self, next: Self) -> bool {
        self.buttons == next.buttons
            && (self.left_trigger == 0) == (next.left_trigger == 0)
            && (self.right_trigger == 0) == (next.right_trigger == 0)
    }

    /// Activation waits for released controls, without treating stick noise as a hold.
    // Only the Linux viewer samples controllers so far (SDL2); the Windows
    // controller has no collector yet, see README's 功能矩阵.
    #[cfg_attr(windows, allow(dead_code))]
    pub(crate) fn at_rest(self) -> bool {
        self.buttons == 0
            && self.left_trigger <= 4
            && self.right_trigger <= 4
            && [
                self.left_thumb_x,
                self.left_thumb_y,
                self.right_thumb_x,
                self.right_thumb_y,
            ]
            .into_iter()
            .all(|axis| i32::from(axis).abs() <= 4096)
    }
}

// Only the Linux viewer samples controllers so far (SDL2); the Windows
// controller has no collector yet, see README's 功能矩阵.
#[cfg_attr(windows, allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GamepadEvent {
    Connect { index: u8 },
    Update { index: u8, state: GamepadState },
    Disconnect { index: u8 },
}

impl GamepadEvent {
    #[cfg_attr(windows, allow(dead_code))]
    pub(crate) fn index(self) -> u8 {
        match self {
            Self::Connect { index } | Self::Update { index, .. } | Self::Disconnect { index } => {
                index
            }
        }
    }

    pub(crate) fn release(self) -> bool {
        matches!(self, Self::Disconnect { .. })
            || matches!(self, Self::Update { state, .. } if state == GamepadState::default())
    }

    pub(crate) fn json(self) -> Value {
        match self {
            Self::Connect { index } => json!({"action": "gpd_connect", "index": index}),
            Self::Disconnect { index } => json!({"action": "gpd_disconnect", "index": index}),
            Self::Update { index, state } => json!({
                "action": "gpd_update", "index": index, "buttons": state.buttons,
                "left_trigger": state.left_trigger, "right_trigger": state.right_trigger,
                "left_thumb_x": state.left_thumb_x, "left_thumb_y": state.left_thumb_y,
                "right_thumb_x": state.right_thumb_x, "right_thumb_y": state.right_thumb_y,
            }),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct GamepadVibration {
    pub index: u8,
    pub left_motor: u8,
    pub right_motor: u8,
}

impl GamepadVibration {
    pub(crate) fn motors(self) -> (u16, u16) {
        (
            u16::from(self.left_motor) * 257,
            u16::from(self.right_motor) * 257,
        )
    }

    pub(crate) fn parse(value: &Value) -> Result<Option<Self>> {
        if value.get("action").and_then(Value::as_str) != Some("gpd_vibration") {
            return Ok(None);
        }
        let number = |name| {
            value
                .get(name)
                .and_then(Value::as_u64)
                .with_context(|| format!("Invalid gamepad {name}"))
        };
        let index = number("index")?;
        ensure!(
            index < u64::from(MAX_GAMEPADS),
            "Gamepad index outside 0..4"
        );
        let left_motor =
            u8::try_from(number("left_motor")?).context("Invalid left motor strength")?;
        let right_motor =
            u8::try_from(number("right_motor")?).context("Invalid right motor strength")?;
        Ok(Some(Self {
            index: index as u8,
            left_motor,
            right_motor,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn official_control_messages_use_zero_based_slots_and_xinput_units() {
        assert_eq!(
            GamepadEvent::Connect { index: 0 }.json(),
            json!({"action":"gpd_connect","index":0})
        );
        let event = GamepadEvent::Update {
            index: 0,
            state: GamepadState {
                buttons: 0x1001,
                left_trigger: 255,
                left_thumb_y: 32767,
                right_thumb_x: -32767,
                ..Default::default()
            },
        };
        assert_eq!(
            event.json(),
            json!({"action":"gpd_update","index":0,"buttons":4097,
            "left_trigger":255,"right_trigger":0,"left_thumb_x":0,"left_thumb_y":32767,
            "right_thumb_x":-32767,"right_thumb_y":0})
        );
        assert_eq!(
            GamepadEvent::Disconnect { index: 3 }.json(),
            json!({"action":"gpd_disconnect","index":3})
        );
    }

    #[test]
    fn activation_tolerates_noise_but_never_a_held_button_or_axis() {
        assert!(
            GamepadState {
                left_thumb_x: -50,
                right_trigger: 2,
                ..Default::default()
            }
            .at_rest()
        );
        assert!(
            !GamepadState {
                buttons: 0x1000,
                ..Default::default()
            }
            .at_rest()
        );
        assert!(
            !GamepadState {
                left_thumb_x: i16::MIN,
                ..Default::default()
            }
            .at_rest()
        );
        assert!(
            !GamepadState {
                left_trigger: 5,
                ..Default::default()
            }
            .at_rest()
        );
    }

    #[test]
    fn feedback_rejects_unknown_slots_and_invalid_numeric_ranges() {
        let vibration = GamepadVibration::parse(&json!({"action":"gpd_vibration","index":0,
            "left_motor":255,"right_motor":128}))
        .unwrap()
        .unwrap();
        assert_eq!(vibration.motors(), (65535, 32896));
        assert!(
            GamepadVibration::parse(&json!({"action":"unrelated"}))
                .unwrap()
                .is_none()
        );
        for bad in [
            json!({"action":"gpd_vibration","index":4,"left_motor":0,"right_motor":0}),
            json!({"action":"gpd_vibration","index":0,"left_motor":-1,"right_motor":0}),
            json!({"action":"gpd_vibration","index":0,"left_motor":256,"right_motor":0}),
        ] {
            assert!(GamepadVibration::parse(&bad).is_err());
        }
    }
}
