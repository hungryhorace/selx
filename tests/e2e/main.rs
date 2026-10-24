//! End-to-end tests against the built `selx` binary. Each test gets its own
//! state dir (via `SELX_STATE_DIR`) and its own workspace root, so tests can
//! run in parallel and never touch the user's real daemons.
//!
//! Cargo builds `tests/e2e/` as one test binary, so the split below is
//! organisation only — isolation and timing are unchanged. This file is the
//! harness; the tests live beside it:
//!
//! - [`lifecycle`] — starting, replacing, stopping and cleaning up daemons
//! - [`send`] — the ingestion path and what `selx send` reports
//! - [`config`] — the config file and the environment that overrides it
//! - [`claude`] — the Claude IDE lock file and `selx claude`
//! - [`reload`] — `selx reload`'s Claude hook, and `selx claude` installing it

mod claude;
mod config;
mod lifecycle;
mod reload;
mod send;

use serde_json::Value;
use std::io::{BufRead, BufReader};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};
use tempfile::TempDir;

const SELX: &str = env!("CARGO_BIN_EXE_selx");

struct Env {
    /// Short path: unix socket paths are length-limited.
    state: TempDir,
    root: TempDir,
    /// Stands in for `~/.claude` so lock files never touch the real one.
    claude_config: TempDir,
}

impl Env {
    fn new() -> Env {
        let state = tempfile::Builder::new()
            .prefix("sx")
            .tempdir_in("/tmp")
            .unwrap();
        let root = tempfile::Builder::new()
            .prefix("selx-ws")
            .tempdir()
            .unwrap();
        let claude_config = tempfile::Builder::new()
            .prefix("selx-cc")
            .tempdir()
            .unwrap();
        Env {
            state,
            root,
            claude_config,
        }
    }

    fn lock_file(&self, port: u64) -> PathBuf {
        self.claude_config
            .path()
            .join("ide")
            .join(format!("{port}.lock"))
    }

    fn root(&self) -> PathBuf {
        std::fs::canonicalize(self.root.path()).unwrap()
    }

    /// This workspace's state directory, derived the way selx derives it
    /// rather than read back from a command — so it can be inspected
    /// before, during and after a daemon's life.
    fn state_dir(&self) -> PathBuf {
        self.state
            .path()
            .join(selx::core::workspace::root_hash(&self.root()))
    }

    /// The daemon's log for this workspace. Daemons for one root append to
    /// the same file, so this accumulates across a test.
    fn daemon_log(&self) -> String {
        std::fs::read_to_string(self.state_dir().join("daemon.log")).unwrap_or_default()
    }

    fn cmd(&self) -> Command {
        self.cmd_bin(Path::new(SELX))
    }

    fn cmd_bin(&self, bin: &Path) -> Command {
        let mut c = Command::new(bin);
        // Nothing selx reads is inherited: a developer with any of these
        // exported must get the run CI gets. Derived from the code's own
        // registry rather than restated, which is what stopped the two
        // drifting apart (`SELX_FOCUS` and `SELX_LOG` went unscrubbed for
        // several releases while the daemon read them).
        for k in selx::env::names() {
            c.env_remove(k);
        }
        c.env("SELX_STATE_DIR", self.state.path());
        c.env("CLAUDE_CONFIG_DIR", self.claude_config.path());
        // Explicitly disabled, so a developer's real config file never
        // affects tests (the daemon inherits this env).
        c.env("SELX_CONFIG", "");
        // `send` fills the event origin from the HERDR_* variables, and the
        // text-inject adapter resolves a socket from them: left inherited, a
        // suite run from inside a herdr pane would target the developer's own
        // live agents. Point them at nothing instead.
        c.env("HERDR_SOCKET_PATH", self.state.path().join("no-herdr.sock"));
        c.current_dir(self.root.path());
        c
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_bin(Path::new(SELX), args)
    }

    /// `selx …` through `bin`, retrying while the binary is still busy —
    /// see [`run_fresh_binary`].
    fn run_bin(&self, bin: &Path, args: &[&str]) -> Output {
        run_fresh_binary(|| {
            let mut c = self.cmd_bin(bin);
            c.args(args);
            c
        })
    }

    fn run_in(&self, dir: &Path, args: &[&str]) -> Output {
        let mut c = self.cmd();
        c.current_dir(dir);
        c.args(args);
        c.output().expect("run selx")
    }

    fn json(&self, args: &[&str]) -> Value {
        self.json_bin(Path::new(SELX), args)
    }

    /// A `--json` command through `bin`, which must succeed and must print
    /// JSON. The two assertions belong together: a command that failed
    /// prints nothing on stdout, and "expected value at line 1" would hide
    /// the error that actually explains it.
    #[track_caller]
    fn json_bin(&self, bin: &Path, args: &[&str]) -> Value {
        let out = succeeded(self.run_bin(bin, args));
        serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
            panic!(
                "bad json from {args:?}: {e}: {}",
                String::from_utf8_lossy(&out.stdout)
            )
        })
    }

    /// A stand-in for the `claude` binary that succeeds and does nothing.
    /// Written here rather than reaching for `/bin/true`, which exists on
    /// Linux but not on macOS — where only `/usr/bin/true` does.
    fn stub_claude(&self) -> PathBuf {
        let p = self.root().join("claude-stub");
        std::fs::write(&p, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    fn pid(&self) -> u32 {
        self.json(&["status", "--json"])["pid"].as_u64().unwrap() as u32
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = self.run(&["stop", "--all"]);
    }
}

/// Portable liveness check (Linux + macOS). The daemons are spawned by
/// `selx ensure`, never by this process, so they can't linger as zombies of
/// ours and `kill(pid, 0)` is a truthful answer.
fn alive(pid: u32) -> bool {
    selx::core::process::pid_alive(pid)
}

/// Assert a command succeeded, showing what it said when it did not. A
/// bare `status.success()` tells you nothing from a CI log — and errors
/// from `ensure` carry the daemon's own log tail.
#[track_caller]
fn succeeded(out: Output) -> Output {
    assert!(
        out.status.success(),
        "command failed ({}):\nstdout: {}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

/// Assert a command failed, showing what it printed when it did not. The
/// mirror of [`succeeded`]: `assert!(!out.status.success())` on its own
/// says nothing about a command that unexpectedly worked.
#[track_caller]
fn failed(out: Output) -> Output {
    assert!(
        !out.status.success(),
        "command unexpectedly succeeded:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

fn wait_until(timeout: Duration, mut f: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    f()
}

/// A copy of the built binary, so a test can replace it under a running
/// daemon without disturbing the one cargo built.
fn installed_copy(dir: &Path) -> PathBuf {
    let bin = dir.join("selx");
    std::fs::copy(SELX, &bin).unwrap();
    bin
}

/// Install over `bin` the way cargo does: write the new binary elsewhere
/// and rename it into place (a running program's own file cannot be
/// written to). One trailing byte is enough to change its identity.
fn install_over(bin: &Path) {
    let mut bytes = std::fs::read(bin).unwrap();
    bytes.push(b'\n');
    let staged = bin.with_extension("new");
    std::fs::write(&staged, &bytes).unwrap();
    std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::rename(&staged, bin).unwrap();
}

/// Run a binary this test just wrote. Any other thread that forks while
/// the file is open for writing holds that descriptor until it execs, and
/// our exec fails with ETXTBSY meanwhile — so retry instead of
/// serialising the whole suite around one copy.
fn run_fresh_binary(mut make: impl FnMut() -> Command) -> Output {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match make().output() {
            Ok(o) => return o,
            Err(e)
                if e.kind() == std::io::ErrorKind::ExecutableFileBusy
                    && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => panic!("running {e}"),
        }
    }
}

/// Enough of the IDE websocket handshake for the daemon to count this as a
/// connected agent. The returned stream must be kept alive.
fn connect_as_agent(port: u64, token: &str) -> std::net::TcpStream {
    use std::io::Write;
    let auth = selx::adapters::claude_ide::AUTH_HEADER;
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port as u16)).unwrap();
    write!(
        s,
        "GET / HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: Upgrade\r\n\
         Upgrade: websocket\r\nSec-WebSocket-Version: 13\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n{auth}: {token}\r\n\r\n"
    )
    .unwrap();
    let mut line = String::new();
    BufReader::new(s.try_clone().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert!(line.contains("101"), "handshake refused: {line}");
    s
}
