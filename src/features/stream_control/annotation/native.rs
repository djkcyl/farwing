//! Negotiated native edits share the controller's existing history and ownership.
use super::*;
use crate::protocol::annotation::{self as protocol, Command, Operation, Packet, Payload};
use anyhow::Context as _;

impl StreamControlHandle {
    pub(crate) fn handle_native_annotation(&self, bytes: &[u8]) -> Result<bool> {
        let Some(packet) = protocol::decode(bytes)? else {
            return Ok(false);
        };
        let mut s = lock(&self.shared);
        if !s.text_channel_open {
            return Ok(true);
        }
        match packet.payload {
            Some(Payload::Hello(1)) => {
                if s.annotation_extension != Some(packet.token) {
                    if s.annotation_extension.is_some() {
                        s.annotation.disconnect();
                    }
                    s.annotation_extension = Some(packet.token);
                    tracing::info!("Farwing native annotation v1 negotiated");
                }
            }
            Some(Payload::Result(code)) if s.annotation.native_token == Some(packet.token) => {
                if s.annotation
                    .pending
                    .get(&packet.request)
                    .is_some_and(|p| p.reply == Reply::Native)
                {
                    s.annotation.complete(packet.request, code.into());
                }
            }
            _ => {}
        }
        drop(s);
        self.mouse.repaint();
        Ok(true)
    }
    pub(super) fn send_native_draw(
        &self,
        s: &mut StreamControlState,
        command: Command,
        pending: Pending,
    ) -> Result<i64> {
        ensure_ready(s)?;
        let token = s.annotation.native_token.context("批注扩展尚未协商")?;
        anyhow::ensure!(s.annotation.pending.len() < MAX_PENDING, "批注请求仍在等待");
        let id = s.next_sequence;
        let bytes = protocol::encode(Packet {
            token,
            request: id,
            payload: Some(Payload::Command(command)),
        })?;
        s.next_sequence = id.wrapping_add(1);
        self.enqueue_annotation(s, id, bytes, pending, Reply::Native)
    }
    pub(super) fn native_replace(
        &self,
        s: &mut StreamControlState,
        stroke: &Stroke,
        pending: Pending,
    ) -> Result<i64> {
        let stroke = encoded_stroke(stroke, &stroke.points);
        self.send_native_draw(
            s,
            Command::new(Operation::Replace(protocol::Object {
                stroke: Some(stroke),
                transient: false,
            })),
            pending,
        )
    }
    pub(super) fn edit_native(
        &self,
        s: &mut StreamControlState,
        edit: Edit,
        direction: Direction,
    ) -> Result<()> {
        let reverse = matches!(direction, Direction::Undo);
        let clears_boards = matches!(&edit, Edit::Clear(_)) && s.annotation.uncertain;
        let restore = |stroke: &Stroke| {
            Command::new(Operation::Replace(protocol::Object {
                stroke: Some(encoded_stroke(stroke, &stroke.points)),
                transient: false,
            }))
        };
        let command = match (&edit, reverse) {
            (Edit::Clear(_), false) => Command::new(Operation::Clear(clears_boards)),
            (Edit::Add(stroke), true) | (Edit::Remove(stroke), false) => Command::new(
                Operation::Draw(clear_request(2, stroke.id, Some(stroke.screen))),
            ),
            (Edit::Add(stroke), false) | (Edit::Remove(stroke), true) => restore(stroke),
            (Edit::Clear(strokes), true) => {
                if strokes.is_empty() {
                    return Ok(());
                }
                Command::new(Operation::Batch(protocol::Batch {
                    commands: strokes.iter().map(restore).collect(),
                }))
            }
        };
        s.annotation.editing = Some(Editing {
            clears_boards,
            edit,
            direction,
            remaining: 1,
        });
        s.annotation.error = None;
        if let Err(error) = self.send_native_draw(s, command, Pending::Edit) {
            s.annotation.uncertain(error.to_string());
            return Err(error);
        }
        Ok(())
    }
}
