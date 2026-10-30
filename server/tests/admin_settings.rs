//! First start, recovery, administration tokens and settings applied while
//! the server runs (`docs/server-admin-api.md`).

mod common;

use common::*;
use reqwest::Method;
use serde_json::{Value, json};

fn free_udp_port() -> u16 {
    std::net::UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

#[tokio::test(flavor = "multi_thread")]
async fn setup_opens_the_server_once() {
    let srv = start_unset().await;
    let code = srv.server.as_ref().unwrap().setup_code.clone().unwrap();

    // Only the setup and the server description answer.
    srv.send(srv.raw(Method::GET, "/api/v2/server")).await.ok();
    srv.send(srv.raw(Method::GET, "/api/v2/web/info"))
        .await
        .err(503, "setup_required");
    let info = srv.send(srv.raw(Method::GET, "/api/v2/setup")).await;
    assert_eq!(info.ok()["state"], "setup");
    assert_eq!(info.ok()["current"]["settings"]["turn"]["shared_secret"], "");

    let request = |code: &str, password: &str| {
        json!({
            "code": code,
            "settings": {"instance_name": "Lab", "registration": true},
            "admin": {"username": "boss", "password": password},
        })
    };
    srv.send(srv.raw(Method::POST, "/api/v2/setup").json(&request("WRONG-CODE", PASSWORD)))
        .await
        .err_reason(403, "forbidden", "setup_code_invalid");
    srv.send(srv.raw(Method::POST, "/api/v2/setup").json(&request(&code, "short")))
        .await
        .err_reason(400, "bad_request", "password_too_short");
    let mut bad = request(&code, PASSWORD);
    bad["settings"]["egress"] = json!({"limit_mbps": 0});
    let r = srv.send(srv.raw(Method::POST, "/api/v2/setup").json(&bad)).await;
    r.err_reason(400, "bad_request", "setting_invalid");
    assert_eq!(r.body["error"]["details"]["field"], "egress.limit_mbps");

    let mut last = request(&code, PASSWORD);
    last["token"] = json!({"name": "provision", "scope": "admin"});
    let done = srv.send(srv.raw(Method::POST, "/api/v2/setup").json(&last)).await;
    assert_eq!(done.ok()["settings"]["instance_name"], "Lab");
    // The token works without a browser.
    let token = done.ok()["token"].as_str().unwrap().to_owned();
    srv.admin_call(Method::GET, "/api/v2/admin/settings", &token, None)
        .await
        .ok();
    assert_eq!(done.ok()["setup"], false);
    let info = srv.send(srv.raw(Method::GET, "/api/v2/setup")).await;
    assert_eq!(info.ok(), &json!({"state": "done"}));
    srv.send(srv.raw(Method::POST, "/api/v2/setup").json(&request(&code, PASSWORD)))
        .await
        .err_reason(409, "conflict", "setup_done");

    // The administrator signs in; the settings are stored.
    let mut boss = srv.init_device("Console", "web", false).await;
    srv.login(&mut boss, "boss").await;
    let view = srv.get("/api/v2/admin/settings", &boss).await;
    assert_eq!(view.ok()["settings"]["registration"], true);
    let stored = farwing_server::db::open_connection(&srv.cfg.db_path()).unwrap();
    let loaded = farwing_server::settings::load(&stored, &srv.cfg.data_dir, None).unwrap();
    assert_eq!(loaded.instance_name, "Lab");
    assert!(!farwing_server::setup::code_pending(&stored).unwrap());
    srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_setup_code_recovers_an_administrator() {
    let srv = start().await;
    let a = srv.account_device(ADMIN, "A", false).await;
    let recover = |code: &str| {
        json!({"code": code, "username": ADMIN, "password": "a-new-password"})
    };
    // No code issued: nothing matches.
    srv.send(srv.raw(Method::POST, "/api/v2/setup/recover").json(&recover("ANY-CODE")))
        .await
        .err_reason(403, "forbidden", "setup_code_invalid");
    let code = {
        let c = farwing_server::db::open_connection(&srv.cfg.db_path()).unwrap();
        farwing_server::setup::new_code(&c).unwrap()
    };
    let info = srv.send(srv.raw(Method::GET, "/api/v2/setup")).await;
    assert_eq!(info.ok()["state"], "recovery");
    srv.send(srv.raw(Method::POST, "/api/v2/setup/recover").json(&recover(&code)))
        .await
        .ok();
    srv.get("/api/v2/me", &a).await.err(401, "session_revoked");
    let mut b = srv.init_device("B", "windows", false).await;
    srv.post(
        "/api/v2/auth/login",
        &b,
        json!({"username": ADMIN, "password": "a-new-password"}),
    )
    .await
    .ok();
    // The code works once.
    srv.send(srv.raw(Method::POST, "/api/v2/setup/recover").json(&recover(&code)))
        .await
        .err_reason(403, "forbidden", "setup_code_invalid");
    // A missing account is created as an administrator.
    let code = {
        let c = farwing_server::db::open_connection(&srv.cfg.db_path()).unwrap();
        farwing_server::setup::new_code(&c).unwrap()
    };
    srv.send(srv.raw(Method::POST, "/api/v2/setup/recover").json(&json!({
        "code": code, "username": "helper", "password": PASSWORD,
    })))
    .await
    .ok();
    srv.login(&mut b, "helper").await;
    srv.get("/api/v2/admin/overview", &b).await.ok();
    srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn administration_tokens() {
    let srv = start().await;
    let root = srv.account_device(ADMIN, "Console", false).await;

    let r = srv
        .post(
            "/api/v2/admin/tokens",
            &root,
            json!({"name": "monitor", "scope": "read", "expires_days": 30}),
        )
        .await;
    let read = r.ok()["token"].as_str().unwrap().to_owned();
    assert!(read.starts_with("fwa_"));
    let r = srv
        .post(
            "/api/v2/admin/tokens",
            &root,
            json!({"name": "deploy", "scope": "admin", "allow": ["127.0.0.0/8"]}),
        )
        .await;
    let admin = r.ok()["token"].as_str().unwrap().to_owned();
    let admin_id = r.ok()["id"].as_str().unwrap().to_owned();
    let r = srv
        .post(
            "/api/v2/admin/tokens",
            &root,
            json!({"name": "far", "scope": "admin", "allow": ["10.0.0.0/8"]}),
        )
        .await;
    let far = r.ok()["token"].as_str().unwrap().to_owned();
    srv.post("/api/v2/admin/tokens", &root, json!({"name": "x", "scope": "owner"}))
        .await
        .err_reason(400, "bad_request", "setting_invalid");

    let list = srv.get("/api/v2/admin/tokens", &root).await;
    assert_eq!(list.ok().as_array().unwrap().len(), 3);
    assert!(!list.body.to_string().contains(&read), "tokens are shown once");

    let patch = Some(json!({"instance_name": "Scripted"}));
    srv.admin_call(Method::GET, "/api/v2/admin/settings", &read, None)
        .await
        .ok();
    srv.admin_call(Method::PUT, "/api/v2/admin/settings", &read, patch.clone())
        .await
        .err(403, "forbidden");
    srv.admin_call(Method::PUT, "/api/v2/admin/settings", &admin, patch.clone())
        .await
        .ok();
    srv.admin_call(Method::GET, "/api/v2/admin/settings", &far, None)
        .await
        .err_reason(403, "forbidden", "admin_network_denied");
    // Tokens do not manage tokens.
    srv.admin_call(
        Method::POST,
        "/api/v2/admin/tokens",
        &admin,
        Some(json!({"name": "more", "scope": "admin"})),
    )
    .await
    .err_reason(403, "forbidden", "admin_login_required");
    // Nor reach the account API.
    srv.admin_call(Method::GET, "/api/v2/me", &admin, None)
        .await
        .err(401, "device_unauthorized");

    let audit = srv.get("/api/v2/admin/audit?kind=settings.update", &root).await;
    assert!(audit.ok()[0]["detail"].as_str().unwrap().contains("deploy"));
    let list = srv.get("/api/v2/admin/tokens", &root).await;
    let deploy = list
        .ok()
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == admin_id.as_str())
        .unwrap()
        .clone();
    assert_eq!(deploy["last_used_ip"], "127.0.0.1");

    srv.delete(&format!("/api/v2/admin/tokens/{admin_id}"), &root)
        .await
        .ok();
    srv.admin_call(Method::GET, "/api/v2/admin/settings", &admin, None)
        .await
        .err(401, "session_revoked");
    srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_builtin_relay_starts_and_stops_with_the_settings() {
    let srv = start().await;
    let token = srv.admin_token("admin");
    let put = |body: Value| srv.admin_call(Method::PUT, "/api/v2/admin/settings", &token, Some(body));
    let r = put(json!({"turn": {
        "mode": "builtin",
        "urls": [],
        "listen_udp": "127.0.0.1:0",
        "listen_tcp": null,
        "listen_tls": "127.0.0.1:0",
        "public_ip": "127.0.0.1",
        "relay_bind": "127.0.0.1",
    }}))
    .await;
    assert_eq!(r.ok()["outcome"]["relay_restarted"], true);
    let udp = r.ok()["relay"]["turn_udp"].as_str().unwrap().to_owned();
    let (_sid, incoming, _ack) = srv.ice_for_session().await;
    let urls = incoming["ice_servers"].to_string();
    assert!(urls.contains(&format!("turn:{udp}?transport=udp")), "{urls}");

    // A port in use is refused and nothing changes.
    let busy = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let r = put(json!({"turn": {"listen_udp": busy.local_addr().unwrap().to_string()}})).await;
    r.err_reason(400, "bad_request", "setting_invalid");
    assert_eq!(r.body["error"]["details"]["problem"], "unavailable");
    let view = srv.admin_call(Method::GET, "/api/v2/admin/settings", &token, None).await;
    assert_eq!(view.ok()["relay"]["turn_udp"], udp.as_str());

    // Fixed ports are bound again on every restart.
    let fixed = |port: u16| format!("127.0.0.1:{port}");
    let (udp_port, tls_port, stun_port) = (free_udp_port(), free_port(), free_udp_port());
    put(json!({"turn": {
        "listen_udp": fixed(udp_port),
        "listen_tls": fixed(tls_port),
        "stun": [{"listen": fixed(stun_port), "public": null}],
    }}))
    .await
    .ok();
    // A new secret restarts the relay.
    for _ in 0..2 {
        let r = srv
            .admin_call(Method::POST, "/api/v2/admin/settings/relay-secret", &token, None)
            .await;
        assert_eq!(r.ok()["outcome"]["relay_restarted"], true);
        assert_eq!(r.ok()["relay"]["turn_udp"], fixed(udp_port));
        assert_eq!(r.ok()["relay"]["turn_tls"], fixed(tls_port));
        assert_eq!(r.ok()["relay"]["stun"], json!([fixed(stun_port)]));
    }
    assert_ne!(srv.app().cfg().turn.shared_secret, "test-shared-secret");

    let r = put(json!({"turn": {"mode": "external", "urls": ["turn:turn.test:3478"]}})).await;
    assert_eq!(r.ok()["relay"]["turn_udp"], Value::Null);
    srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_listen_change_is_confirmed_or_reverted() {
    let mut srv = start().await;
    let token = srv.admin_token("admin");
    let at = |port: u16, method: Method, path: &str| {
        srv.http
            .request(method, format!("https://127.0.0.1:{port}{path}"))
            .header("x-farwing-protocol", common::V.to_string())
            .header("authorization", format!("Bearer {token}"))
    };
    let first = srv.port;

    // Saved with other settings: refused.
    let second = free_port();
    srv.admin_call(
        Method::PUT,
        "/api/v2/admin/settings",
        &token,
        Some(json!({"listen": format!("127.0.0.1:{second}"), "instance_name": "x"})),
    )
    .await
    .err_reason(400, "bad_request", "setting_invalid");

    let r = srv
        .admin_call(
            Method::PUT,
            "/api/v2/admin/settings",
            &token,
            Some(json!({"listen": format!("127.0.0.1:{second}")})),
        )
        .await;
    assert_eq!(
        r.ok()["outcome"]["listen_url"],
        format!("https://127.0.0.1:{second}")
    );
    assert_eq!(r.ok()["listen_pending"]["url"], format!("https://127.0.0.1:{second}"));
    // Both answer while it waits.
    srv.send(at(first, Method::GET, "/api/v2/server")).await.ok();
    srv.send(at(second, Method::GET, "/api/v2/server")).await.ok();
    srv.send(at(second, Method::POST, "/api/v2/admin/settings/listen/confirm"))
        .await
        .ok();
    assert!(srv.http.get(format!("https://127.0.0.1:{first}/")).send().await.is_err());
    srv.send(at(second, Method::POST, "/api/v2/admin/settings/listen/confirm"))
        .await
        .err_reason(409, "conflict", "nothing_to_confirm");
    let stored = farwing_server::db::open_connection(&srv.cfg.db_path()).unwrap();
    let loaded = farwing_server::settings::load(&stored, &srv.cfg.data_dir, None).unwrap();
    assert_eq!(loaded.listen.port(), second);

    // Reverted: the confirmed address stays.
    let third = free_port();
    srv.send(
        at(second, Method::PUT, "/api/v2/admin/settings")
            .json(&json!({"listen": format!("127.0.0.1:{third}")})),
    )
    .await
    .ok();
    srv.send(at(third, Method::GET, "/api/v2/server")).await.ok();
    srv.send(at(third, Method::POST, "/api/v2/admin/settings/listen/revert"))
        .await
        .ok();
    assert!(srv.http.get(format!("https://127.0.0.1:{third}/")).send().await.is_err());
    let view = srv
        .send(at(second, Method::GET, "/api/v2/admin/settings"))
        .await;
    assert_eq!(view.ok()["settings"]["listen"], format!("127.0.0.1:{second}"));
    assert_eq!(view.ok()["listen_pending"], Value::Null);
    let loaded = farwing_server::settings::load(&stored, &srv.cfg.data_dir, None).unwrap();
    assert_eq!(loaded.listen.port(), second);

    srv.port = second;
    srv.base = format!("https://127.0.0.1:{second}");
    srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn certificate_upload_export_and_import() {
    let srv = start().await;
    let token = srv.admin_token("admin");
    let pair = rcgen::generate_simple_self_signed(vec!["127.0.0.1".into(), "localhost".into()]).unwrap();
    let cert_pem = pair.cert.pem();
    let fingerprint = farwing_server::tls::fingerprint(pair.cert.der());

    let wrong = rcgen::generate_simple_self_signed(vec!["x.test".into()]).unwrap();
    let r = srv
        .admin_call(
            Method::PUT,
            "/api/v2/admin/settings/certificate",
            &token,
            Some(json!({"cert_pem": cert_pem, "key_pem": wrong.signing_key.serialize_pem()})),
        )
        .await;
    r.err_reason(400, "bad_request", "setting_invalid");
    assert_eq!(r.body["error"]["details"]["field"], "tls.certificate");

    let r = srv
        .admin_call(
            Method::PUT,
            "/api/v2/admin/settings/certificate",
            &token,
            Some(json!({"cert_pem": cert_pem, "key_pem": pair.signing_key.serialize_pem()})),
        )
        .await;
    assert_eq!(r.ok()["outcome"]["fingerprint_changed"], true);
    assert_eq!(r.ok()["tls"]["fingerprint"], fingerprint.as_str());
    assert_eq!(r.ok()["tls"]["provided"], true);
    assert_eq!(r.ok()["settings"]["tls"]["mode"], "provided");
    // New connections get the uploaded certificate.
    let mut roots = rustls::RootCertStore::empty();
    roots.add(pair.cert.der().clone()).unwrap();
    let tls = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    let fresh = reqwest::Client::builder()
        .use_preconfigured_tls(tls)
        .build()
        .unwrap();
    let r = fresh
        .get(srv.url("/api/v2/server"))
        .header("x-farwing-protocol", common::V.to_string())
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success());

    // Export without secrets, import with a change: the secret stays.
    let doc = srv
        .admin_call(Method::GET, "/api/v2/admin/settings/export", &token, None)
        .await;
    let mut doc = doc.ok().clone();
    assert_eq!(doc["turn"]["shared_secret"], "");
    let full = srv
        .admin_call(Method::GET, "/api/v2/admin/settings/export?secrets=true", &token, None)
        .await;
    assert_eq!(full.ok()["turn"]["shared_secret"], "test-shared-secret");
    doc["instance_name"] = json!("Imported");
    let r = fresh
        .post(srv.url("/api/v2/admin/settings/import"))
        .header("x-farwing-protocol", common::V.to_string())
        .header("authorization", format!("Bearer {token}"))
        .json(&doc)
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success());
    assert_eq!(srv.app().cfg().instance_name, "Imported");
    assert_eq!(srv.app().cfg().turn.shared_secret, "test-shared-secret");
    doc["unknown_key"] = json!(1);
    let r = fresh
        .post(srv.url("/api/v2/admin/settings/import"))
        .header("x-farwing-protocol", common::V.to_string())
        .header("authorization", format!("Bearer {token}"))
        .json(&doc)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    srv.shutdown().await;
}
