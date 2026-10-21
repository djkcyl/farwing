//! Host input configuration: keyboard interrupts and per-application mouse rules.
use crate::i18n::t;
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Rule {
    pub mode: u8,
    pub simulation: bool,
}
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Configuration {
    pub keyboard_interrupts: bool,
    pub games: BTreeMap<String, Rule>,
}
impl Default for Configuration {
    fn default() -> Self {
        Self {
            keyboard_interrupts: true,
            games: Default::default(),
        }
    }
}
impl Configuration {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.games.len() <= 8192,
            "{}",
            t().features.input_policy_too_many
        );
        for (name, rule) in &self.games {
            ensure!(
                !name.is_empty() && name.len() <= 512 && rule.mode <= 2,
                "{}",
                t().features.input_policy_entry_invalid
            );
        }
        ensure!(
            serde_json::to_vec(self)?.len() <= 384 * 1024,
            "{}",
            t().features.input_policy_too_large
        );
        Ok(())
    }
}
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct MousePolicy {
    pub mode: Option<u8>,
    pub restore: Option<[f64; 2]>,
}
impl MousePolicy {
    pub fn message(&self) -> Option<Vec<u8>> {
        let mode = self.mode?;
        let mut value = serde_json::json!({"mouse_mode":mode,"force_mode":mode!=2});
        if mode == 2
            && let Some([x, y]) = self.restore
        {
            value["coordinate_x_scale"] = x.into();
            value["coordinate_y_scale"] = y.into();
        }
        Some(crate::protocol::peer::frame(
            crate::protocol::peer::FrameType::PointerPolicy,
            &serde_json::to_vec(&value).expect("validated finite mouse policy"),
        ))
    }
}
