//! The Claude Code side: the IDE lock file's lifecycle, and `selx claude`.

use crate::{alive, failed, succeeded, wait_until, Env};
use serde_json::Value;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

#[test]
fn claude_lock_file_lifecycle() {
    let env = Env::new();
    let ensured = env.json(&["ensure", "--json"]);
    let port = ensured["port"].as_u64().unwrap();
    let pid = ensured["pid"].as_u64().unwrap() as u32;
    let lock = env.lock_file(port);
    assert!(
        lock.exists(),
        "lock file should exist once ensure reports healthy"
    );
    let content: Value = serde_json::from_str(&std::fs::read_to_string(&lock).unwrap()).unwrap();
    assert_eq!(content["pid"], pid);
    assert_eq!(
        content["workspaceFolders"],
        serde_json::json!([env.root().to_str().unwrap()])
    );
    assert_eq!(content["ideName"], "selx");
    assert_eq!(content["transport"], "ws");
    assert_eq!(content["authToken"].as_str().unwrap().len(), 36);
    assert_eq!(
        std::fs::metadata(&lock).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        std::fs::metadata(lock.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );

    // Status shows the adapter with zero clients.
    let status = env.json(&["status", "--json"]);
    let names: Vec<&str> = status["adapters"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"claude-ide"), "{names:?}");
    assert_eq!(status["clients"], 0);

    // With no Claude connected, an explicit send is dropped by claude-ide but still logged.
    std::fs::write(env.root.path().join("a.rs"), "x\n").unwrap();
    let out = env.json(&[
        "send",
        "--file",
        "a.rs",
        "--start",
        "1",
        "--explicit",
        "--json",
    ]);
    assert_eq!(out["delivered"], serde_json::json!(["log"]));
    assert_eq!(out["dropped"][0]["adapter"], "claude-ide");

    // Graceful stop removes the lock file.
    env.run(&["stop"]);
    assert!(wait_until(Duration::from_secs(5), || !alive(pid)));
    assert!(!lock.exists(), "lock file must be removed on stop");

    // kill -9 leaves it orphaned; the next ensure replaces it with a fresh token.
    let ensured = env.json(&["ensure", "--json"]);
    let pid2 = ensured["pid"].as_u64().unwrap() as u32;
    let old_token = serde_json::from_str::<Value>(&std::fs::read_to_string(&lock).unwrap())
        .unwrap()["authToken"]
        .clone();
    Command::new("kill")
        .args(["-9", &pid2.to_string()])
        .status()
        .unwrap();
    assert!(wait_until(Duration::from_secs(5), || !alive(pid2)));
    assert!(lock.exists(), "SIGKILL cannot clean up");
    let ensured = env.json(&["ensure", "--json"]);
    assert_eq!(ensured["started"], true);
    let fresh: Value = serde_json::from_str(&std::fs::read_to_string(&lock).unwrap()).unwrap();
    assert_eq!(fresh["pid"], ensured["pid"]);
    assert_ne!(fresh["authToken"], old_token);

    // A foreign IDE's lock file on another port is left alone by cleanup.
    let foreign = env.lock_file(port + 1);
    std::fs::write(&foreign, r#"{"pid":1,"workspaceFolders":["/x"],"ideName":"Visual Studio Code","transport":"ws","authToken":"t"}"#).unwrap();
    env.run(&["stop"]);
    env.json(&["ensure", "--json"]);
    assert!(foreign.exists());
}

#[test]
fn selx_claude_exports_ide_environment() {
    let env = Env::new();
    // Stand in for the real `claude` binary with `env`, which prints its environment.
    let mut c = env.cmd();
    c.env("SELX_CLAUDE_BIN", "/usr/bin/env")
        .env("SELX_CLAUDE_UPDATE", "0")
        .arg("claude");
    let out = succeeded(c.output().unwrap());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let port = env.json(&["status", "--json"])["port"].as_u64().unwrap();
    assert!(
        stdout.contains(&format!("CLAUDE_CODE_SSE_PORT={port}\n")),
        "{stdout}"
    );
    assert!(stdout.contains("ENABLE_IDE_INTEGRATION=true\n"), "{stdout}");
    // Claude's own updater is off: updating happens before the session (below).
    assert!(stdout.contains("DISABLE_AUTOUPDATER=1\n"), "{stdout}");
    // The daemon's lock file is in place before claude is exec'd.
    assert!(env.lock_file(port).exists());

    // Arguments (including flags) pass through untouched.
    let mut c = env.cmd();
    c.env("SELX_CLAUDE_BIN", "/bin/echo")
        .env("SELX_CLAUDE_UPDATE", "0")
        .args(["claude", "--", "--resume", "-p", "hi there"]);
    let out = succeeded(c.output().unwrap());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "--resume -p hi there"
    );
    let mut c = env.cmd();
    c.env("SELX_CLAUDE_BIN", "/bin/echo")
        .env("SELX_CLAUDE_UPDATE", "0")
        .args(["claude", "--resume"]);
    assert_eq!(
        String::from_utf8_lossy(&c.output().unwrap().stdout).trim(),
        "--resume"
    );

    // A missing binary is a clear error, not a hang.
    let mut c = env.cmd();
    c.env("SELX_CLAUDE_BIN", "/nonexistent/claude")
        .arg("claude");
    let out = failed(c.output().unwrap());
    assert!(String::from_utf8_lossy(&out.stderr).contains("SELX_CLAUDE_BIN"));
}

/// Updates happen once, before the session, never underneath it: `claude
/// update` runs first, and the Claude that is then exec'd has its background
/// updater switched off. A failed update is not fatal.
#[test]
fn selx_claude_updates_before_starting_and_never_during() {
    let env = Env::new();
    let log = env.root().join("calls.log");
    let claude = env.root().join("claude-stub");
    std::fs::write(
        &claude,
        format!(
            "#!/bin/sh\necho \"$* [autoupdater=${{DISABLE_AUTOUPDATER:-unset}}]\" >> '{}'\n\
             [ \"$1\" = update ] && exit \"${{UPDATE_EXIT:-0}}\"\nexit 0\n",
            log.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).unwrap();

    let mut c = env.cmd();
    c.env("SELX_CLAUDE_BIN", &claude)
        .args(["claude", "--resume"]);
    succeeded(c.output().unwrap());
    assert_eq!(
        std::fs::read_to_string(&log).unwrap(),
        "update [autoupdater=unset]\n--resume [autoupdater=1]\n"
    );

    // An update that fails still lets the session start.
    std::fs::remove_file(&log).unwrap();
    let mut c = env.cmd();
    c.env("SELX_CLAUDE_BIN", &claude)
        .env("UPDATE_EXIT", "1")
        .arg("claude");
    succeeded(c.output().unwrap());
    assert_eq!(
        std::fs::read_to_string(&log).unwrap(),
        "update [autoupdater=unset]\n [autoupdater=1]\n"
    );

    // Opting out skips the update; the background updater stays off.
    std::fs::remove_file(&log).unwrap();
    let mut c = env.cmd();
    c.env("SELX_CLAUDE_BIN", &claude)
        .env("SELX_CLAUDE_UPDATE", "0")
        .arg("claude");
    succeeded(c.output().unwrap());
    assert_eq!(std::fs::read_to_string(&log).unwrap(), " [autoupdater=1]\n");
}

#[test]
fn selx_claude_registers_its_pane_with_the_daemon() {
    let env = Env::new();
    let ensured = env.json(&["ensure", "--json"]);
    let state_dir = PathBuf::from(ensured["state_dir"].as_str().unwrap());

    // Outside herdr there are no coordinates to register, and running
    // Claude must not care.
    let claude = env.stub_claude();
    let mut c = env.cmd();
    c.env("SELX_CLAUDE_BIN", &claude).arg("claude");
    succeeded(c.output().unwrap());

    // From a herdr pane, the pane id reaches the daemon before claude is
    // exec'd — Claude speaks no multiplexer, so nobody else can say.
    let mut c = env.cmd();
    c.env("SELX_CLAUDE_BIN", &claude)
        .env("HERDR_SESSION", "selx-e2e")
        .env("HERDR_WORKSPACE_ID", "w1")
        .env("HERDR_TAB_ID", "w1:t1")
        .env("HERDR_PANE_ID", "w1:p4")
        .arg("claude");
    succeeded(c.output().unwrap());

    let log = std::fs::read_to_string(state_dir.join("daemon.log")).unwrap();
    assert!(log.contains("Claude registered its pane"), "{log}");
    assert!(log.contains("w1:p4"), "{log}");
    // The daemon is still healthy afterwards.
    assert_eq!(env.json(&["status", "--json"])["ok"], true);
}
