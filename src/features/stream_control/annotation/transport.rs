//! Wire-specific reply validation. Editor state only observes completed effects.
use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Outcome {
    Applied,
    NotLoggedIn,
    Locked,
    Rejected,
    Unknown(i32),
}
impl From<i32> for Outcome {
    fn from(code: i32) -> Self {
        match code {
            0 => Self::Applied,
            2 => Self::NotLoggedIn,
            3 => Self::Locked,
            4 => Self::Rejected,
            other => Self::Unknown(other),
        }
    }
}
impl Outcome {
    pub(super) fn message(self) -> String {
        match self {
            Self::NotLoggedIn => "被控端未登录，批注已关闭".into(),
            Self::Locked => "被控端已锁定".into(),
            Self::Rejected => "远端未完成批注操作".into(),
            Self::Unknown(code) => format!("批注操作未完成（{code}）"),
            Self::Applied => unreachable!("successful operation has no error message"),
        }
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Reply {
    Native,
}
pub(super) struct InFlight {
    pub effect: Pending,
    pub reply: Reply,
    pub sent: Instant,
}
impl StreamControlHandle {
    pub(super) fn send_draw(
        &self,
        s: &mut StreamControlState,
        request: PbDrawRequest,
        pending: Pending,
    ) -> Result<i64> {
        // Annotation always travels as native ANNOTATION frames.
        if s.annotation.native_token.is_none() {
            bail!("对端批注尚未就绪");
        }
        self.send_native_draw(
            s,
            crate::protocol::annotation::Command::new(
                crate::protocol::annotation::Operation::Draw(request),
            ),
            pending,
        )
    }
    pub(super) fn enqueue_annotation(
        &self,
        s: &mut StreamControlState,
        seq: i64,
        payload: Vec<u8>,
        effect: Pending,
        reply: Reply,
    ) -> Result<i64> {
        s.annotation.pending.insert(
            seq,
            InFlight {
                effect,
                reply,
                sent: Instant::now(),
            },
        );
        let outgoing = OutgoingControlMessage {
            sequence: seq,
            payload,
            protocol: protocol(s),
            completion: None,
            annotation_generation: Some(s.annotation.generation),
        };
        if self.outgoing.send(outgoing).is_err() {
            s.annotation.uncertain("批注发送任务已停止".into());
            bail!("批注发送任务已停止");
        }
        Ok(seq)
    }
}
