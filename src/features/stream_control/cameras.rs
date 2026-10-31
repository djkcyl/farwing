//! The host's cameras as this viewer sees them (`docs/protocol/camera-design.md`
//! §6): their details from `Cameras`, opening one with the parameters this
//! viewer chose for the device before, and changing parameters and
//! adjustments.
use super::camera::{CameraAdjust, CameraControlKind, CameraInfo, CameraOp, CameraParams, CameraRequest, SetCameraParams};
use super::requests::Waiting;
use super::wire::SessionBody;
use super::{StreamControlHandle, lock};
use crate::media::camera::{Control, Params};
use anyhow::Result;
use std::collections::BTreeMap;
use std::time::Duration;

/// This viewer's camera state.
#[derive(Default)]
pub(super) struct Cameras {
    pub(super) list: Vec<CameraInfo>,
    /// The parameters this viewer chose, by camera display ID, as saved for
    /// the device.
    pub(super) chosen: BTreeMap<i32, Params>,
    pub(super) store: Option<crate::features::viewing_settings::ViewingSettingsStore>,
    /// Why the last camera request failed.
    pub(super) error: Option<String>,
}

impl StreamControlHandle {
    /// The cameras this participant may see.
    pub(crate) fn cameras(&self) -> Vec<CameraInfo> {
        lock(&self.shared).cameras.list.clone()
    }
    pub(crate) fn camera_error(&self) -> Option<String> {
        lock(&self.shared).cameras.error.clone()
    }
    /// The parameters this viewer chose for camera `id`, if any.
    pub(crate) fn chosen_camera_params(&self, id: i32) -> Option<Params> {
        lock(&self.shared).cameras.chosen.get(&id).copied()
    }
    /// Remember the saved choices of this device's cameras and save later
    /// ones there.
    pub(crate) fn attach_camera_store(
        &self,
        store: crate::features::viewing_settings::ViewingSettingsStore,
        chosen: BTreeMap<i32, Params>,
    ) {
        let mut state = lock(&self.shared);
        state.cameras.store = Some(store);
        state.cameras.chosen = chosen;
    }
    /// Show camera `id`: with this viewer's parameters for it, if it chose
    /// any, else the owner's.
    pub(crate) async fn open_camera(&self, id: i32) -> Result<()> {
        if let Some(params) = self.chosen_camera_params(id) {
            self.send_camera(CameraOp::Params(SetCameraParams {
                camera: id,
                params: Some(params.into()),
            }))?;
        }
        self.set_screen_capture(id, true).await
    }
    /// Use `params` for camera `id` (`None`: the owner's again) and remember
    /// the choice for this device.
    pub(crate) fn set_camera_params(&self, id: i32, params: Option<Params>) -> Result<()> {
        let store = {
            let mut state = lock(&self.shared);
            match params {
                Some(params) => state.cameras.chosen.insert(id, params),
                None => state.cameras.chosen.remove(&id),
            };
            state.cameras.store.clone()
        };
        if let Some(store) = store {
            tokio::spawn(async move {
                if let Err(error) = store.save_camera(id, params).await {
                    tracing::warn!(error = %format!("{error:#}"), "camera parameters not saved");
                }
            });
        }
        self.send_camera(match params {
            Some(params) => CameraOp::Params(SetCameraParams {
                camera: id,
                params: Some(CameraParams::from(params)),
            }),
            None => CameraOp::Reset(id),
        })
    }
    /// Set a picture adjustment of camera `id`; `auto` hands it to the camera.
    pub(crate) fn adjust_camera(&self, id: i32, control: Control, value: i32, auto: bool) -> Result<()> {
        self.send_camera(CameraOp::Adjust(CameraAdjust {
            camera: id,
            control: CameraControlKind::from(control) as i32,
            value,
            auto,
        }))
    }
    fn send_camera(&self, op: CameraOp) -> Result<()> {
        let mut state = lock(&self.shared);
        state.cameras.error = None;
        self.send_request(
            &mut state,
            SessionBody::CameraRequest(CameraRequest { op: Some(op) }),
            Waiting::Camera,
            Duration::from_secs(10),
        )?;
        Ok(())
    }
}
