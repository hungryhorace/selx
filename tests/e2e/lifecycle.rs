//! Daemon lifecycle: starting one, reusing it, replacing an installed-over
//! one, and every way it is expected to go away.

use crate::{
    alive, connect_as_agent, failed, install_over, installed_copy, succeeded, wait_until, Env,
};
use serde_json::Value;
use std::io::{BufRead, BufReader};
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

#[test]
fn ensure_send_status_stop() {
    let env = Env::new();
    let ensured = env.json(&["ensure", "--json"]);
    assert_eq!(ensured["started"], true);
    assert_eq!(
        ensured["root"].as_str().unwrap(),
        env.root().to_str().unwrap()
    );
    let port = ensured["port"].as_u64().unwrap();
    assert!((10000..=65535).contains(&port));

    // Bare `ensure` prints just the port.
    let out = env.run(&["ensure"]);
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        port.to_string()
    );

    // Second ensure is a no-op on the same daemon.
    let again = env.json(&["ensure", "--json"]);
    assert_eq!(again["started"], false);
    assert_eq!(again["pid"], ensured["pid"]);

    std::fs::write(env.root.path().join("main.rs"), "fn main() {}\n").unwrap();
    let out = env.json(&[
        "send",
        "--file",
        "main.rs",
        "--start",
        "1",
        "--end",
        "1",
        "--explicit",
        "--source",
        "helix",
        "--json",
    ]);
    assert_eq!(out["delivered"], serde_json::json!(["log"]));

    // Ambient send with text: the log adapter accepts it, and text never shows up in status.
    let mut c = env.cmd();
    c.args([
        "send",
        "--file",
        "main.rs",
        "--start",
        "1",
        "--text-stdin",
        "--source",
        "lazygit",
    ])
    .stdin(Stdio::piped());
    let mut child = c.spawn().unwrap();
    use std::io::Write;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"SECRET-TEXT")
        .unwrap();
    assert!(child.wait().unwrap().success());

    let status = env.json(&["status", "--json"]);
    assert_eq!(status["events_received"], 2);
    assert_eq!(status["clients"], 0);
    assert_eq!(status["adapters"][0]["name"], "log");
    assert_eq!(status["adapters"][0]["supports_ambient"], true);
    let recent = status["recent"].as_array().unwrap();
    assert_eq!(recent.len(), 2);
    assert_eq!(recent[0]["event"]["kind"], "explicit");
    assert_eq!(recent[0]["event"]["source"], "helix");
    assert_eq!(
        recent[0]["event"]["file"].as_str().unwrap(),
        env.root().join("main.rs").to_str().unwrap()
    );
    assert_eq!(recent[1]["event"]["kind"], "ambient");
    assert!(
        recent[1]["event"].get("text").is_none(),
        "text leaked into status"
    );
    let raw = String::from_utf8_lossy(&env.run(&["status", "--json"]).stdout).to_string();
    assert!(!raw.contains("SECRET-TEXT"));

    // Human status mentions the essentials.
    let human = String::from_utf8_lossy(&env.run(&["status"]).stdout).to_string();
    assert!(human.contains("running"), "{human}");
    assert!(human.contains("main.rs:1"), "{human}");

    // Events log exists in the state dir, redacted.
    let state_dir = PathBuf::from(ensured["state_dir"].as_str().unwrap());
    let log = std::fs::read_to_string(state_dir.join("events.log")).unwrap();
    assert_eq!(log.lines().count(), 2);
    assert!(!log.contains("SECRET-TEXT"));

    let pid = env.pid();
    succeeded(env.run(&["stop"]));
    assert!(wait_until(Duration::from_secs(5), || !alive(pid)));
    assert!(!state_dir.join("pid").exists());
    assert!(!state_dir.join("ingest.sock").exists());
    let out = env.run(&["status"]);
    assert_eq!(out.status.code(), Some(2));
    let out = env.run(&["stop"]);
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn ensure_recovers_from_kill_9() {
    let env = Env::new();
    let first = env.json(&["ensure", "--json"]);
    let pid = first["pid"].as_u64().unwrap() as u32;
    let state_dir = PathBuf::from(first["state_dir"].as_str().unwrap());

    Command::new("kill")
        .args(["-9", &pid.to_string()])
        .status()
        .unwrap();
    assert!(wait_until(Duration::from_secs(5), || !alive(pid)));
    // Stale files are left behind by SIGKILL...
    assert!(state_dir.join("pid").exists());
    let out = env.run(&["status", "--json"]);
    assert_eq!(out.status.code(), Some(2));

    // ...and ensure cleans them up and starts a fresh daemon.
    let second = env.json(&["ensure", "--json"]);
    assert_eq!(second["started"], true);
    let pid2 = second["pid"].as_u64().unwrap() as u32;
    assert_ne!(pid, pid2);
    assert_eq!(
        std::fs::read_to_string(state_dir.join("pid"))
            .unwrap()
            .trim(),
        pid2.to_string()
    );
    assert!(alive(pid2));
    assert_eq!(env.pid(), pid2);
}

/// A daemon outlives the command that started it and keeps running the
/// binary it was started from, so installing a new selx over the old one
/// leaves the new CLI driving old behaviour. Every command checks, and
/// replaces an idle daemon rather than talking to it.
#[test]
fn an_installed_over_daemon_is_replaced_on_the_next_command() {
    let env = Env::new();
    let bindir = tempfile::Builder::new()
        .prefix("selx-bin")
        .tempdir()
        .unwrap();
    let bin = installed_copy(bindir.path());

    let first = env.json_bin(&bin, &["ensure", "--json"]);
    let pid = first["pid"].as_u64().unwrap() as u32;
    assert_eq!(first["started"], true);
    assert_eq!(first["restarted"], false);
    assert_eq!(first["outdated"], Value::Null);

    install_over(&bin);

    // The old daemon is idle (no agent has connected), so it goes.
    let second = env.json_bin(&bin, &["ensure", "--json"]);
    assert_eq!(second["restarted"], true, "{second}");
    assert_eq!(second["started"], true, "{second}");
    let pid2 = second["pid"].as_u64().unwrap() as u32;
    assert_ne!(pid2, pid);
    assert!(wait_until(Duration::from_secs(5), || !alive(pid)));

    // ...and the replacement is current, so nothing restarts it again.
    let third = env.json_bin(&bin, &["ensure", "--json"]);
    assert_eq!(third["pid"].as_u64().unwrap() as u32, pid2, "{third}");
    assert_eq!(third["restarted"], false);

    // The old CLI is now the odd one out, and says so without acting.
    let out = env.run(&["status"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("older selx binary"), "{text}");
    assert_eq!(env.pid(), pid2, "status must not restart anything");
}

/// Replacing a daemon drops every agent connected to it, so one with a
/// Claude attached is reported and left running, however old it is.
#[test]
fn an_outdated_daemon_with_an_agent_attached_is_left_running() {
    let env = Env::new();
    let bindir = tempfile::Builder::new()
        .prefix("selx-bin")
        .tempdir()
        .unwrap();
    let bin = installed_copy(bindir.path());

    let first = env.json_bin(&bin, &["ensure", "--json"]);
    let pid = first["pid"].as_u64().unwrap() as u32;
    let port = first["port"].as_u64().unwrap();

    let lock: Value =
        serde_json::from_str(&std::fs::read_to_string(env.lock_file(port)).unwrap()).unwrap();
    let _agent = connect_as_agent(port, lock["authToken"].as_str().unwrap());
    assert!(wait_until(Duration::from_secs(5), || {
        env.json(&["status", "--json"])["clients"] == 1
    }));

    install_over(&bin);

    let out = env.run_bin(&bin, &["ensure", "--json"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("older selx"), "no warning: {stderr}");
    assert!(stderr.contains("selx stop"), "no remedy named: {stderr}");
    let second: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(second["pid"].as_u64().unwrap() as u32, pid, "{second}");
    assert_eq!(second["restarted"], false);
    assert_eq!(second["outdated"]["clients"], 1, "{second}");
    assert!(alive(pid), "the agent's daemon was killed under it");
}

#[test]
fn concurrent_ensure_starts_exactly_one_daemon() {
    let env = Env::new();
    let children: Vec<_> = (0..6)
        .map(|_| {
            let mut c = env.cmd();
            c.args(["ensure", "--json"])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            c.spawn().unwrap()
        })
        .collect();
    let outs: Vec<Value> = children
        .into_iter()
        .map(|c| {
            let o = succeeded(c.wait_with_output().unwrap());
            serde_json::from_slice(&o.stdout).unwrap()
        })
        .collect();
    let pids: std::collections::HashSet<u64> =
        outs.iter().map(|o| o["pid"].as_u64().unwrap()).collect();
    assert_eq!(pids.len(), 1, "multiple daemons: {outs:?}");
    let started = outs.iter().filter(|o| o["started"] == true).count();
    assert_eq!(started, 1, "{outs:?}");
    // Only one daemon process exists for this state dir.
    let ps = String::from_utf8_lossy(
        &Command::new("pgrep")
            .args([
                "-f",
                &format!("selx daemon --root {}", env.root().display()),
            ])
            .output()
            .unwrap()
            .stdout,
    )
    .to_string();
    assert_eq!(ps.lines().count(), 1, "processes: {ps}");
}

#[test]
fn daemon_refuses_to_run_twice_for_one_root() {
    let env = Env::new();
    env.json(&["ensure", "--json"]);
    let out = failed(env.run(&[
        "daemon",
        "--root",
        env.root().to_str().unwrap(),
        "--foreground",
    ]));
    assert!(String::from_utf8_lossy(&out.stderr).contains("already running"));
}

#[test]
fn follow_streams_events_live() {
    let env = Env::new();
    env.json(&["ensure", "--json"]);
    let mut c = env.cmd();
    c.args(["status", "--follow", "--json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut follower = c.spawn().unwrap();
    let mut lines = BufReader::new(follower.stdout.take().unwrap()).lines();
    // First line: the status snapshot.
    let first: Value = serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
    assert_eq!(first["ok"], true);

    std::fs::write(env.root.path().join("f.rs"), "").unwrap();
    // Give the follower a moment to subscribe before the send.
    std::thread::sleep(Duration::from_millis(150));
    succeeded(env.run(&[
        "send",
        "--file",
        "f.rs",
        "--start",
        "7",
        "--end",
        "9",
        "--explicit",
        "--source",
        "helix",
    ]));

    let frame: Value = serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
    assert_eq!(frame["event"]["lines"]["start"], 7);
    assert_eq!(frame["event"]["lines"]["end"], 9);
    assert_eq!(frame["event"]["kind"], "explicit");
    assert_eq!(frame["outcome"]["delivered"], serde_json::json!(["log"]));

    // Stopping the daemon ends the follow stream cleanly.
    env.run(&["stop"]);
    assert!(lines.next().is_none());
    assert!(wait_until(Duration::from_secs(5), || follower
        .try_wait()
        .unwrap()
        .is_some()));
}

#[test]
fn idle_shutdown_cleans_up() {
    let env = Env::new();
    let mut c = env.cmd();
    c.env("SELX_IDLE_TIMEOUT", "1").args(["ensure", "--json"]);
    let out = succeeded(c.output().unwrap());
    let ensured: Value = serde_json::from_slice(&out.stdout).unwrap();
    let pid = ensured["pid"].as_u64().unwrap() as u32;
    let state_dir = PathBuf::from(ensured["state_dir"].as_str().unwrap());
    assert!(
        wait_until(Duration::from_secs(5), || !alive(pid)),
        "daemon did not idle out"
    );
    assert!(!state_dir.join("pid").exists());
    assert!(!state_dir.join("port").exists());
    assert!(!state_dir.join("ingest.sock").exists());
    let log = std::fs::read_to_string(state_dir.join("daemon.log")).unwrap();
    assert!(log.contains("idle"), "{log}");
}

#[test]
fn sigterm_cleans_up() {
    let env = Env::new();
    let ensured = env.json(&["ensure", "--json"]);
    let pid = ensured["pid"].as_u64().unwrap() as u32;
    let state_dir = PathBuf::from(ensured["state_dir"].as_str().unwrap());
    Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status()
        .unwrap();
    assert!(wait_until(Duration::from_secs(5), || !alive(pid)));
    assert!(!state_dir.join("pid").exists());
    assert!(!state_dir.join("ingest.sock").exists());
}

/// `selx status | head` must end the way every other Unix tool does.
/// Rust ignores SIGPIPE, so the closed pipe arrives as an EPIPE that
/// `println!` could only report by panicking; `crate::outln!` handles it.
#[test]
fn writing_into_a_closed_pipe_ends_quietly() {
    let env = Env::new();
    env.json(&["ensure", "--json"]);

    let mut child = env
        .cmd()
        .args(["status", "--follow"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Read the status it prints before following, then close the pipe
    // under it — as `head` does once it has the lines it wanted.
    let mut line = String::new();
    let mut out = BufReader::new(child.stdout.take().unwrap());
    out.read_line(&mut line).unwrap();
    assert!(line.contains("selx daemon: running"), "{line}");
    drop(out);

    // Now give it something to write.
    let file = env.root().join("src/main.rs");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, "fn main() {}\n").unwrap();
    env.run(&["send", "--file", file.to_str().unwrap(), "--start", "1"]);

    let out = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("panicked"),
        "panicked on a closed pipe: {stderr}"
    );
    // 141 is what a program killed by SIGPIPE reports, so pipelines see
    // the usual thing — and no signal killed us mid-request to get it.
    assert_eq!(out.status.code(), Some(141), "{:?} / {stderr}", out.status);
    assert_eq!(out.status.signal(), None, "died by signal, not by choice");
}

#[test]
fn subfolder_pairs_with_worktree_root_and_plain_folders_stand_alone() {
    let env = Env::new();
    let ok = Command::new("git")
        .args(["init", "-q"])
        .arg(env.root.path())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        eprintln!("git unavailable; skipping");
        return;
    }
    let sub = env.root.path().join("src/deep");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::write(sub.join("x.rs"), "x").unwrap();

    // send from the subfolder, relative path: pairs with the repo root.
    let out = env.run_in(
        &sub,
        &[
            "send",
            "--file",
            "x.rs",
            "--start",
            "2",
            "--end",
            "5",
            "--explicit",
            "--json",
        ],
    );
    succeeded(out);
    let status: Value =
        serde_json::from_slice(&env.run_in(&sub, &["status", "--json"]).stdout).unwrap();
    assert_eq!(
        status["root"].as_str().unwrap(),
        env.root().to_str().unwrap()
    );
    assert_eq!(
        status["recent"][0]["event"]["file"].as_str().unwrap(),
        env.root().join("src/deep/x.rs").to_str().unwrap()
    );
    assert_eq!(status["recent"][0]["event"]["lines"]["start"], 2);
    assert_eq!(status["recent"][0]["event"]["lines"]["end"], 5);

    // A file outside any repo, sent from inside the repo, falls back to cwd's root.
    let outside = tempfile::tempdir().unwrap();
    let f = outside.path().join("o.txt");
    std::fs::write(&f, "o").unwrap();
    let out = env.run_in(
        &sub,
        &[
            "send",
            "--file",
            f.to_str().unwrap(),
            "--start",
            "1",
            "--json",
        ],
    );
    succeeded(out);
    let status: Value = serde_json::from_slice(&env.run(&["status", "--json"]).stdout).unwrap();
    assert_eq!(status["events_received"], 2);

    // A plain, non-git folder is its own root (subfolder does not pair upward).
    let plain = tempfile::tempdir().unwrap();
    let plain_sub = plain.path().join("a");
    std::fs::create_dir_all(&plain_sub).unwrap();
    let ensured: Value =
        serde_json::from_slice(&env.run_in(&plain_sub, &["ensure", "--json"]).stdout).unwrap();
    assert_eq!(
        ensured["root"].as_str().unwrap(),
        std::fs::canonicalize(&plain_sub).unwrap().to_str().unwrap()
    );

    // Explicit --root overrides everything.
    let ensured: Value = serde_json::from_slice(
        &env.run_in(
            &plain_sub,
            &["ensure", "--json", "--root", plain.path().to_str().unwrap()],
        )
        .stdout,
    )
    .unwrap();
    assert_eq!(
        ensured["root"].as_str().unwrap(),
        std::fs::canonicalize(plain.path())
            .unwrap()
            .to_str()
            .unwrap()
    );

    // stop --all takes down all three.
    let stopped: Value =
        serde_json::from_slice(&env.run(&["stop", "--all", "--json"]).stdout).unwrap();
    assert_eq!(stopped.as_array().unwrap().len(), 3);
    assert!(stopped
        .as_array()
        .unwrap()
        .iter()
        .all(|s| s["was_running"] == true));
}
