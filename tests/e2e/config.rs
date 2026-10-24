//! The config file, and the environment variables that override it.
//!
//! Precedence, most specific first: CLI flag, environment, `[[workspace]]`
//! section, top level, built-in default.

use crate::{failed, succeeded, Env};

#[test]
fn config_file_sets_idle_timeout_with_workspace_override() {
    let env = Env::new();
    let cfg = env.state.path().join("config.toml");
    std::fs::write(
        &cfg,
        format!(
            "idle_timeout_secs = 3333\n\
             [[workspace]]\n\
             root = \"{}\"\n\
             idle_timeout_secs = 77\n",
            env.root().display()
        ),
    )
    .unwrap();

    let mut c = env.cmd();
    c.env("SELX_CONFIG", &cfg).args(["ensure"]);
    succeeded(c.output().unwrap());
    let status = env.json(&["status", "--json"]);
    assert_eq!(status["idle_timeout_secs"], 77, "workspace section wins");
    env.run(&["stop"]);

    // Environment variable beats the config file.
    let mut c = env.cmd();
    c.env("SELX_CONFIG", &cfg)
        .env("SELX_IDLE_TIMEOUT", "99")
        .args(["ensure"]);
    succeeded(c.output().unwrap());
    let status = env.json(&["status", "--json"]);
    assert_eq!(status["idle_timeout_secs"], 99);
    env.run(&["stop"]);

    // A config file that applies to some *other* workspace: top level applies.
    let mut c = env.cmd();
    let other = "idle_timeout_secs = 3333\n[[workspace]]\nroot = \"/no/such/root\"\nidle_timeout_secs = 1\n";
    std::fs::write(&cfg, other).unwrap();
    c.env("SELX_CONFIG", &cfg).args(["ensure"]);
    succeeded(c.output().unwrap());
    let status = env.json(&["status", "--json"]);
    assert_eq!(status["idle_timeout_secs"], 3333);
}

/// The injection settings have the same four layers as the idle timeout,
/// and until now only the timeout and the focus mode had any coverage of
/// them. The daemon reports what it resolved at startup, which is both how
/// this reads the answer back and how a user answers "why that pane?".
#[test]
fn inject_settings_take_the_environment_over_the_config_file() {
    let env = Env::new();
    let cfg = env.state.path().join("config.toml");
    std::fs::write(
        &cfg,
        format!(
            "[inject]\n\
             target = \"from-top-level\"\n\
             exclude = [\"claude\", \"from-top-level\"]\n\
             [[workspace]]\n\
             root = \"{}\"\n\
             inject = {{ target = \"from-workspace\" }}\n",
            env.root().display()
        ),
    )
    .unwrap();

    // The `[[workspace]]` section beats the top level, per setting: the
    // target comes from the section, the exclude list from above it.
    let mut c = env.cmd();
    c.env("SELX_CONFIG", &cfg).args(["ensure"]);
    succeeded(c.output().unwrap());
    let log = env.daemon_log();
    let line = configured_line(&log);
    assert!(line.contains("target=\"from-workspace\""), "{line}");
    assert!(line.contains("from-top-level"), "exclude list lost: {line}");
    succeeded(env.run(&["stop"]));

    // The environment beats both, for each setting independently.
    let mut c = env.cmd();
    c.env("SELX_CONFIG", &cfg)
        .env("SELX_INJECT_TARGET", "from-env")
        .env("SELX_INJECT_EXCLUDE", "claude, gemini")
        .args(["ensure"]);
    succeeded(c.output().unwrap());
    let log = env.daemon_log();
    let line = configured_line(&log);
    assert!(line.contains("target=\"from-env\""), "{line}");
    assert!(line.contains("claude,gemini"), "{line}");
    assert!(!line.contains("from-workspace"), "{line}");
    succeeded(env.run(&["stop"]));

    // With neither, locality decides and `claude` is excluded by default —
    // it has its own adapter.
    succeeded(env.run(&["ensure"]));
    let log = env.daemon_log();
    let line = configured_line(&log);
    assert!(line.contains("(locality)"), "{line}");
    assert!(line.contains("exclude=claude"), "{line}");
}

/// The last "text injection configured" line in a daemon log. Daemons for
/// one workspace append to the same file, so the newest one is the answer.
#[track_caller]
fn configured_line(log: &str) -> String {
    log.lines()
        .rfind(|l| l.contains("text injection configured"))
        .unwrap_or_else(|| panic!("no injection settings logged:\n{log}"))
        .to_string()
}

#[test]
fn broken_config_file_fails_ensure_with_advice() {
    let env = Env::new();
    let cfg = env.state.path().join("config.toml");
    std::fs::write(&cfg, "idle_timeout = 5\n").unwrap(); // typo'd key
    let mut c = env.cmd();
    c.env("SELX_CONFIG", &cfg).args(["ensure"]);
    let out = failed(c.output().unwrap());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("config file"), "stderr: {err}");
    assert!(err.contains("fix or remove"), "stderr: {err}");
    // Nothing left half-started.
    let out = env.run(&["status"]);
    assert_eq!(out.status.code(), Some(2), "daemon must not be running");
}

#[test]
fn an_unusable_focus_mode_fails_ensure_with_the_valid_values() {
    let env = Env::new();
    let mut c = env.cmd();
    c.env("SELX_FOCUS", "sometimes").args(["ensure"]);
    let out = failed(c.output().unwrap());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("delivered, explicit or off"), "stderr: {err}");
    assert_eq!(env.run(&["status"]).status.code(), Some(2));

    // The config file spelling is checked the same way.
    let cfg = env.state.path().join("config.toml");
    std::fs::write(&cfg, "[focus]\nmode = \"sometimes\"\n").unwrap();
    let mut c = env.cmd();
    c.env("SELX_CONFIG", &cfg).args(["ensure"]);
    let out = failed(c.output().unwrap());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("config file"),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // A valid mode starts fine.
    let mut c = env.cmd();
    c.env("SELX_FOCUS", "off").args(["ensure"]);
    succeeded(c.output().unwrap());
}

#[test]
fn an_unusable_idle_timeout_fails_ensure_rather_than_falling_through() {
    let env = Env::new();
    // A duration spelling selx does not accept must say so, not quietly
    // become the default — the same rule SELX_FOCUS follows.
    let mut c = env.cmd();
    c.env("SELX_IDLE_TIMEOUT", "5m").args(["ensure"]);
    let out = failed(c.output().unwrap());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("whole number of seconds"), "stderr: {err}");
    assert_eq!(env.run(&["status"]).status.code(), Some(2));

    // Seconds start fine, and an empty value is "unset", not malformed.
    let mut c = env.cmd();
    c.env("SELX_IDLE_TIMEOUT", " 300 ").args(["ensure"]);
    succeeded(c.output().unwrap());
    succeeded(env.run(&["stop"]));
    let mut c = env.cmd();
    c.env("SELX_IDLE_TIMEOUT", "").args(["ensure"]);
    succeeded(c.output().unwrap());
}
