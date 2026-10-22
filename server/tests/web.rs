//! The web console's server side (`docs/web-console.md`): roles and the
//! administrator guard, two-step verification, invitations, account
//! administration, runtime settings, update announcements, the audit log,
//! browser logins and the browser's realtime admission.

mod common;

use common::*;
use reqwest::Method;
use serde_json::{Value, json};
use std::sync::Arc;

fn promote(srv: &TestServer, username: &str) {
    srv.promote(username);
}

/// A browser (platform `web`) signed in as `username`.
async fn browser(srv: &TestServer, username: &str) -> Dev {
    let mut d = srv.init_device("网页 · Chrome · Windows", "web", false).await;
    srv.login(&mut d, username).await;
    d
}

fn now_s() -> i64 {
    farwing_server::db::now_s()
}

fn totp_code(secret: &str) -> String {
    farwing_server::totp::code(secret, now_s()).unwrap()
}

#[tokio::test]
async fn admin_endpoints_need_the_admin_role() {
    let srv = start().await;
    srv.create_user("root");
    srv.create_user("alice");
    promote(&srv, "root");
    let root = browser(&srv, "root").await;
    let alice = browser(&srv, "alice").await;

    srv.get("/api/v2/admin/overview", &alice).await.err(403, "forbidden");
    srv.get("/api/v2/admin/users", &alice).await.err(403, "forbidden");
    let anonymous = srv.init_device("anon", "web", false).await;
    srv.get("/api/v2/admin/overview", &anonymous).await.err(401, "unauthorized");

    let o = srv.get("/api/v2/admin/overview", &root).await;
    let o = o.ok();
    assert_eq!(o["accounts"], 2);
    assert_eq!(o["admins"], 1);
    assert!(o["version"].as_str().is_some());

    let me = srv.get("/api/v2/me/security", &root).await;
    assert_eq!(me.ok()["role"], "admin");
    let me = srv.get("/api/v2/me/security", &alice).await;
    assert_eq!(me.ok()["role"], "user");
    srv.shutdown().await;
}

#[tokio::test]
async fn admin_allow_list_limits_the_console_by_address() {
    let srv = start_with(|c| c.web.admin_allow = vec!["10.9.0.0/16".parse().unwrap()]).await;
    srv.create_user("root");
    promote(&srv, "root");
    let root = browser(&srv, "root").await;
    let r = srv.get("/api/v2/admin/overview", &root).await;
    r.err_reason(403, "forbidden", "admin_network_denied");
    // The user centre is not limited.
    srv.get("/api/v2/me/security", &root).await.ok();
    srv.shutdown().await;
}

#[tokio::test]
async fn totp_guards_the_admin_console() {
    let srv = start().await;
    srv.create_user("root");
    srv.create_user("alice");
    promote(&srv, "root");
    let root = browser(&srv, "root").await;
    let alice = browser(&srv, "alice").await;

    // Only administrators turn it on; the password is required.
    srv.post("/api/v2/me/totp/setup", &alice, json!({"password": PASSWORD}))
        .await
        .err_reason(403, "forbidden", "totp_admin_only");
    srv.post("/api/v2/me/totp/setup", &root, json!({"password": "wrong-password"}))
        .await
        .err_reason(403, "forbidden", "password_incorrect");
    let setup = srv
        .post("/api/v2/me/totp/setup", &root, json!({"password": PASSWORD}))
        .await;
    let secret = setup.ok()["secret"].as_str().unwrap().to_owned();
    assert!(setup.body["uri"].as_str().unwrap().starts_with("otpauth://totp/"));
    srv.post("/api/v2/me/totp/enable", &root, json!({"code": "000000"}))
        .await
        .err_reason(403, "forbidden", "totp_code_incorrect");
    let enabled = srv
        .post("/api/v2/me/totp/enable", &root, json!({"code": totp_code(&secret)}))
        .await;
    let codes: Vec<String> = enabled.ok()["recovery_codes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c.as_str().unwrap().to_owned())
        .collect();
    assert_eq!(codes.len(), 10);
    // The login that enabled it counts as verified.
    srv.get("/api/v2/admin/overview", &root).await.ok();

    // A new login needs the second step for the console, not for the user centre.
    let root2 = browser(&srv, "root").await;
    let s = srv.get("/api/v2/me/security", &root2).await;
    assert_eq!((s.ok()["totp"].as_bool(), s.body["verified"].as_bool()), (Some(true), Some(false)));
    let r = srv.get("/api/v2/admin/overview", &root2).await;
    r.err_reason(403, "forbidden", "totp_required");
    srv.post("/api/v2/auth/totp", &root2, json!({"code": "123456"}))
        .await
        .err(403, "forbidden");
    srv.post("/api/v2/auth/totp", &root2, json!({"code": totp_code(&secret)}))
        .await
        .ok();
    srv.get("/api/v2/admin/overview", &root2).await.ok();

    // A recovery code works once.
    let root3 = browser(&srv, "root").await;
    srv.post("/api/v2/auth/totp", &root3, json!({"code": codes[0]})).await.ok();
    let root4 = browser(&srv, "root").await;
    srv.post("/api/v2/auth/totp", &root4, json!({"code": codes[0]}))
        .await
        .err(403, "forbidden");
    let s = srv.get("/api/v2/me/security", &root3).await;
    assert_eq!(s.ok()["recovery_codes_left"], 9);

    // Turning it off needs the password and a code.
    srv.post("/api/v2/me/totp/disable", &root, json!({"password": PASSWORD, "code": "000000"}))
        .await
        .err(403, "forbidden");
    srv.post(
        "/api/v2/me/totp/disable",
        &root,
        json!({"password": PASSWORD, "code": codes[1]}),
    )
    .await
    .ok();
    srv.get("/api/v2/admin/overview", &root4).await.ok();
    srv.shutdown().await;
}

#[tokio::test]
async fn last_administrator_and_self_are_protected() {
    let srv = start().await;
    let root_id = srv.create_user("root");
    let bob_id = srv.create_user("bob");
    promote(&srv, "root");
    let root = browser(&srv, "root").await;

    for body in [json!({"admin": false}), json!({"disabled": true})] {
        let r = srv
            .call(Method::PATCH, &format!("/api/v2/admin/users/{root_id}"), &root, Some(body))
            .await;
        r.err_reason(400, "bad_request", "last_admin");
    }
    srv.delete(&format!("/api/v2/admin/users/{root_id}"), &root)
        .await
        .err_reason(400, "bad_request", "self_delete");
    // So does an administration token.
    let token = srv.admin_token("admin");
    srv.admin_call(
        Method::PATCH,
        &format!("/api/v2/admin/users/{root_id}"),
        &token,
        Some(json!({"admin": false})),
    )
    .await
    .err_reason(400, "bad_request", "last_admin");

    // With a second administrator, the first may be demoted by the second.
    srv.call(Method::PATCH, &format!("/api/v2/admin/users/{bob_id}"), &root, Some(json!({"admin": true})))
        .await
        .ok();
    let bob = browser(&srv, "bob").await;
    srv.call(Method::PATCH, &format!("/api/v2/admin/users/{root_id}"), &bob, Some(json!({"admin": false})))
        .await
        .ok();
    srv.get("/api/v2/admin/overview", &root).await.err(403, "forbidden");
    srv.shutdown().await;
}

#[tokio::test]
async fn administrators_manage_accounts() {
    let srv = start().await;
    srv.create_user("root");
    promote(&srv, "root");
    let root = browser(&srv, "root").await;

    // Created with a generated password.
    let r = srv
        .post("/api/v2/admin/users", &root, json!({"username": "carol", "nickname": "Carol"}))
        .await;
    let carol_id = r.ok()["id"].as_str().unwrap().to_owned();
    let generated = r.body["password"].as_str().unwrap().to_owned();
    assert!(generated.len() >= 12);
    srv.post("/api/v2/admin/users", &root, json!({"username": "carol"}))
        .await
        .err_reason(409, "conflict", "username_taken");
    let r = srv
        .post("/api/v2/admin/users", &root, json!({"username": "bad name!", "password": "long-enough-1"}))
        .await;
    r.err_reason(400, "bad_request", "username_invalid");
    assert_eq!(r.body["error"]["details"]["max"], 64);
    let r = srv
        .post("/api/v2/admin/users", &root, json!({"username": "dave", "password": "short"}))
        .await;
    r.err_reason(400, "bad_request", "password_too_short");
    assert_eq!(r.body["error"]["details"]["min"], 8);

    let mut carol = srv.init_device("CAROL-PC", "windows", true).await;
    let login = srv
        .post("/api/v2/auth/login", &carol, json!({"username": "carol", "password": generated}))
        .await;
    carol.access = Some(login.ok()["access_token"].as_str().unwrap().to_owned());

    let list = srv.get("/api/v2/admin/users", &root).await;
    let carol_row = list
        .ok()
        .as_array()
        .unwrap()
        .iter()
        .find(|u| u["username"] == "carol")
        .unwrap()
        .clone();
    assert_eq!((carol_row["nickname"].as_str(), carol_row["devices"].as_i64()), (Some("Carol"), Some(1)));
    let detail = srv.get(&format!("/api/v2/admin/users/{carol_id}"), &root).await;
    assert_eq!(detail.ok()["devices"][0]["device_id"], carol.id.as_str());
    assert_eq!(detail.body["logins"].as_array().unwrap().len(), 1);

    // A password reset ends every login of the account.
    let reset = srv
        .post(&format!("/api/v2/admin/users/{carol_id}/password"), &root, json!({}))
        .await;
    let new_password = reset.ok()["password"].as_str().unwrap().to_owned();
    assert_ne!(new_password, generated);
    srv.get("/api/v2/me", &carol).await.err(401, "session_revoked");

    // Disabled accounts cannot sign in.
    srv.call(Method::PATCH, &format!("/api/v2/admin/users/{carol_id}"), &root, Some(json!({"disabled": true})))
        .await
        .ok();
    srv.post("/api/v2/auth/login", &carol, json!({"username": "carol", "password": new_password}))
        .await
        .err_reason(401, "unauthorized", "login_failed");

    srv.delete(&format!("/api/v2/admin/users/{carol_id}"), &root).await.ok();
    srv.get(&format!("/api/v2/admin/users/{carol_id}"), &root)
        .await
        .err(404, "not_found");

    // Everything above is in the audit log.
    let audit = srv.get("/api/v2/admin/audit?limit=50", &root).await;
    let kinds: Vec<&str> = audit
        .ok()
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["kind"].as_str().unwrap())
        .collect();
    for kind in ["user.create", "user.password", "user.update", "user.delete", "login"] {
        assert!(kinds.contains(&kind), "{kind} missing in {kinds:?}");
    }
    // Details are language-neutral JSON.
    let entries = audit.body.as_array().unwrap();
    let detail = |kind: &str, ok: bool| -> serde_json::Value {
        let e = entries
            .iter()
            .find(|e| e["kind"] == kind && e["ok"] == ok)
            .unwrap_or_else(|| panic!("no {kind} entry"));
        serde_json::from_str(e["detail"].as_str().unwrap()).unwrap()
    };
    assert_eq!(detail("user.update", true), json!({"disabled": true}));
    assert_eq!(detail("login", false), json!({"reason": "login_failed"}));
    assert!(detail("login", true)["device_id"].is_string());
    let only = srv.get("/api/v2/admin/audit?kind=user.delete", &root).await;
    assert_eq!(only.ok().as_array().unwrap().len(), 1);
    srv.shutdown().await;
}

#[tokio::test]
async fn invitations_register_accounts() {
    let srv = start().await;
    srv.create_user("root");
    promote(&srv, "root");
    let root = browser(&srv, "root").await;

    let r = srv
        .post("/api/v2/admin/invites", &root, json!({"note": "team", "max_uses": 1, "auto_friend": true}))
        .await;
    let code = r.ok()["code"].as_str().unwrap().to_owned();
    let invite_id = r.body["id"].as_str().unwrap().to_owned();
    assert_eq!(code.len(), 14);

    let newcomer = srv.init_device("browser", "web", false).await;
    let body = json!({"invite": code, "username": "erin", "password": "erin-password"});
    // Registration is off by default.
    srv.post("/api/v2/auth/register", &newcomer, body.clone())
        .await
        .err(403, "registration_disabled");
    let settings = srv.get("/api/v2/admin/settings", &root).await;
    let mut runtime = settings.ok()["runtime"].clone();
    runtime["registration"] = json!(true);
    srv.put("/api/v2/admin/settings", &root, runtime).await.ok();
    assert_eq!(srv.get("/api/v2/web/info", &newcomer).await.ok()["registration"], true);

    srv.post(
        "/api/v2/auth/register",
        &newcomer,
        json!({"invite": "AAAA-BBBB-CCCC", "username": "erin", "password": "erin-password"}),
    )
    .await
    .err_reason(400, "bad_request", "invite_invalid");
    let reg = srv.post("/api/v2/auth/register", &newcomer, body).await;
    let mut erin = newcomer.clone();
    erin.access = Some(reg.ok()["access_token"].as_str().unwrap().to_owned());
    assert_eq!(srv.get("/api/v2/me", &erin).await.ok()["username"], "erin");
    // Friends with the inviter.
    let friends = srv.get("/api/v2/friends", &erin).await;
    assert_eq!(friends.ok()["friends"][0]["user"]["username"], "root");
    // Browsers are never shared with friends.
    let shared = srv.get("/api/v2/devices", &root).await;
    assert_eq!(shared.ok()["shared"], json!([]));
    srv.get(&format!("/api/v2/devices/{}/detail", erin.id), &root)
        .await
        .err(404, "device_not_found");

    // Used up.
    let other = srv.init_device("browser 2", "web", false).await;
    srv.post(
        "/api/v2/auth/register",
        &other,
        json!({"invite": code, "username": "frank", "password": "frank-password"}),
    )
    .await
    .err_reason(400, "bad_request", "invite_expired");
    let uses = srv.get(&format!("/api/v2/admin/invites/{invite_id}/uses"), &root).await;
    assert_eq!(uses.ok()[0]["username"], "erin");

    // Revoked invitations are refused.
    let r = srv.post("/api/v2/admin/invites", &root, json!({})).await;
    let (code2, id2) = (
        r.ok()["code"].as_str().unwrap().to_owned(),
        r.body["id"].as_str().unwrap().to_owned(),
    );
    srv.delete(&format!("/api/v2/admin/invites/{id2}"), &root).await.ok();
    srv.post(
        "/api/v2/auth/register",
        &other,
        json!({"invite": code2, "username": "frank", "password": "frank-password"}),
    )
    .await
    .err(400, "bad_request");
    srv.shutdown().await;
}

#[tokio::test]
async fn settings_and_announcements_apply_at_once() {
    let srv = start().await;
    srv.create_user("root");
    promote(&srv, "root");
    let root = browser(&srv, "root").await;

    let view = srv.get("/api/v2/admin/settings", &root).await;
    assert_eq!(view.ok()["settings"]["turn"]["shared_secret"], "", "secrets are not shown");
    assert_eq!(view.ok()["turn_secret_set"], true);
    srv.put(
        "/api/v2/admin/settings",
        &root,
        json!({"instance_name": "Lab", "bitrate_limit_mbps": 40}),
    )
    .await
    .ok();
    let info = srv.get("/api/v2/server", &root).await;
    assert_eq!(info.ok()["server"]["instance_name"], "Lab");
    assert_eq!(srv.get("/api/v2/config", &root).await.ok()["bitrate_limit_mbps"], 40);
    let r = srv
        .put("/api/v2/admin/settings", &root, json!({"bitrate_limit_mbps": 0}))
        .await;
    r.err_reason(400, "bad_request", "setting_invalid");
    assert_eq!(r.body["error"]["details"]["field"], "bitrate_limit_mbps");
    assert_eq!(r.body["error"]["details"]["problem"], "out_of_range");
    // The refused change left the settings as they were, secret included.
    assert_eq!(srv.get("/api/v2/config", &root).await.ok()["bitrate_limit_mbps"], 40);
    assert_eq!(srv.app().cfg().turn.shared_secret, "test-shared-secret");
    let audit = srv.get("/api/v2/admin/audit?kind=settings.update", &root).await;
    let detail: serde_json::Value =
        serde_json::from_str(audit.ok()[0]["detail"].as_str().unwrap()).unwrap();
    assert_eq!(detail, json!({"groups": ["bitrate_limit_mbps", "instance_name"]}));

    srv.post(
        "/api/v2/admin/updates",
        &root,
        json!({"platform": "windows", "channel": "stable", "version": "0.2.0",
               "notes": "fixes", "url": "https://example.com/farwing-0.2.0.msi", "published_at": 0}),
    )
    .await
    .ok();
    srv.post(
        "/api/v2/admin/updates",
        &root,
        json!({"platform": "windows", "channel": "stable", "version": "0.3.0",
               "url": "ftp://example.com/x", "published_at": 0}),
    )
    .await
    .err_reason(400, "bad_request", "update_url_invalid");
    let updates = srv.get("/api/v2/updates/windows", &root).await;
    assert_eq!(updates.ok()["latest"]["version"], "0.2.0", "{}", updates.body);
    let info = srv.get("/api/v2/web/info", &root).await;
    assert_eq!(info.ok()["downloads"][0]["version"], "0.2.0");
    srv.shutdown().await;
}

#[tokio::test]
async fn browser_logins_expire_sooner_and_web_caps_limit_browsers() {
    let srv = start().await;
    srv.create_user("alice");
    promote(&srv, "alice");
    let host = srv.account_device("alice", "ALICE-PC", true).await;
    let (_hw, _) = srv.presence(&host).await;
    let web = browser(&srv, "alice").await;
    let pc = srv.account_device("alice", "ALICE-LAPTOP", false).await;

    // By default a browser may do what a browser can: not ports, power,
    // display or diagnostics.
    let r = srv
        .post("/api/v2/sessions", &web, json!({"target_device_id": host.id, "purpose": "desktop"}))
        .await;
    assert_eq!(
        r.ok()["target"]["granted"],
        json!(["view", "control", "clipboard", "files", "audio", "microphone", "annotation"])
    );
    srv.post("/api/v2/sessions", &web, json!({"target_device_id": host.id, "purpose": "ports"}))
        .await
        .err_reason(403, "forbidden", "web_not_allowed");
    // Client devices are not limited.
    let r = srv
        .post("/api/v2/sessions", &pc, json!({"target_device_id": host.id, "purpose": "desktop"}))
        .await;
    assert!(r.ok()["target"]["granted"].as_array().unwrap().contains(&json!("ports")));

    // The administrator makes the web view-only.
    let settings = srv.get("/api/v2/admin/settings", &web).await.ok()["settings"].clone();
    assert_eq!(settings["web"]["caps"].as_array().unwrap().len(), 7);
    srv.put("/api/v2/admin/settings", &web, json!({"web": {"caps": ["audio", "view", "ports"]}}))
        .await
        .err(400, "bad_request");
    let r = srv
        .put("/api/v2/admin/settings", &web, json!({"web": {"caps": ["audio"]}}))
        .await;
    r.err_reason(400, "bad_request", "setting_invalid");
    assert_eq!(r.body["error"]["details"]["field"], "web.caps");
    let saved = srv
        .put("/api/v2/admin/settings", &web, json!({"web": {"caps": ["audio", "view"]}}))
        .await;
    assert_eq!(saved.ok()["settings"]["web"]["caps"], json!(["view", "audio"]));
    srv.post("/api/v2/sessions", &web, json!({"target_device_id": host.id, "purpose": "files"}))
        .await
        .err(403, "forbidden");
    let r = srv
        .post("/api/v2/sessions", &web, json!({"target_device_id": host.id, "purpose": "desktop"}))
        .await;
    assert_eq!(r.ok()["target"]["granted"], json!(["view", "audio"]));

    // An idle browser login ends; a client login of the same age does not.
    let idle = 31 * 60;
    srv.app()
        .db
        .call(move |c| {
            c.execute(
                "UPDATE access_tokens SET last_used_at = last_used_at - ?1, created_at = created_at - ?1",
                [idle],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    srv.get("/api/v2/me", &web).await.err(401, "session_revoked");
    srv.get("/api/v2/me", &pc).await.ok();
    srv.shutdown().await;
}

#[tokio::test]
async fn wrong_password_is_not_a_lost_login() {
    let srv = start().await;
    srv.create_user("alice");
    let web = browser(&srv, "alice").await;
    srv.post(
        "/api/v2/me/password",
        &web,
        json!({"old_password": "not-the-password", "new_password": "new-password-1"}),
    )
    .await
    .err_reason(403, "forbidden", "current_password_incorrect");
    srv.get("/api/v2/me", &web).await.ok();
    srv.post(
        "/api/v2/me/password",
        &web,
        json!({"old_password": PASSWORD, "new_password": "short"}),
    )
    .await
    .err(400, "bad_request");
    srv.shutdown().await;
}

/// A browser's realtime connection: credentials in the subprotocol list,
/// no `X-Farwing-Protocol` header (browsers cannot set headers).
#[tokio::test]
async fn browser_realtime_admission_by_subprotocol() {
    use futures_util::StreamExt;
    use rustls::pki_types::ServerName;
    use tokio::net::TcpStream;
    use tokio_rustls::TlsConnector;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    let srv = start().await;
    srv.create_user("alice");
    let host = srv.account_device("alice", "ALICE-PC", true).await;
    let (_hw, _) = srv.presence(&host).await;
    let web = browser(&srv, "alice").await;
    let ticket = srv
        .post("/api/v2/sessions", &web, json!({"target_device_id": host.id, "purpose": "desktop"}))
        .await;
    let ticket = ticket.ok()["ticket"].as_str().unwrap().to_owned();

    let connect = |protocols: String| {
        let tls = Arc::clone(&srv.tls);
        let port = srv.port;
        async move {
            let mut req = format!("wss://127.0.0.1:{port}/api/v2/ws").into_client_request().unwrap();
            req.headers_mut()
                .insert("sec-websocket-protocol", protocols.parse().unwrap());
            let tcp = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
            let tls = TlsConnector::from(tls)
                .connect(ServerName::try_from("127.0.0.1").unwrap(), tcp)
                .await
                .unwrap();
            tokio_tungstenite::client_async(req, tls).await
        }
    };

    // Without `farwing.v2` the protocol header is still required.
    assert!(connect(format!("ticket.{ticket}")).await.is_err());
    let (mut ws, response) = connect(format!("farwing.v2, ticket.{ticket}")).await.unwrap();
    assert_eq!(response.headers()["sec-websocket-protocol"], "farwing.v2");
    let hello: Value = match ws.next().await.unwrap().unwrap() {
        tokio_tungstenite::tungstenite::Message::Text(t) => serde_json::from_str(&t).unwrap(),
        other => panic!("unexpected {other:?}"),
    };
    assert_eq!((hello["type"].as_str(), hello["data"]["role"].as_str()), (Some("hello"), Some("controller")));
    // Tickets are single-use.
    assert!(connect(format!("farwing.v2, ticket.{ticket}")).await.is_err());
    srv.shutdown().await;
}

#[tokio::test]
async fn web_pages_and_unknown_api_paths() {
    let srv = start().await;
    let page = srv.send(srv.raw(Method::GET, "/admin/users")).await;
    assert_eq!(page.status, 200);
    let csp = page.headers["content-security-policy"].to_str().unwrap();
    assert!(csp.contains("default-src 'self'") && csp.contains("frame-ancestors 'none'"), "{csp}");
    assert_eq!(page.headers["x-frame-options"], "DENY");
    let api = srv.send(srv.raw(Method::GET, "/api/v2/nothing-here")).await;
    api.err(404, "not_found");
    srv.shutdown().await;
}
