//! Account login against the configured Farwing Server.
//!
//! Accounts are created by the server administrator. A login registers (or
//! refreshes) this installation as a device on the server, then exchanges the
//! username and password for an account token bound to that device.

use anyhow::{Context, Error, Result, bail};

use crate::account::api::{ApiFailure, ErrorCode, ServerApi};
use crate::account::auth::{KeyringSessionStore, LoginSession, SessionStore};
use crate::account::server::ServerConfig;
use crate::account::session_restore::{self, RestoreTrigger};
use crate::session::device_session::{DeviceHandle, DeviceRuntime};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LoginOutcome {
    Restored,
    New,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LoginProgress {
    ValidatingSavedSession,
    RegisteringDevice,
    DeviceRegistered,
    SubmittingCredentials,
}

/// Username and password for one login attempt. Never logged or persisted.
#[derive(Clone)]
pub struct Credentials {
    pub username: String,
    password: String,
}

impl Credentials {
    pub fn new(username: &str, password: String) -> Result<Self> {
        let username = username.trim();
        if username.is_empty() || username.chars().any(char::is_control) || username.len() > 128 {
            bail!("{}", crate::i18n::t().session.username_invalid);
        }
        if password.is_empty() || password.len() > 1024 {
            bail!("{}", crate::i18n::t().session.password_required);
        }
        Ok(Self {
            username: username.to_owned(),
            password,
        })
    }
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("username", &self.username)
            .field("password", &"***")
            .finish()
    }
}

/// Command-line login: prompts for anything not given on the command line.
pub async fn interactive_login(
    server: Option<String>,
    username: Option<String>,
    password_stdin: bool,
) -> Result<LoginOutcome> {
    if let Some(address) = server {
        let config = ServerConfig::new(&address, None, None)?;
        config.save()?;
        println!("服务器已设置为 {}", config.origin());
    }
    let server = ServerConfig::require()?;
    #[cfg(target_os = "linux")]
    let password_stdin = {
        use std::io::IsTerminal as _;
        let terminal = std::io::stdin().is_terminal();
        anyhow::ensure!(
            terminal || password_stdin,
            "Use --password-stdin with redirected input"
        );
        !terminal
    };
    let username = match username {
        Some(username) => username,
        None => prompt("用户名：")?,
    };
    let password = if password_stdin {
        let mut line = String::new();
        std::io::stdin()
            .read_line(&mut line)
            .context("读取密码失败")?;
        line.trim_end_matches(['\r', '\n']).to_owned()
    } else {
        read_hidden("密码：")?
    };
    let credentials = Credentials::new(&username, password)?;
    println!("正在登录 {} ……", server.origin());
    login_with_progress(credentials, |progress| match progress {
        LoginProgress::ValidatingSavedSession => println!("正在验证保存的登录态……"),
        LoginProgress::RegisteringDevice => println!("正在登记本机设备……"),
        LoginProgress::DeviceRegistered => println!("本机设备已登记，凭据保存在系统凭据存储中。"),
        LoginProgress::SubmittingCredentials => println!("正在验证用户名和密码……"),
    })
    .await
}

fn prompt(label: &str) -> Result<String> {
    use std::io::Write as _;
    print!("{label}");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).context("读取输入失败")?;
    Ok(line.trim().to_owned())
}

/// Read a line without echo on a Windows console; falls back to a plain read
/// when standard input is redirected.
#[cfg(windows)]
fn read_hidden(label: &str) -> Result<String> {
    use std::io::Write as _;
    use windows::Win32::System::Console::{
        CONSOLE_MODE, ENABLE_ECHO_INPUT, GetConsoleMode, GetStdHandle, STD_INPUT_HANDLE,
        SetConsoleMode,
    };
    print!("{label}");
    std::io::stdout().flush()?;
    let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) }.context("无法访问控制台输入")?;
    let mut mode = CONSOLE_MODE::default();
    let console = unsafe { GetConsoleMode(handle, &mut mode) }.is_ok();
    if console {
        let _ = unsafe { SetConsoleMode(handle, mode & !ENABLE_ECHO_INPUT) };
    }
    let mut line = String::new();
    let result = std::io::stdin().read_line(&mut line);
    if console {
        let _ = unsafe { SetConsoleMode(handle, mode) };
        println!();
    }
    result.context("读取密码失败")?;
    Ok(line.trim_end_matches(['\r', '\n']).to_owned())
}

#[cfg(target_os = "linux")]
fn read_hidden(label: &str) -> Result<String> {
    use std::io::{Read as _, Write as _};
    use std::os::fd::AsRawFd as _;

    let fd = std::io::stdin().as_raw_fd();
    let mut original = std::mem::MaybeUninit::<libc::termios>::uninit();
    // tcgetattr initializes the terminal settings only on success.
    if unsafe { libc::tcgetattr(fd, original.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error()).context("Read terminal settings");
    }
    let original = unsafe { original.assume_init() };
    let mut hidden = original;
    // Read the terminal's interrupt/quit/suspend keys ourselves so we can
    // restore echo before returning, including when Ctrl+C is pressed.
    hidden.c_lflag &= !(libc::ECHO | libc::ECHONL | libc::ICANON | libc::ISIG);
    hidden.c_cc[libc::VMIN] = 1;
    hidden.c_cc[libc::VTIME] = 0;
    // Both tcsetattr calls use our open stdin descriptor and initialized settings.
    if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &hidden) } != 0 {
        return Err(std::io::Error::last_os_error()).context("Disable terminal echo");
    }
    let result = (|| -> Result<String> {
        {
            let mut output = std::io::stdout().lock();
            write!(output, "{label}")?;
            output.flush()?;
        }
        let mut input = std::io::stdin().lock();
        let mut password = Vec::new();
        let key = |index, byte| original.c_cc[index] != 0 && original.c_cc[index] == byte;
        loop {
            let mut byte = [0];
            input.read_exact(&mut byte).context("Read password")?;
            let [byte] = byte;
            if matches!(byte, b'\n' | b'\r') {
                break;
            }
            if key(libc::VINTR, byte)
                || key(libc::VQUIT, byte)
                || key(libc::VSUSP, byte)
                || key(libc::VEOF, byte)
            {
                bail!("Password entry cancelled");
            }
            if key(libc::VERASE, byte) {
                let start = password.iter().rposition(|&byte| byte & 0xc0 != 0x80);
                password.truncate(start.unwrap_or(0));
            } else if key(libc::VKILL, byte) {
                password.clear();
            } else {
                password.push(byte);
            }
        }
        String::from_utf8(password).context("Password is not valid UTF-8")
    })();
    if unsafe { libc::tcsetattr(fd, libc::TCSAFLUSH, &original) } != 0 {
        return Err(std::io::Error::last_os_error()).context("Restore terminal settings");
    }
    println!();
    result
}

pub async fn login_with_progress<F>(credentials: Credentials, report: F) -> Result<LoginOutcome>
where
    F: FnMut(LoginProgress),
{
    let device = DeviceRuntime::start()?;
    let result = prepare_login_with_gate(
        device.handle(),
        LoginCommitGate::default(),
        credentials,
        report,
    )
    .await
    .and_then(PreparedLogin::commit);
    device.close().await;
    result
}

pub(crate) enum PreparedLogin {
    Restored,
    ConditionalNew {
        session: LoginSession,
        expected: Option<LoginSession>,
        permit: tokio::sync::OwnedMutexGuard<bool>,
    },
}

/// Only credential exchange/commit is serialized, so two near-simultaneous
/// logins cannot replace one another.
#[derive(Clone, Default)]
pub(crate) struct LoginCommitGate(std::sync::Arc<tokio::sync::Mutex<bool>>);

impl LoginCommitGate {
    pub(crate) async fn enter(&self) -> Result<tokio::sync::OwnedMutexGuard<bool>> {
        let permit = self.0.clone().lock_owned().await;
        if *permit {
            bail!("{}", crate::i18n::t().session.login_already_done);
        }
        Ok(permit)
    }
}

impl PreparedLogin {
    pub(crate) fn commit(self) -> Result<LoginOutcome> {
        match self {
            Self::Restored => Ok(LoginOutcome::Restored),
            Self::ConditionalNew {
                session,
                expected,
                mut permit,
            } => {
                if !KeyringSessionStore::new()?.save_if_matches(expected.as_ref(), &session)? {
                    bail!("{}", crate::i18n::t().session.login_changed_elsewhere);
                }
                *permit = true;
                Ok(LoginOutcome::New)
            }
        }
    }
}

pub(crate) async fn prepare_login_with_gate<F>(
    device: DeviceHandle,
    gate: LoginCommitGate,
    credentials: Credentials,
    mut report: F,
) -> Result<PreparedLogin>
where
    F: FnMut(LoginProgress),
{
    let server = ServerConfig::require()?;
    let session_store = KeyringSessionStore::new()?;
    report(LoginProgress::RegisteringDevice);
    let identity = device.ensure(true).await?;
    let mut api = ServerApi::new(&server)?;
    api.set_identity(identity.client_identity()?);
    report(LoginProgress::DeviceRegistered);

    if let Some(session) = session_store.load()?
        && session.server() == server.key()
        && session.username() == credentials.username
    {
        report(LoginProgress::ValidatingSavedSession);
        let mut saved = api.clone();
        saved.set_access_token(Some(session.token()))?;
        match session_restore::restore_user(&saved, RestoreTrigger::ExplicitLogin).await {
            Ok(_) => return Ok(PreparedLogin::Restored),
            Err(failure) if failure.invalid_saved_credentials() => {
                session_store.clear_if_matches(&session)?;
            }
            Err(error) => {
                return Err(Error::new(error))
                    .context(crate::i18n::t().session.saved_login_unverified);
            }
        }
    }

    let permit = gate.enter().await?;
    // Registration may have rotated after a server-side device removal.
    api.set_identity(device.ensure(false).await?.client_identity()?);
    let expected = session_store.load()?;
    report(LoginProgress::SubmittingCredentials);
    let login = api
        .login(&credentials.username, &credentials.password)
        .await
        .map_err(login_error)?;
    let session = LoginSession::new(
        server.key(),
        login.access_token,
        login.user_id,
        if login.username.is_empty() {
            credentials.username.clone()
        } else {
            login.username
        },
        login.nickname,
    )?;
    Ok(PreparedLogin::ConditionalNew {
        session,
        expected,
        permit,
    })
}

fn login_error(error: Error) -> Error {
    match error.downcast_ref::<ApiFailure>().map(|failure| failure.code) {
        Some(ErrorCode::Unauthorized) => anyhow::anyhow!("{}", crate::i18n::t().session.login_invalid),
        Some(ErrorCode::RateLimited) => anyhow::anyhow!("{}", crate::i18n::t().session.login_too_frequent),
        Some(ErrorCode::Network) => error.context(crate::i18n::t().session.cannot_reach_server),
        _ => error,
    }
}

/// User-facing message for a failed login (no credentials echoed).
pub(crate) fn error_message(error: &Error) -> String {
    if let Some(failure) = error.downcast_ref::<ApiFailure>() {
        return format!("{failure}");
    }
    let text = format!("{error:#}");
    text.chars().filter(|c| !c.is_control()).take(240).collect()
}
