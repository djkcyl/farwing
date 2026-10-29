//! `identity-check`: what the background host's work can do under the
//! identity it runs as. The resident runs it as LocalService when the file
//! `identity-check` is in its vault directory and logs the outcome; each
//! line is one check.
use anyhow::Result;

fn line(name: &str, result: Result<String>) {
    match result {
        Ok(detail) => println!("ok {name}: {detail}"),
        Err(error) => println!("failed {name}: {error:#}"),
    }
}

fn open_device(path: &str) -> Result<String> {
    use windows::Win32::Storage::FileSystem::*;
    let wide: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
    let handle = unsafe {
        CreateFileW(
            windows::core::PCWSTR(wide.as_ptr()),
            (windows::Win32::Foundation::GENERIC_READ | windows::Win32::Foundation::GENERIC_WRITE).0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )?
    };
    drop(super::pipe::Handle(handle));
    Ok("opened".into())
}

fn loopback() -> Result<String> {
    use windows::Win32::{Media::Audio::*, System::Com::*};
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let devices: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let endpoint = devices.GetDefaultAudioEndpoint(eRender, eConsole)?;
        let client: IAudioClient = endpoint.Activate(CLSCTX_ALL, None)?;
        let format = client.GetMixFormat()?;
        let result = client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_LOOPBACK,
            100_000,
            0,
            format,
            None,
        );
        CoTaskMemFree(Some(format.cast()));
        result?;
        Ok("loopback client initialized".into())
    }
}

fn window() -> Result<String> {
    use windows::Win32::UI::WindowsAndMessaging::*;
    let window = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            windows::core::w!("STATIC"),
            windows::core::w!(""),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            None,
            None,
        )?
    };
    unsafe {
        let _ = DestroyWindow(window);
    }
    Ok("message window created".into())
}

fn display_config() -> Result<String> {
    use windows::Win32::Devices::Display::*;
    let topology = crate::platform::windows::display::topology::Topology::query(true)?;
    let targets = topology.targets()?.len();
    let status = unsafe { SetDisplayConfig(None, None, SDC_VALIDATE | SDC_USE_DATABASE_CURRENT) };
    anyhow::ensure!(status == 0, "SetDisplayConfig validate returned {status}");
    Ok(format!("{targets} targets; changes validate"))
}

fn capture_and_encode() -> Result<String> {
    use anyhow::Context as _;
    let pipe = super::pipe::Pipe::client(&crate::product::pipe_name("Capture.v1"))
        .context("capture pipe")?
        .context("no capture pipe")?;
    super::install::verify_running(pipe.peer_pid(false)?).context("capture service identity")?;
    drop(pipe);
    let screens = crate::platform::windows::capture::screens().context("screens")?;
    let screen = screens
        .iter()
        .find(|s| s.primary)
        .or(screens.first())
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("no screen"))?;
    let _runtime = crate::platform::encoder::Runtime::with_gpu_compatibility(false).context("encoder runtime")?;
    let mut desktop = crate::platform::windows::capture::Desktop::open_selected(&screen).context("open capture")?;
    let backend = desktop.backend_name();
    let codecs = crate::platform::encoder::probe(&mut desktop, &|| true).context("probe encoders")?;
    Ok(format!("{backend} capture of {}; {} encoder capabilities", screen.device_name, codecs.len()))
}

fn groups() -> Result<String> {
    let output = std::process::Command::new("whoami").arg("/groups").arg("/fo").arg("csv").arg("/nh").output()?;
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.split("\",\"").nth(2).map(str::to_owned))
        .collect::<Vec<_>>()
        .join(" "))
}

fn scm() -> Result<String> {
    use anyhow::Context as _;
    use windows::Win32::System::Services::*;
    let name: Vec<u16> = crate::product::SERVICE_NAME.encode_utf16().chain(Some(0)).collect();
    unsafe {
        let manager = OpenSCManagerW(None, None, SC_MANAGER_CONNECT).context("SCM connect")?;
        let status = OpenServiceW(manager, windows::core::PCWSTR(name.as_ptr()), SERVICE_QUERY_STATUS).context("service status");
        let config = OpenServiceW(manager, windows::core::PCWSTR(name.as_ptr()), SERVICE_QUERY_CONFIG).context("service config");
        let result = format!("status {}; config {}", status.as_ref().map_or_else(|e| format!("{e:#}"), |_| "ok".into()), config.as_ref().map_or_else(|e| format!("{e:#}"), |_| "ok".into()));
        if let Ok(h) = status { let _ = CloseServiceHandle(h); }
        if let Ok(h) = config { let _ = CloseServiceHandle(h); }
        let _ = CloseServiceHandle(manager);
        Ok(result)
    }
}

pub(crate) fn run() -> Result<()> {
    line("identity", super::vault::sid(std::process::id()));
    line("groups", groups());
    line(
        "install directory",
        super::install::directory().and_then(|d| Ok(format!("{} bytes of images.json", std::fs::read(d.join("images.json"))?.len()))),
    );
    line("service", scm());
    line("session", super::process::session(std::process::id()).map(|s| s.to_string()));
    line("window", window());
    line(
        "tcp",
        std::net::TcpStream::connect_timeout(
            &"127.0.0.1:18743".parse().expect("an address"),
            std::time::Duration::from_secs(3),
        )
        .map(|_| "connected to 127.0.0.1:18743".into())
        .map_err(Into::into),
    );
    line(
        "udp",
        std::net::UdpSocket::bind("0.0.0.0:0").map(|s| format!("bound {:?}", s.local_addr())).map_err(Into::into),
    );
    line("vault", super::vault::root().and_then(|root| Ok(format!("{} entries", std::fs::read_dir(root)?.count()))));
    line("data dir", crate::product::local_data_dir().map(|d| d.display().to_string()));
    line(
        "display driver",
        crate::platform::windows::display::virtual_driver::Driver::open().map(|_| "opened".into()),
    );
    line("audio bridge", open_device(r"\\.\FarwingAudioBridge"));
    line("input device", open_device(r"\\.\FarwingInput"));
    line("display config", display_config());
    line("loopback", loopback());
    line("capture and encode", capture_and_encode());
    Ok(())
}
