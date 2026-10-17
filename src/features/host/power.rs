//! Authenticated host-room power requests. Never accepts peer DATA channel commands.
use crate::session::host_client::HostClient;
use anyhow::{Result, ensure};
use serde_json::Value;
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::mpsc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    Shutdown,
    Reboot,
}
#[derive(Debug)]
struct Command {
    action: Action,
    call_id: Option<String>,
    need_response: bool,
}
fn parse(push: &Value) -> Result<Option<Command>> {
    let kind = push.get("type").and_then(Value::as_str);
    if !matches!(kind, Some("device_reboot" | "transport_rpc")) {
        return Ok(None);
    }
    let data = push
        .get("data")
        .filter(|v| v.is_object())
        .ok_or_else(|| anyhow::anyhow!("电源请求缺少对象数据"))?;
    if kind == Some("device_reboot") {
        return Ok(Some(Command {
            action: Action::Reboot,
            call_id: None,
            need_response: false,
        }));
    }
    if data.get("p").and_then(Value::as_str) != Some("shutdown") {
        return Ok(None);
    }
    let need_response = match data.get("need_response") {
        Some(Value::Bool(v)) => *v,
        None => false,
        _ => anyhow::bail!("电源回执标记无效"),
    };
    let call_id = match data.get("call_id") {
        Some(Value::String(s)) if !s.is_empty() && s.len() <= 128 => Some(s.clone()),
        None => None,
        Some(Value::String(s)) if s.is_empty() => None,
        _ => anyhow::bail!("电源请求标识无效"),
    };
    ensure!(!need_response || call_id.is_some(), "电源请求缺少回执标识");
    Ok(Some(Command {
        action: Action::Shutdown,
        call_id,
        need_response,
    }))
}
#[derive(Default)]
struct State {
    binding: u64,
    epoch: u64,
    busy: bool,
    committed: bool,
    sender: Option<mpsc::Sender<Request>>,
    seen: VecDeque<String>,
    last_anonymous: Option<Instant>,
}
#[derive(Clone, Default)]
pub(crate) struct Handle(Arc<Mutex<State>>);
impl Handle {
    pub fn invalidate(&self) {
        let mut s = super::lock(&self.0);
        s.epoch = s.epoch.wrapping_add(1);
        s.busy = false;
        s.committed = false;
    }
    pub fn push(&self, push: &Value) -> bool {
        let command = match parse(push) {
            Ok(Some(c)) => c,
            Ok(None) => return false,
            Err(_) => {
                tracing::warn!("invalid host power push discarded");
                return true;
            }
        };
        let mut s = super::lock(&self.0);
        if s.busy || s.committed {
            return true;
        }
        if let Some(id) = &command.call_id {
            if s.seen.contains(id) {
                return true;
            }
        } else if s
            .last_anonymous
            .is_some_and(|at| at.elapsed() < Duration::from_secs(30))
        {
            return true;
        }
        let Some(sender) = s.sender.clone() else {
            return true;
        };
        if let Some(id) = &command.call_id {
            if s.seen.len() == 64 {
                s.seen.pop_front();
            }
            s.seen.push_back(id.clone());
        } else {
            s.last_anonymous = Some(Instant::now());
        }
        s.busy = true;
        let request = Request {
            command,
            owner: self.clone(),
            binding: s.binding,
            epoch: s.epoch,
        };
        // Never drop a request under the same lock its destructor needs.
        drop(s);
        if let Err(error) = sender.try_send(request) {
            drop(error);
        }
        true
    }
    pub fn bind(&self) -> Inbox {
        let (tx, rx) = mpsc::channel(1);
        let mut s = super::lock(&self.0);
        s.binding = s.binding.wrapping_add(1);
        s.busy = false;
        s.sender = Some(tx);
        Inbox {
            owner: self.clone(),
            binding: s.binding,
            receiver: rx,
            preparing: None,
        }
    }
}
pub(crate) struct Request {
    command: Command,
    owner: Handle,
    binding: u64,
    epoch: u64,
}
impl Request {
    pub fn valid(&self) -> bool {
        let s = super::lock(&self.owner.0);
        s.binding == self.binding && s.epoch == self.epoch && s.busy && !s.committed
    }
    pub fn action(&self) -> Action {
        self.command.action
    }
    fn claim(&self) -> Result<()> {
        let mut s = super::lock(&self.owner.0);
        ensure!(
            s.binding == self.binding && s.epoch == self.epoch && s.busy && !s.committed,
            "电源请求已取消"
        );
        s.committed = true;
        Ok(())
    }
    pub fn execute(&self, client: &HostClient) -> Result<()> {
        ensure!(
            client.is_active() && client.host.power_permitted(),
            "远程电源许可已撤销"
        );
        self.claim()?;
        let result = crate::platform::system_power::execute(self.command.action);
        if result.is_err() {
            let mut s = super::lock(&self.owner.0);
            if s.binding == self.binding && s.epoch == self.epoch {
                s.committed = false;
            }
        }
        result
    }
}
impl Drop for Request {
    fn drop(&mut self) {
        let mut s = super::lock(&self.owner.0);
        if s.binding == self.binding && s.epoch == self.epoch {
            s.busy = false;
        }
    }
}
pub(crate) enum Event {
    Prepare(Request),
    Ready(Result<Request>),
}
pub(crate) struct Inbox {
    owner: Handle,
    binding: u64,
    receiver: mpsc::Receiver<Request>,
    preparing: Option<tokio::task::JoinHandle<Result<Request>>>,
}
impl Inbox {
    pub async fn next(&mut self) -> Event {
        tokio::select! {
            request=self.receiver.recv()=>match request{Some(request)=>Event::Prepare(request),None=>std::future::pending().await},
            result=async {match &mut self.preparing{Some(task)=>task.await,None=>std::future::pending().await}}=>{
                self.preparing=None;Event::Ready(result.map_err(|e|anyhow::anyhow!("电源准备任务中断：{e}")).and_then(|r|r))
            }
        }
    }
    pub fn prepare(&mut self, request: Request, client: HostClient) {
        if let Some(old) = self.preparing.take() {
            old.abort();
        }
        self.preparing = Some(tokio::spawn(async move {
            let ended = client.ended();
            tokio::select! {biased; _=ended.cancelled()=>anyhow::bail!("账号已退出"),
                result=tokio::time::timeout(Duration::from_secs(15),prepare(request,&client))=>result.map_err(|_|anyhow::anyhow!("电源准备超时，未执行"))?
            }
        }));
    }
}
impl Drop for Inbox {
    fn drop(&mut self) {
        if let Some(task) = self.preparing.take() {
            task.abort();
        }
        let mut s = super::lock(&self.owner.0);
        if s.binding == self.binding {
            s.binding = s.binding.wrapping_add(1);
            s.sender = None;
            s.busy = false;
        }
    }
}
async fn prepare(request: Request, client: &HostClient) -> Result<Request> {
    let permitted = request.valid()
        && !client.is_guest()
        && client.is_active()
        && client.host.power_permitted();
    let ready = if permitted {
        tokio::task::spawn_blocking(crate::platform::system_power::probe)
            .await
            .map_err(|_| anyhow::anyhow!("读取关机权限失败"))
            .and_then(|r| r)
    } else {
        Err(anyhow::anyhow!("本机未允许远程关机和重启"))
    };
    if request.command.need_response && request.valid() && client.is_active() {
        client
            .power_response(
                request.command.call_id.clone().unwrap(),
                if ready.is_ok() { 0 } else { -1 },
                if ready.is_ok() {
                    "ok".into()
                } else {
                    "power request denied".into()
                },
            )
            .await?;
    }
    ready?;
    ensure!(
        request.valid() && client.is_active() && client.host.power_permitted(),
        "远程电源许可已撤销"
    );
    Ok(request)
}
