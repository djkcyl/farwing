//! 主程序这一侧的客户端：连上辅助程序，要一块虚拟屏，走的时候把连接关掉。
//!
//! 连接本身就是租约：辅助程序把这条连接要到的连接器记在自己名下，读到 EOF 就全部
//! 断开。所以主程序不需要（也没法）做额外的清理工作，进程死掉也不会留下悬空的虚拟屏。
use crate::protocol::{Reply, Request, Status};
use anyhow::{Context, Result};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

/// 系统级套接字。systemd 的 `openuuyc-vkms.socket` 直接监听这个路径。
pub const SOCKET: &str = "/run/openuuyc-vkms/control.sock";
/// 发行版把辅助程序装在这里。
pub const HELPER: &str = "/usr/bin/openuuyc-vkms";
/// 套接字单元：有它就说明这套东西装上了，能不能用还要真的连一次（见 [`probe`]）。
/// 两个路径都认：Debian 系的单元在 /lib/systemd/system，Arch 与新版 Debian 在
/// /usr/lib/systemd/system。
pub const HELPER_SOCKET_UNITS: [&str; 2] = [
    "/usr/lib/systemd/system/openuuyc-vkms.socket",
    "/lib/systemd/system/openuuyc-vkms.socket",
];

/// 单次往返的等待上限。辅助程序建屏时要等内核把连接器报出来，但也就几百毫秒。
const TIMEOUT: Duration = Duration::from_secs(10);

/// 这条连接已经不能用了：辅助程序没在跑、它空闲超时把我们踢了、或者它重启了。
///
/// 单独一个类型是为了让主程序能把它和「辅助程序拒绝了这次请求」分开：前者丢掉
/// 缓存的连接重来一次就好，后者重试多少次都是一样的结果。
#[derive(Debug)]
pub struct Disconnected(String);

impl std::fmt::Display for Disconnected {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}
impl std::error::Error for Disconnected {}

/// 辅助程序装没装。装没装看文件，能不能用要真的连一次（见 [`probe`]）。
pub fn installed() -> bool {
    std::path::Path::new(HELPER).exists()
        || HELPER_SOCKET_UNITS
            .iter()
            .any(|unit| std::path::Path::new(unit).exists())
}

/// 问一次现在的状态。诊断页用这个判断「可用」。
pub fn probe() -> Result<Status> {
    let mut client = Client::connect()?;
    let status = client.status()?;
    Ok(status)
}

pub struct Client {
    writer: UnixStream,
    reader: BufReader<UnixStream>,
}

impl Client {
    pub fn connect() -> Result<Self> {
        Self::connect_to(SOCKET)
    }

    pub fn connect_to(path: &str) -> Result<Self> {
        let stream = UnixStream::connect(path).map_err(|error| {
            Disconnected(format!(
                "无法连接虚拟显示器辅助程序（{path}）：{error}；它可能没有安装或没有运行"
            ))
        })?;
        stream.set_read_timeout(Some(TIMEOUT))?;
        stream.set_write_timeout(Some(TIMEOUT))?;
        Ok(Self {
            reader: BufReader::new(stream.try_clone()?),
            writer: stream,
        })
    }

    pub fn create(&mut self, width: u32, height: u32, hz: u32) -> Result<String> {
        let reply = self.call(&Request::Create { width, height, hz })?;
        reply.connector.context("辅助程序没有报出连接器名")
    }

    pub fn destroy(&mut self, connector: &str) -> Result<()> {
        self.call(&Request::Destroy {
            connector: connector.to_owned(),
        })?;
        Ok(())
    }

    pub fn status(&mut self) -> Result<Status> {
        let reply = self.call(&Request::Status)?;
        reply.status.context("辅助程序没有报出状态")
    }

    fn call(&mut self, request: &Request) -> Result<Reply> {
        let mut line = serde_json::to_vec(request)?;
        line.push(b'\n');
        if let Err(error) = self
            .writer
            .write_all(&line)
            .and_then(|()| self.writer.flush())
        {
            return Err(self.parting_words().unwrap_or_else(|| send_failed(error)));
        }
        let mut answer = String::new();
        let read = match self.reader.read_line(&mut answer) {
            // 空闲的连接会被辅助程序收掉（见 openuuyc-vkms 的 IDLE_TIMEOUT：没租屏的
            // 连接两条请求之间最多隔 10 秒），所以读失败也要算成「这条连接没了」，
            // 而不是一次业务失败。
            Ok(read) => read,
            Err(error) => {
                return Err(Disconnected(format!("读取辅助程序的应答失败：{error}")).into());
            }
        };
        if read == 0 {
            return Err(Disconnected("辅助程序没有应答就断开了连接".into()).into());
        }
        let reply: Reply = serde_json::from_str(&answer)
            .with_context(|| format!("辅助程序的应答无法解析：{}", answer.trim()))?;
        reply.into_result()
    }
}

impl Client {
    /// 请求写不进去时，看看辅助程序关连接之前有没有留下一句话。
    ///
    /// 它回绝一条连接（连接位满了、或者不认这个用户）时是先写一行原因、再关连接的；
    /// 我们的请求晚到一步就会写失败，而那行原因还在接收缓冲里。不读出来的话，用户只能
    /// 看到一句莫名其妙的「连接被重置」。
    fn parting_words(&mut self) -> Option<anyhow::Error> {
        // 对端已经关了的话这一读立刻返回；没关（比如写超时）就别再等满一整个 TIMEOUT。
        self.reader
            .get_ref()
            .set_read_timeout(Some(Duration::from_secs(1)))
            .ok()?;
        let mut answer = String::new();
        self.reader.read_line(&mut answer).ok()?;
        let reply: Reply = serde_json::from_str(answer.trim()).ok()?;
        reply.into_result().err()
    }
}

fn send_failed(error: std::io::Error) -> anyhow::Error {
    Disconnected(format!("向辅助程序发送请求失败：{error}")).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refusal_becomes_an_error_with_the_helpers_own_words() {
        let reply = Reply::failed("没有空闲的虚拟显示器");
        let error = reply.into_result().unwrap_err();
        assert!(error.to_string().contains("没有空闲的虚拟显示器"));
        assert!(
            error
                .downcast_ref::<crate::protocol::Unsupported>()
                .is_none()
        );
    }

    /// 「这台机器做不了」要能和普通拒绝区分开，诊断页才说得出「内核不支持」。
    #[test]
    fn an_unsupported_reply_is_marked_as_such() {
        let reply = Reply::unsupported("本机内核 6.8 的 VKMS 不支持 configfs");
        let error = reply.into_result().unwrap_err();
        assert!(
            error
                .downcast_ref::<crate::protocol::Unsupported>()
                .is_some()
        );
        assert!(error.to_string().contains("不支持 configfs"));
    }

    #[test]
    fn a_successful_reply_carries_through() {
        let reply = Reply::connector("Virtual-5".into());
        let reply = reply.into_result().unwrap();
        assert_eq!(reply.connector.as_deref(), Some("Virtual-5"));
    }

    /// 辅助程序先回一句原因再关连接，而我们的请求晚到写不进去：用户该看到那句原因，
    /// 而不是「连接被重置」。
    #[test]
    fn a_refusal_sent_before_the_request_still_reaches_the_user() {
        let directory =
            std::env::temp_dir().join(format!("openuuyc-vkms-test-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("refuse.sock");
        let _ = std::fs::remove_file(&path);
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut line = serde_json::to_vec(&Reply::failed("虚拟显示器辅助程序忙")).unwrap();
            line.push(b'\n');
            (&stream).write_all(&line).unwrap();
            // 不读请求就整个关掉：之后的 write 必然失败。
            drop(stream);
        });
        let mut client = Client::connect_to(path.to_str().unwrap()).unwrap();
        server.join().unwrap();
        let error = client.status().unwrap_err();
        assert!(error.to_string().contains("忙"), "{error:#}");
        assert!(error.downcast_ref::<Disconnected>().is_none());
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn requests_round_trip_through_the_wire_format() {
        for request in [
            Request::Create {
                width: 2560,
                height: 1440,
                hz: 144,
            },
            Request::Destroy {
                connector: "Virtual-5".into(),
            },
            Request::Status,
        ] {
            let text = serde_json::to_string(&request).unwrap();
            assert!(!text.contains('\n'), "一行一条：{text}");
            assert_eq!(serde_json::from_str::<Request>(&text).unwrap(), request);
        }
    }
}
