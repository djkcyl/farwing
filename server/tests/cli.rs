//! The `farwing-server` binary: `init` and the admin commands.

use std::io::Write;
use std::process::{Command, Output, Stdio};

fn run(args: &[&str], stdin: Option<&str>) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_farwing-server"))
        .args(args)
        .env_remove("FARWING_CONFIG")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    {
        let mut pipe = child.stdin.take().unwrap();
        if let Some(input) = stdin {
            pipe.write_all(input.as_bytes()).unwrap();
        }
    }
    child.wait_with_output().unwrap()
}

#[track_caller]
fn ok(out: &Output) -> String {
    assert!(
        out.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn init_and_admin_commands() {
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = dir.path().join("conf").join("farwing-server.toml");
    let cfg = cfg_path.to_str().unwrap();
    ok(&run(
        &[
            "init",
            "--config",
            cfg,
            "--public-url",
            "https://farwing.test:9443",
            "--data-dir",
            "./data",
        ],
        None,
    ));
    let text = std::fs::read_to_string(&cfg_path).unwrap();
    assert!(!text.contains("CHANGE-ME"));
    assert!(text.contains("hostnames = [\"farwing.test\"]"));
    assert!(text.contains("\"turn:farwing.test:3478?transport=udp\""));
    let parsed = farwing_server::Config::load(&cfg_path).unwrap();
    assert_eq!(parsed.ws_endpoint(), "wss://farwing.test:9443/api/v2/ws");
    assert!(!text.contains("[input]"));
    assert_eq!(parsed.turn.shared_secret.len(), 43);
    // Refuses to overwrite.
    assert!(!run(&["init", "--config", cfg], None).status.success());

    ok(&run(
        &[
            "--config",
            cfg,
            "user",
            "create",
            "bob",
            "--nickname",
            "Bob",
            "--password-stdin",
        ],
        Some("bob-password-1\n"),
    ));
    let dup = run(
        &["--config", cfg, "user", "create", "bob", "--password-stdin"],
        Some("bob-password-1\n"),
    );
    assert!(!dup.status.success());
    let short = run(
        &["--config", cfg, "user", "create", "eve", "--password-stdin"],
        Some("short\n"),
    );
    assert!(!short.status.success());
    assert!(String::from_utf8_lossy(&short.stderr).contains("at least 8"));
    let list = ok(&run(&["user", "list", "--config", cfg], None));
    assert!(list.contains("bob") && list.contains("Bob") && list.contains("active"));
    ok(&run(&["user", "disable", "bob", "--config", cfg], None));
    assert!(ok(&run(&["user", "list", "--config", cfg], None)).contains("disabled"));
    ok(&run(&["user", "enable", "bob", "--config", cfg], None));
    ok(&run(
        &["user", "passwd", "bob", "--password-stdin", "--config", cfg],
        Some("bob-password-2\n"),
    ));
    ok(&run(&["device", "list", "--config", cfg], None));
    ok(&run(
        &["device", "list", "--user", "bob", "--config", cfg],
        None,
    ));
    assert!(
        !run(
            &["device", "remove", "aaaaaaaaaaaaaaaa", "--config", cfg],
            None
        )
        .status
        .success()
    );
    ok(&run(&["session", "list", "--config", cfg], None));
    let friends = ok(&run(&["friend", "list", "bob", "--config", cfg], None));
    assert!(friends.contains("GRANTED BY bob"));
    assert!(!run(&["friend", "list", "nobody", "--config", cfg], None)
        .status
        .success());
    ok(&run(&["user", "delete", "bob", "--config", cfg], None));
    assert!(!ok(&run(&["user", "list", "--config", cfg], None)).contains("bob"));
    assert!(
        !run(&["user", "list", "--config", "missing.toml"], None)
            .status
            .success()
    );
}
