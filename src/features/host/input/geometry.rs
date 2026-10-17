//! Screen rectangles input coordinates are resolved against, in desktop
//! pixels. Platform engines convert them to their native rectangle types.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

/// A half-open desktop rectangle: `right` and `bottom` are exclusive.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Screen {
    pub id: i32,
    pub left: i32,
    pub top: i32,
    pub width: u32,
    pub height: u32,
    /// The same screen as the compositor lays it out, for the sessions where a
    /// position is named in that space instead. Empty where the display server
    /// is the compositor.
    #[serde(default)]
    pub logical: Option<Rect>,
}
impl Screen {
    pub fn rect(&self) -> Result<Rect> {
        ensure!(
            self.width > 0 && self.height > 0 && self.width <= 65536 && self.height <= 65536,
            "屏幕尺寸无效"
        );
        Ok(Rect {
            left: self.left,
            top: self.top,
            right: self
                .left
                .checked_add(self.width as i32)
                .ok_or_else(|| anyhow::anyhow!("屏幕坐标溢出"))?,
            bottom: self
                .top
                .checked_add(self.height as i32)
                .ok_or_else(|| anyhow::anyhow!("屏幕坐标溢出"))?,
        })
    }
}
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Geometry {
    pub current: i32,
    pub screens: Vec<Screen>,
}
impl Geometry {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.screens.len() <= 32, "输入屏幕数量无效");
        let mut ids = std::collections::BTreeSet::new();
        for screen in &self.screens {
            screen.rect()?;
            ensure!(ids.insert(screen.id), "重复输入屏幕");
        }
        Ok(())
    }
    pub fn screen(&self, id: Option<i32>) -> Result<&Screen> {
        self.screens
            .iter()
            .find(|s| s.id == id.unwrap_or(self.current))
            .ok_or_else(|| anyhow::anyhow!("输入目标屏幕已失效"))
    }
}
