use super::*;
use crate::account::auth::KeyringIdentityStore;
use crate::application::app::{DeviceCenterApp, view};
use crate::media::LocalDisplayInfo;
use crate::session::presence::self_hosted_tests::TestServer;
use std::sync::Mutex;
use std::time::Duration;

struct TestIdentity(KeyringIdentityStore);

impl TestIdentity {
    fn for_origin(origin: &url::Url) -> Result<Self> {
        let settings = self_hosted::SelfHosted::load(origin.clone())?;
        Ok(Self(KeyringIdentityStore::for_self_hosted(
            &settings.device_id,
        )?))
    }
}

impl Drop for TestIdentity {
    fn drop(&mut self) {
        self.0.clear_test_identity();
    }
}

async fn present_until(
    app: &mut DeviceCenterApp,
    context: &egui::Context,
    predicate: impl Fn(&DeviceCenterApp) -> bool,
) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(25), async {
        loop {
            app.drain_events(context);
            if predicate(app) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .context("timed out waiting for the device center")
}

fn renders_text(shape: &egui::Shape, expected: &str) -> bool {
    match shape {
        egui::Shape::Text(text) => text.galley.text().contains(expected),
        egui::Shape::Vec(shapes) => shapes.iter().any(|shape| renders_text(shape, expected)),
        _ => false,
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires the local Windows credential store and Node.js server dependencies"]
async fn self_hosted_connection_reports_failure_and_opens_the_device_list_on_retry() -> Result<()> {
    let (_server, origin) = TestServer::start(&serde_json::json!([])).await?;
    let closed_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let unavailable =
        self_hosted::parse_origin(&format!("http://{}", closed_listener.local_addr()?))?;
    drop(closed_listener);
    let _unavailable_identity = TestIdentity::for_origin(&unavailable)?;
    let _working_identity = TestIdentity::for_origin(&origin)?;
    let initial = DeviceRuntime::start_for(Some(unavailable.clone()))?;

    let (commands, incoming) = mpsc::unbounded_channel();
    let (focus, foreground) = watch::channel(false);
    let (sender, events) = std::sync::mpsc::channel();
    let session_events = sender.clone();
    let saved = Arc::new(Mutex::new(None));
    let record = Arc::clone(&saved);
    let task = tokio::spawn(run_commands(
        incoming,
        sender,
        foreground,
        initial,
        move |origin| {
            *record.lock().unwrap() = origin.cloned();
            Ok(())
        },
    ));
    let worker = GuiWorker {
        commands,
        focus,
        events,
        thread: None,
    };
    let context = egui::Context::default();
    view::configure_visuals(&context);
    let mut app = DeviceCenterApp::new(
        &context,
        LocalDisplayInfo::FALLBACK,
        Default::default(),
        None,
        worker,
    );
    present_until(&mut app, &context, |app| {
        app.server_address == unavailable.as_str()
    })
    .await?;

    app.set_server(unavailable.to_string());
    assert!(app.server_pending);
    present_until(&mut app, &context, |app| !app.server_pending).await?;
    assert!(
        app.server_error
            .as_deref()
            .is_some_and(|error| error.contains("服务器连接失败"))
    );
    assert!(saved.lock().unwrap().is_none());
    let mut frame = context.run_ui(egui::RawInput::default(), |ui| app.draw_center(ui));
    frame.textures_delta.clear();
    assert!(
        frame
            .shapes
            .iter()
            .any(|shape| renders_text(&shape.shape, "服务器连接失败"))
    );

    app.set_server(origin.to_string());
    assert!(app.server_pending);
    present_until(&mut app, &context, |app| !app.server_pending).await?;
    assert_eq!(app.server_address, origin.as_str());
    assert!(app.devices.is_some() && app.catalog.is_some());
    assert!(app.server_error.is_none());
    assert_eq!(*saved.lock().unwrap(), Some(origin));

    let generation = app.login_generation;
    let ended = "自建服务器会话已结束，请重新连接";
    session_events.send(GuiEvent::AccountEnded(generation.wrapping_sub(1), ended.into()))?;
    app.drain_events(&context);
    assert!(app.devices.is_some() && app.catalog.is_some());

    session_events.send(GuiEvent::AccountEnded(generation, ended.into()))?;
    app.drain_events(&context);
    assert!(app.devices.is_none() && app.catalog.is_none());
    let frame = context.run_ui(egui::RawInput::default(), |ui| app.draw_center(ui));
    assert!(
        frame
            .shapes
            .iter()
            .any(|shape| renders_text(&shape.shape, ended))
    );

    drop(app);
    task.await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires an installed Windows resident and a saved, reachable self-hosted server"]
async fn installed_self_hosted_startup_keeps_device_list_online() -> Result<()> {
    anyhow::ensure!(
        crate::platform::windows::host_service::resident::managed(),
        "the Windows resident must be enabled"
    );
    let origin = self_hosted::load_origin()?.context("no self-hosted address is configured")?;
    let context = egui::Context::default();
    view::configure_visuals(&context);
    let mut app = DeviceCenterApp::new(
        &context,
        LocalDisplayInfo::FALLBACK,
        Default::default(),
        None,
        GuiWorker::spawn(),
    );

    present_until(&mut app, &context, |app| {
        app.login_error.is_some()
            || (app.devices.is_some()
                && app.catalog.is_some()
                && matches!(app.presence, PresenceState::Online))
    })
    .await?;
    assert!(app.login_error.is_none(), "{:?}", app.login_error);
    assert_eq!(app.server_address, origin.as_str());

    app.set_server(origin.to_string());
    assert!(app.server_pending);
    present_until(&mut app, &context, |app| !app.server_pending).await?;
    assert!(app.server_error.is_none(), "{:?}", app.server_error);
    present_until(&mut app, &context, |app| {
        app.login_error.is_some() || matches!(app.presence, PresenceState::Online)
    })
    .await?;

    for _ in 0..8 {
        tokio::time::sleep(Duration::from_millis(250)).await;
        app.drain_events(&context);
        assert!(app.login_error.is_none(), "{:?}", app.login_error);
        assert!(app.devices.is_some() && app.catalog.is_some());
        assert!(matches!(app.presence, PresenceState::Online));
    }
    let frame = context.run_ui(egui::RawInput::default(), |ui| app.draw_center(ui));
    assert!(
        frame
            .shapes
            .iter()
            .any(|shape| renders_text(&shape.shape, "我的设备"))
    );
    Ok(())
}
