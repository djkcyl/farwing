//! Authenticated host-room power requests. Never accepts peer DATA channel commands.
use crate::session::host_client::HostClient;
use crate::i18n::t;
use anyhow::{Result, ensure};
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
fn parse(push: &farwing_protocol::realtime::Envelope) -> Result<Option<Command>> {
    use farwing_protocol::api::PowerAction;
    if push.kind != farwing_protocol::realtime::types::POWER_REQUEST {
        return Ok(None);
    }
    let request: farwing_protocol::realtime::PowerRequestPush = push.body()?;
    ensure!(
        farwing_protocol::ids::is_uuid(&request.request_id),
        "电源请求标识无效"
    );
    let action = match request.action {
        PowerAction::Reboot => Action::Reboot,
        PowerAction::Shutdown => Action::Shutdown,
        PowerAction::Wake => anyhow::bail!("在线设备不处理唤醒请求"),
    };
    // Every Farwing power request is acknowledged with its result.
    Ok(Some(Command {
        action,
        call_id: Some(request.request_id),
        need_response: true,
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
    pub fn push(&self, push: &farwing_protocol::realtime::Envelope) -> bool {
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
            "{}", t().features.power_request_cancelled
        );
        s.committed = true;
        Ok(())
    }
    pub fn execute(&self, client: &HostClient) -> Result<()> {
        ensure!(
            client.is_active() && client.host.power_permitted(),
            "{}", t().features.power_permission_revoked
        );
        self.claim()?;
        let result = crate::platform::windows::system_power::execute(self.command.action);
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
                self.preparing=None;Event::Ready(result.map_err(|e|anyhow::anyhow!((t().features.power_prepare_interrupted)(&e.to_string()))).and_then(|r|r))
            }
        }
    }
    pub fn prepare(&mut self, request: Request, client: HostClient) {
        if let Some(old) = self.preparing.take() {
            old.abort();
        }
        self.preparing = Some(tokio::spawn(async move {
            let ended = client.ended();
            tokio::select! {biased; _=ended.cancelled()=>anyhow::bail!("{}", t().features.power_signed_out),
                result=tokio::time::timeout(Duration::from_secs(15),prepare(request,&client))=>result.map_err(|_|anyhow::anyhow!(t().features.power_prepare_timeout))?
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
        tokio::task::spawn_blocking(crate::platform::windows::system_power::probe)
            .await
            .map_err(|_| anyhow::anyhow!(t().features.power_privilege_read_failed))
            .and_then(|r| r)
    } else {
        Err(anyhow::anyhow!(t().features.power_not_allowed_here))
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
        "{}", t().features.power_permission_revoked
    );
    Ok(request)
}
