//! Remote pointer state, independent of video decoding and local input.
//!
//! The host sends its cursor shape in `CURSOR` frames on the control channel.
//! `pos_x/y` are image hotspots; `coordinate_*_scale` are sampled at shape
//! changes, not continuously transmitted. Watching uses capture-side cursor
//! composition.

use std::sync::{Arc, Mutex};

use anyhow::{Result, ensure};
use prost::Message;

use crate::protocol::participants::ParticipantPointer;

const MAX_PNG_BYTES: usize = 4 * 1024 * 1024;
const MAX_CURSOR_SIZE: u32 = 2048;

#[derive(Clone, Debug, PartialEq)]
pub struct CursorImage {
    pub png: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub hotspot: [u32; 2],
    pub system_type: i32,
}

#[derive(Clone, Debug)]
pub struct RemoteCursor {
    pub image: Arc<CursorImage>,
    /// Position sampled with this shape, not a live pointer position.
    pub sampled_position: Option<[f64; 2]>,
    pub screen_id: i32,
}

#[derive(Clone, Default)]
pub(crate) struct RemoteCursorState(Arc<Mutex<CursorState>>);

#[derive(Default)]
struct CursorState {
    cursor: Option<RemoteCursor>,
    hidden: bool,
}

impl RemoteCursorState {
    pub fn snapshot(&self) -> Option<RemoteCursor> {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .cursor
            .clone()
    }

    pub fn hidden(&self) -> bool {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).hidden
    }

    pub fn clear(&self) {
        self.publish(None, false);
    }

    fn publish(&self, mut cursor: Option<RemoteCursor>, hidden: bool) {
        let mut state = self.0.lock().unwrap_or_else(|p| p.into_inner());
        if let (Some(previous), Some(next)) = (state.cursor.as_ref(), &mut cursor)
            && previous.image == next.image
        {
            next.image = previous.image.clone();
        }
        state.cursor = cursor;
        state.hidden = hidden;
    }

    /// Apply a `CURSOR` body. A participants update returns the other
    /// participants' pointers instead.
    pub fn receive(&self, bytes: &[u8]) -> Result<Option<Vec<ParticipantPointer>>> {
        ensure!(
            bytes.len() <= MAX_PNG_BYTES + 1024,
            "cursor state message too large"
        );
        let message = CursorMessage::decode(bytes)?;
        if message.pointers {
            return Ok(Some(message.participants));
        }
        let Some(shape) = message.shape else {
            return Ok(None);
        };
        tracing::trace!(
            screen = shape.screen_id,
            kind = shape.cursor_type,
            width = shape.width,
            height = shape.height,
            bytes = shape.byte_value.len(),
            x = shape.coordinate_x_scale,
            y = shape.coordinate_y_scale,
            "remote cursor state"
        );
        // Hidden by a program on a captured screen; on another screen (-1)
        // the pointer is merely outside the picture.
        let hidden = shape.cursor_type == -1 && shape.screen_id >= 0;
        let parsed = parse_shape(shape);
        match parsed {
            Ok(cursor) => self.publish(cursor, hidden),
            Err(error) => {
                self.clear();
                return Err(error);
            }
        }
        Ok(None)
    }
}

fn parse_shape(shape: CursorShape) -> Result<Option<RemoteCursor>> {
    // Type -1 without an image: the host cursor is hidden.
    if shape.cursor_type == -1 {
        return Ok(None);
    }
    let position = [shape.coordinate_x_scale, shape.coordinate_y_scale];
    let sampled_position = (shape.screen_id >= 0
        && position
            .iter()
            .all(|v| v.is_finite() && (0.0..=1.0).contains(v)))
    .then_some(position);
    ensure!(
        shape.byte_value.len() <= MAX_PNG_BYTES,
        "cursor PNG too large"
    );
    ensure!(
        shape.width >= 0 && shape.height >= 0,
        "negative cursor dimensions"
    );
    let (width, height) = (shape.width as u32, shape.height as u32);
    ensure!(
        width <= MAX_CURSOR_SIZE && height <= MAX_CURSOR_SIZE,
        "cursor dimensions too large"
    );
    if !shape.byte_value.is_empty() {
        ensure!(width > 0 && height > 0, "empty cursor dimensions");
        ensure!(
            shape.pos_x >= 0
                && shape.pos_y >= 0
                && shape.pos_x < shape.width
                && shape.pos_y < shape.height,
            "invalid cursor hotspot"
        );
    }
    Ok(Some(RemoteCursor {
        image: Arc::new(CursorImage {
            png: shape.byte_value,
            width,
            height,
            hotspot: [shape.pos_x.max(0) as u32, shape.pos_y.max(0) as u32],
            system_type: shape.cursor_type,
        }),
        sampled_position,
        screen_id: shape.screen_id,
    }))
}

/// Body of a `CURSOR` frame.
#[derive(Clone, PartialEq, Message)]
struct CursorMessage {
    #[prost(message, optional, tag = "1")]
    shape: Option<CursorShape>,
    /// The other participants' pointers (`docs/protocol/v2-design.md` §4.3).
    #[prost(message, repeated, tag = "2")]
    participants: Vec<ParticipantPointer>,
    /// Marks a participants update; the list may be empty.
    #[prost(bool, tag = "3")]
    pointers: bool,
}

#[derive(Clone, PartialEq, Message)]
struct CursorShape {
    #[prost(int32, tag = "1")]
    pos_x: i32,
    #[prost(int32, tag = "2")]
    pos_y: i32,
    #[prost(int32, tag = "3")]
    width: i32,
    #[prost(int32, tag = "4")]
    height: i32,
    #[prost(bytes, tag = "5")]
    byte_value: Vec<u8>,
    #[prost(int32, tag = "6")]
    cursor_type: i32,
    #[prost(double, tag = "7")]
    coordinate_x_scale: f64,
    #[prost(double, tag = "8")]
    coordinate_y_scale: f64,
    #[prost(int32, tag = "9")]
    screen_id: i32,
}

/// A complete `CURSOR` frame announcing the host cursor shape.
pub(crate) fn encode_shape(
    image: Option<&CursorImage>,
    screen_id: i32,
    position: [f64; 2],
) -> Vec<u8> {
    let shape = match image {
        Some(image) => CursorShape {
            pos_x: image.hotspot[0] as i32,
            pos_y: image.hotspot[1] as i32,
            width: image.width as i32,
            height: image.height as i32,
            byte_value: image.png.clone(),
            cursor_type: image.system_type,
            coordinate_x_scale: position[0],
            coordinate_y_scale: position[1],
            screen_id,
        },
        None => CursorShape {
            cursor_type: -1,
            screen_id,
            ..Default::default()
        },
    };
    crate::protocol::peer::message(
        crate::protocol::peer::FrameType::Cursor,
        &CursorMessage {
            shape: Some(shape),
            ..Default::default()
        },
    )
}

/// A complete `CURSOR` frame with the other participants' pointers.
pub(crate) fn encode_pointers(participants: Vec<ParticipantPointer>) -> Vec<u8> {
    crate::protocol::peer::message(
        crate::protocol::peer::FrameType::Cursor,
        &CursorMessage {
            shape: None,
            participants,
            pointers: true,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_cursor_hidden_on_a_captured_screen_is_hidden() {
        let state = RemoteCursorState::default();
        let elsewhere = encode_shape(None, -1, [0.0, 0.0]);
        let (_, body) = crate::protocol::peer::decode(&elsewhere).unwrap();
        state.receive(body).unwrap();
        assert!(!state.hidden(), "outside the picture is not hidden");
        let hidden = encode_shape(None, 0, [0.5, 0.5]);
        let (_, body) = crate::protocol::peer::decode(&hidden).unwrap();
        state.receive(body).unwrap();
        assert!(state.hidden());
    }
}
