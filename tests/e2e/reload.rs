//! `selx reload`, and the hooks `selx claude` installs to run it. What it
//! does to a real Helix is `scripts/reload-smoke.py`'s business; this is
//! what must hold without one. A hook runs after every edit Claude makes,
//! so it must never fail and never print; a reload it could not deliver is
//! owed until one gets through; and the switch turns all of it off.

use crate::{failed, succeeded, Env};
use serde_json::json;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};

const STOP: &str = r#"{"session_id":"s","hook_event_name":"Stop","stop_hook_active":false}"#;

fn edit(file: &Path) -> String {
    json!({
        "session_id": "s",
        "hook_event_name": "PostToolUse",
        "tool_name": "Edit",
        "tool_input": { "file_path": file, "old_string": "a", "new_string": "b" },
    })
    .to_string()
}

/// `selx reload --claude-hook` with `payload` on stdin, as Claude runs it.
fn hook(env: &Env, payload: &str, vars: &[(&str, &str)]) -> Output {
    let mut c = env.cmd();
    c.args(["reload", "--claude-hook"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in vars {
        c.env(k, v);
    }
    let mut child = c.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

/// Exit 0 and not a word: anything else is shown on every edit.
#[track_caller]
fn quietly(out: Output) {
    let out = succeeded(out);
    assert!(
        out.stdout.is_empty() && out.stderr.is_empty(),
        "a hook must say nothing:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn pending(env: &Env) -> PathBuf {
    env.state_dir().join("reload-pending")
}

/// A herdr that is "there" — its socket path exists and this is a herdr
/// pane — but answers nothing: the state in which a reload is owed.
fn broken_herdr(env: &Env) -> Vec<(&'static str, &'static str)> {
    std::fs::write(env.state.path().join("no-herdr.sock"), "").unwrap();
    vec![
        ("HERDR_WORKSPACE_ID", "w1"),
        ("HERDR_TAB_ID", "w1:t1"),
        ("HERDR_PANE_ID", "w1:p2"),
    ]
}

#[test]
fn a_hook_with_nothing_to_reload_succeeds_silently() {
    let env = Env::new();
    let file = env.root().join("a.rs");
    std::fs::write(&file, "x\n").unwrap();
    // Not in herdr, so no editor to reach: every payload is a quiet no.
    for payload in [
        edit(&file),
        STOP.to_string(),
        String::new(),
        "garbage".into(),
    ] {
        quietly(hook(&env, &payload, &[]));
    }
    // And nothing is owed: there is no editor to be stale.
    assert!(!pending(&env).exists());
}

#[test]
fn a_reload_that_cannot_get_through_is_owed_until_one_does() {
    let env = Env::new();
    let file = env.root().join("a.rs");
    std::fs::write(&file, "x\n").unwrap();
    let herdr = broken_herdr(&env);

    // An edit Claude made: herdr fails, so the reload is owed.
    quietly(hook(&env, &edit(&file), &herdr));
    assert!(pending(&env).exists(), "the failed reload should be owed");

    // A turn end tries again; still failing, still owed.
    quietly(hook(&env, STOP, &herdr));
    assert!(pending(&env).exists());

    // herdr gone altogether: no editor, so nothing is owed any more.
    std::fs::remove_file(env.state.path().join("no-herdr.sock")).unwrap();
    quietly(hook(&env, STOP, &herdr));
    assert!(!pending(&env).exists());

    // A turn end with nothing owed does not even look for an editor.
    let herdr = broken_herdr(&env);
    quietly(hook(&env, STOP, &herdr));
    assert!(!pending(&env).exists());

    // An edit outside the workspace is not this editor's business.
    let elsewhere = tempfile::tempdir().unwrap();
    let outside = elsewhere.path().join("CLAUDE.md");
    std::fs::write(&outside, "x\n").unwrap();
    quietly(hook(&env, &edit(&outside), &herdr));
    assert!(!pending(&env).exists());
}

#[test]
fn switching_reload_off_quiets_the_hook() {
    let env = Env::new();
    let file = env.root().join("a.rs");
    std::fs::write(&file, "x\n").unwrap();
    let mut herdr = broken_herdr(&env);

    // The same edit that is owed with reload on (above) is ignored.
    herdr.push(("SELX_RELOAD", "off"));
    quietly(hook(&env, &edit(&file), &herdr));
    assert!(!pending(&env).exists());
    herdr.pop();

    // The config file does it too — and, being read by every hook run, it
    // reaches a Claude that is already running.
    let config = env.root().join("selx.toml");
    std::fs::write(&config, "[reload]\nmode = \"off\"\n").unwrap();
    let with_config: Vec<(&str, &str)> = herdr
        .iter()
        .copied()
        .chain([("SELX_CONFIG", config.to_str().unwrap())])
        .collect();
    quietly(hook(&env, &edit(&file), &with_config));
    assert!(!pending(&env).exists());

    // A value selx cannot parse is still not worth an error on every edit.
    herdr.push(("SELX_RELOAD", "sometimes"));
    quietly(hook(&env, &edit(&file), &herdr));
    assert!(!pending(&env).exists());
}

#[test]
fn a_reload_asked_for_by_hand_says_why_it_did_not_happen() {
    let env = Env::new();
    let out = failed(env.run(&["reload"]));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("selx: warning: not reloading: not running in a herdr pane"),
        "{stderr}"
    );
}

/// What `selx claude` hands Claude, with `/bin/echo` standing in for it.
fn claude_args(env: &Env, args: &[&str], vars: &[(&str, &str)]) -> Output {
    let mut c = env.cmd();
    c.env("SELX_CLAUDE_BIN", "/bin/echo")
        .env("SELX_CLAUDE_UPDATE", "0")
        .arg("claude")
        .args(args);
    for (k, v) in vars {
        c.env(k, v);
    }
    c.output().unwrap()
}

#[test]
fn selx_claude_installs_the_reload_hooks_only_when_wanted() {
    let env = Env::new();
    let herdr = [
        ("HERDR_WORKSPACE_ID", "w1"),
        ("HERDR_TAB_ID", "w1:t1"),
        ("HERDR_PANE_ID", "w1:p2"),
    ];

    // In a herdr pane: `--settings` with the hooks, ahead of the user's
    // own arguments.
    let out = succeeded(claude_args(&env, &["--resume"], &herdr));
    let line = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let settings = line
        .strip_prefix("--settings ")
        .and_then(|rest| rest.strip_suffix(" --resume"))
        .unwrap_or_else(|| panic!("no --settings in front: {line}"));
    let v: serde_json::Value = serde_json::from_str(settings).unwrap();
    let command = v["hooks"]["PostToolUse"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap();
    assert!(
        command.ends_with(&format!(
            " reload --claude-hook --root '{}'",
            env.root().display()
        )),
        "{command}"
    );
    assert_eq!(v["hooks"]["Stop"][0]["hooks"][0]["command"], command);

    // Not outside herdr, where the hook could only ever do nothing.
    let out = succeeded(claude_args(&env, &["--resume"], &[]));
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "--resume");

    // Not when switched off.
    let mut off = herdr.to_vec();
    off.push(("SELX_RELOAD", "off"));
    let out = succeeded(claude_args(&env, &["--resume"], &off));
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "--resume");

    // Not over a `--settings` of the user's own, which is passed untouched.
    let out = succeeded(claude_args(&env, &["--settings", "mine.json"], &herdr));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "--settings mine.json"
    );

    // A switch selx cannot read is an error here, where someone is looking.
    let mut bad = herdr.to_vec();
    bad.push(("SELX_RELOAD", "sometimes"));
    let out = failed(claude_args(&env, &[], &bad));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("SELX_RELOAD"), "{stderr}");
    assert!(stderr.contains("on or off"), "{stderr}");
}
