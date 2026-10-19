//! Small policy/status JSON plus a binary payload over the existing protected
//! pipe. File blocks never expand into hundreds of thousands of JSON numbers.
use super::agent::{Reply, Request};
use crate::platform::windows::host_service::pipe::Pipe;
use anyhow::{Result, ensure};
use prost::Message;
use std::time::Duration;
#[derive(Clone, PartialEq, Message)]
struct Frame {
    #[prost(bytes = "vec", tag = "1")]
    metadata: Vec<u8>,
    #[prost(bytes = "vec", tag = "2")]
    payload: Vec<u8>,
}
pub(super) trait Wire: serde::Serialize + serde::de::DeserializeOwned {
    fn payload(&mut self) -> Option<&mut Vec<u8>>;
}
impl Wire for Request {
    fn payload(&mut self) -> Option<&mut Vec<u8>> {
        self.packet.as_mut()
    }
}
impl Wire for Reply {
    fn payload(&mut self) -> Option<&mut Vec<u8>> {
        self.packet.as_mut().map(|p| &mut p.data)
    }
}
pub(super) fn send<T: Wire>(
    pipe: &Pipe,
    mut message: T,
    permitted: impl Fn() -> bool,
) -> Result<()> {
    let payload = message.payload().map(std::mem::take).unwrap_or_default();
    let metadata = serde_json::to_vec(&message)?;
    ensure!(
        metadata.len() <= 128 * 1024 && payload.len() < 524288,
        "剪贴板代理消息过大"
    );
    pipe.send_raw(Frame { metadata, payload }.encode_to_vec(), permitted)
}
pub(super) fn receive<T: Wire>(
    pipe: &Pipe,
    timeout: Duration,
    permitted: impl Fn() -> bool,
) -> Result<T> {
    let bytes = pipe.receive_raw(timeout, permitted)?;
    ensure!(bytes.len() < 656000, "剪贴板代理消息过大");
    let frame = Frame::decode(bytes.as_slice())?;
    ensure!(
        frame.metadata.len() <= 128 * 1024 && frame.payload.len() < 524288,
        "剪贴板代理消息过大"
    );
    let mut message: T = serde_json::from_slice(&frame.metadata)?;
    if let Some(payload) = message.payload() {
        ensure!(payload.is_empty(), "剪贴板代理重复载荷");
        *payload = frame.payload;
    } else {
        ensure!(frame.payload.is_empty(), "剪贴板代理载荷没有归属");
    }
    Ok(message)
}
