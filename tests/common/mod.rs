//! Shared harness for the herdr integration tests: a real herdr server in
//! its own named session, torn down on drop. Never the default session — a
//! stray `server stop` there would kill the developer's live workspace.
//! Every test skips (with a message) when `herdr` is not installed, so CI
//! nodes without it stay green.
#![allow(dead_code)]

use selx::adapters::herdr::{HerdrClient, MUX};
use selx::core::{EventKind, Origin, SelectionEvent};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::http::Request as HttpRequest;
use tokio_tungstenite::WebSocketStream;

pub type Ws = WebSocketStream<TcpStream>;

/// A fake Claude Code client: the upgrade request Claude itself sends, with
/// the lock file's token in the authorization header.
pub async fn connect(port: u16, token: Option<&str>, origin: Option<&str>) -> Result<Ws, String> {
    let stream = TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port))
        .await
        .unwrap();
    // tungstenite's client expects a fully formed upgrade request.
    let mut b = HttpRequest::builder()
        .uri(format!("ws://127.0.0.1:{port}/"))
        .header("Host", format!("127.0.0.1:{port}"))
        .header("Connection", "Upgrade")
        .header("Upgrade", "websocket")
        .header("Sec-WebSocket-Version", "13")
        .header(
            "Sec-WebSocket-Key",
            tokio_tungstenite::tungstenite::handshake::client::generate_key(),
        );
    if let Some(t) = token {
        b = b.header(selx::adapters::claude_ide::AUTH_HEADER, t);
    }
    if let Some(o) = origin {
        b = b.header("Origin", o);
    }
    let req = b.body(()).unwrap();
    tokio_tungstenite::client_async(req, stream)
        .await
        .map(|(ws, _)| ws)
        .map_err(|e| e.to_string())
}

pub fn herdr_bin() -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|p| {
        std::env::split_paths(&p)
            .map(|d| d.join("herdr"))
            .find(|b| b.is_file())
    })
}

/// The panes a freshly created herdr workspace hands back.
pub struct Workspace {
    /// The workspace's root pane — where the editor sits in these tests.
    pub editor: String,
    pub tab: String,
    pub id: String,
}

/// A herdr id out of a JSON reply. Every one of these is a required field
/// in herdr's schema, so a missing one is a broken harness, not a case.
#[track_caller]
fn id(v: &Value) -> String {
    v.as_str()
        .unwrap_or_else(|| panic!("expected a herdr id, got {v}"))
        .to_string()
}

pub struct Session {
    pub name: String,
    pub socket: PathBuf,
    pub server_pid: u32,
    pub bin: PathBuf,
}

impl Session {
    /// `tag` keeps concurrently running tests in separate herdr sessions.
    pub fn start(bin: PathBuf, tag: &str) -> Session {
        let name = format!("selx-test-{}-{tag}", std::process::id());
        let log = std::env::temp_dir().join(format!("{name}.log"));
        // Detach with setsid so herdr fully daemonizes; a plain child leaves
        // the api socket un-accepted and connect() would block forever.
        let mut cmd = Command::new(&bin);
        cmd.arg("server").env("HERDR_SESSION", &name);
        let pid = selx::core::process::spawn_detached(cmd, &log)
            .expect("spawn herdr server")
            .id();
        let socket = selx::adapters::herdr::herdr_config_dir()
            .join("sessions")
            .join(&name)
            .join("herdr.sock");
        let client = HerdrClient::new(socket.clone(), Duration::from_secs(2));
        let deadline = Instant::now() + Duration::from_secs(15);
        while client.ping().is_err() {
            assert!(
                Instant::now() < deadline,
                "herdr server did not come up at {}; log:\n{}",
                socket.display(),
                std::fs::read_to_string(&log).unwrap_or_default()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        Session {
            name,
            socket,
            server_pid: pid,
            bin: bin.clone(),
        }
    }

    pub fn cli(&self, args: &[&str]) -> Value {
        let out = Command::new(&self.bin)
            .args(args)
            .env("HERDR_SESSION", &self.name)
            .output()
            .expect("run herdr");
        assert!(
            out.status.success(),
            "herdr {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap_or(Value::Null)
    }

    /// A fresh herdr workspace whose root pane sits in `cwd`.
    pub fn create_workspace(&self, cwd: &Path) -> Workspace {
        let cwd = cwd.to_str().expect("utf-8 cwd");
        let v = self.cli(&["workspace", "create", "--cwd", cwd]);
        Workspace {
            editor: id(&v["result"]["root_pane"]["pane_id"]),
            tab: id(&v["result"]["root_pane"]["tab_id"]),
            id: id(&v["result"]["workspace"]["workspace_id"]),
        }
    }

    /// A second tab in `workspace`; returns its root pane.
    pub fn create_tab(&self, workspace: &str, cwd: &Path) -> String {
        let cwd = cwd.to_str().expect("utf-8 cwd");
        id(
            &self.cli(&["tab", "create", "--workspace", workspace, "--cwd", cwd])["result"]
                ["root_pane"]["pane_id"],
        )
    }

    /// Split `pane` (`"right"`, `"down"`, …); returns the new pane.
    pub fn split(&self, pane: &str, direction: &str, cwd: &Path) -> String {
        let cwd = cwd.to_str().expect("utf-8 cwd");
        id(&self.cli(&[
            "pane",
            "split",
            pane,
            "--direction",
            direction,
            "--cwd",
            cwd,
        ])["result"]["pane"]["pane_id"])
    }

    /// Tell herdr an agent of `kind` is in `pane`, in state `state`
    /// (`"idle"`, `"blocked"`, …). Reporting is how these tests conjure
    /// agents without installing codex or claude.
    ///
    /// `seq` pins herdr's `state_change_seq`, which orders "most recently
    /// active"; `None` lets herdr assign the next one, which is what makes
    /// report order the recency order.
    pub fn report_agent(&self, pane: &str, kind: &str, state: &str, seq: Option<u64>) {
        let seq = seq.map(|n| n.to_string());
        let mut args = vec![
            "pane",
            "report-agent",
            pane,
            "--source",
            "selx-test",
            "--agent",
            kind,
            "--state",
            state,
        ];
        if let Some(s) = &seq {
            args.extend(["--seq", s]);
        }
        self.cli(&args);
    }

    /// Wait for `needle` to appear in the pane's visible text. Returns the
    /// text that matched; panics with what the pane held instead.
    #[track_caller]
    pub fn wait_for_text(&self, pane: &str, needle: &str, timeout: Duration) -> String {
        let deadline = Instant::now() + timeout;
        loop {
            let text = self.pane_text(pane);
            if text.contains(needle) {
                return text;
            }
            assert!(
                Instant::now() < deadline,
                "pane {pane} never showed {needle:?}:\n{text}"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Wait until the pane's shell really sits in `cwd`: a shell rc file may
    /// `cd` elsewhere after herdr starts it.
    #[track_caller]
    pub fn settle_cwd(&self, pane: &str, cwd: &Path) {
        let want = cwd.to_str().expect("utf-8 cwd");
        self.cli(&["pane", "run", pane, &format!("cd {want}")]);
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.pane_cwd(pane).as_deref() != Some(want) {
            assert!(
                Instant::now() < deadline,
                "pane never reached {want}: {:?}",
                self.pane_cwd(pane)
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    pub fn pane_text(&self, pane: &str) -> String {
        let out = Command::new(&self.bin)
            .args(["pane", "read", pane, "--source", "visible"])
            .env("HERDR_SESSION", &self.name)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).to_string()
    }

    /// Does this pane currently hold herdr's focus?
    pub fn pane_focused(&self, pane: &str) -> bool {
        self.cli(&["pane", "get", pane])["result"]["pane"]["focused"]
            .as_bool()
            .unwrap_or(false)
    }

    pub fn pane_cwd(&self, pane: &str) -> Option<String> {
        let v = self.cli(&["pane", "get", pane]);
        v["result"]["pane"]["foreground_cwd"]
            .as_str()
            .or(v["result"]["pane"]["cwd"].as_str())
            .map(String::from)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = Command::new(&self.bin)
            .args(["server", "stop"])
            .env("HERDR_SESSION", &self.name)
            .output();
        if !selx::core::process::wait_gone(self.server_pid, Duration::from_secs(5)) {
            selx::core::process::terminate(self.server_pid, Duration::from_millis(500));
        }
        let _ = Command::new(&self.bin)
            .args(["session", "delete", &self.name])
            .env("HERDR_SESSION", &self.name)
            .output();
    }
}

/// A send from a pane at the given herdr coordinates, as `selx send` would
/// build it from the editor's environment.
pub fn event_from(
    root: &std::path::Path,
    workspace: &str,
    tab: &str,
    pane: &str,
) -> SelectionEvent {
    event(root, EventKind::Explicit).with_origin(Origin {
        mux: MUX.into(),
        session: None,
        workspace: Some(workspace.into()),
        tab: Some(tab.into()),
        pane: Some(pane.into()),
    })
}

pub fn event(root: &std::path::Path, kind: EventKind) -> SelectionEvent {
    SelectionEvent::new(root, root.join("src/capture.rs"), kind)
        .with_lines(40, 62)
        .with_source("helix")
}

/// Run an async test body on a current-thread runtime inside a `LocalSet`:
/// the adapters are `!Send` by design, so they cannot be spawned anywhere
/// else. Mirrors how `core`'s daemon runs them.
pub fn run_local<F: std::future::Future<Output = ()>>(f: F) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    tokio::task::LocalSet::new().block_on(&rt, f);
}
