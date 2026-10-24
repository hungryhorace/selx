//! ClaudeIdeAdapter: the editor side of the Claude Code IDE protocol.
//!
//! Claude Code discovers "IDEs" through `~/.claude/ide/<port>.lock` and
//! connects to a WebSocket server on that port, authenticating with the
//! token from the lock file. Over the socket it speaks JSON-RPC 2.0 in the
//! shape of MCP; the IDE pushes `selection_changed` (ambient) and
//! `at_mentioned` (explicit) notifications. The protocol is reverse
//! engineered and observed, not guaranteed — see `docs/PROTOCOL-NOTES.md`.

pub mod lock;
pub mod payload;
pub mod rpc;

use crate::adapters::herdr::{self, HerdrClient, HerdrOrigin};
use crate::core::daemon::at_exit;
use crate::core::{Adapter, Delivery, EventKind, Origin, SelectionEvent};
use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use subtle::ConstantTimeEq;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::spawn_local;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, info, warn};

pub const ADAPTER_NAME: &str = "claude-ide";
pub const DEFAULT_IDE_NAME: &str = "selx";
/// Header Claude Code sends on the WebSocket upgrade request.
pub const AUTH_HEADER: &str = "x-claude-code-ide-authorization";
pub struct ClaudeIdeOptions {
    pub root: PathBuf,
    pub port: u16,
    /// Already bound to 127.0.0.1 by the daemon.
    pub listener: TcpListener,
    /// Directory holding `<port>.lock`; see [`lock::default_lock_dir`].
    pub lock_dir: PathBuf,
    pub ide_name: String,
    /// Log selected text (only ever at high verbosity).
    pub verbose: bool,
    /// herdr socket used to find Claude's pane. `None` resolves the default
    /// at every delivery, so a herdr started later is still found.
    pub herdr_socket: Option<PathBuf>,
}

pub struct ClaudeIdeAdapter {
    inner: Rc<Inner>,
}

pub(crate) struct Inner {
    pub root: PathBuf,
    pub port: u16,
    pub lock_path: PathBuf,
    auth_token: String,
    verbose: bool,
    clients: RefCell<HashMap<u64, mpsc::UnboundedSender<String>>>,
    next_client: Cell<u64>,
    /// Most recent selection, served by `getCurrentSelection` / `getLatestSelection`.
    pub latest: RefCell<Option<payload::Selection>>,
    herdr_socket: Option<PathBuf>,
    /// Panes that ran `selx claude` against this daemon, newest last. Never
    /// pruned here — liveness is decided against `agent.list` at use, so a
    /// Claude that quits simply stops being found.
    registered: RefCell<Vec<String>>,
}

impl ClaudeIdeAdapter {
    /// Write the lock file and start accepting Claude connections. Must be
    /// called inside a tokio `LocalSet`.
    pub fn start(opts: ClaudeIdeOptions) -> Result<Self> {
        let auth_token = uuid::Uuid::new_v4().to_string();
        let lock = lock::LockFile::new(opts.root.clone(), &opts.ide_name, auth_token.clone());
        let lock_path = lock::write(&opts.lock_dir, opts.port, &lock).with_context(|| {
            format!(
                "writing Claude IDE lock file in {}",
                opts.lock_dir.display()
            )
        })?;
        {
            let p = lock_path.clone();
            at_exit(move || {
                let _ = std::fs::remove_file(&p);
            });
        }
        let inner = Rc::new(Inner {
            root: opts.root,
            port: opts.port,
            lock_path: lock_path.clone(),
            auth_token,
            verbose: opts.verbose,
            clients: RefCell::new(HashMap::new()),
            next_client: Cell::new(1),
            latest: RefCell::new(None),
            herdr_socket: opts.herdr_socket,
            registered: RefCell::new(Vec::new()),
        });
        spawn_local(accept_loop(inner.clone(), opts.listener));
        info!(port = inner.port, lock = %lock_path.display(), "Claude IDE server ready");
        Ok(ClaudeIdeAdapter { inner })
    }

    pub fn lock_path(&self) -> &Path {
        &self.inner.lock_path
    }

    /// The token Claude must present. Exposed for tests.
    pub fn auth_token(&self) -> &str {
        &self.inner.auth_token
    }
}

impl Inner {
    /// The pane the Claude this event is for is sitting in, and the herdr
    /// that knows. Claude speaks no multiplexer, so this is inferred from
    /// herdr: a pane that registered itself through `selx claude`, else
    /// one herdr recognizes as Claude. `Err` says why there is none, for
    /// a delivery that cannot go ahead without a pane.
    fn locate(&self, event: &SelectionEvent) -> Result<(HerdrClient, String), String> {
        let Some(client) = HerdrClient::discover(self.herdr_socket.as_deref()) else {
            return Err("herdr is not running".into());
        };
        let agents = client
            .list_agents()
            .map_err(|e| format!("could not ask herdr where Claude is: {e:#}"))?;
        let registered = self.registered.borrow();
        match herdr::resolve_claude_pane(
            &agents,
            &registered,
            HerdrOrigin::read(event.origin.as_ref()),
        ) {
            Some(a) => Ok((client, a.pane_id.clone())),
            None => Err(format!(
                "no herdr pane looks like Claude ({} registered, {} agents)",
                registered.len(),
                agents.len()
            )),
        }
    }

    /// The pane for focus to follow a notification to. Every failure here
    /// is silent and costs only focus — the notification has been sent.
    fn pane_for(&self, event: &SelectionEvent) -> Option<String> {
        match self.locate(event) {
            Ok((_, pane)) => Some(pane),
            Err(why) => {
                debug!("{why}");
                None
            }
        }
    }

    /// A message goes by typing: the protocol's mention carries only a
    /// file and lines, so the sender's sentence is put in Claude's prompt
    /// the way text injection does for agents with no protocol. Nothing
    /// is sent over the socket. The daemon has already made the message
    /// one safe line.
    fn type_message(&self, event: &SelectionEvent, message: &str) -> Result<Delivery> {
        let (client, pane) = match self.locate(event) {
            Ok(found) => found,
            Err(why) => return Ok(Delivery::Dropped(format!("a message is typed, but {why}"))),
        };
        client
            .send_text(&pane, message)
            .with_context(|| format!("typing into Claude's pane {pane}"))?;
        info!(%pane, "typed a message from {}", event.source);
        Ok(Delivery::Delivered(Some(pane)))
    }
}

impl Adapter for ClaudeIdeAdapter {
    fn name(&self) -> &str {
        ADAPTER_NAME
    }

    fn supports_ambient(&self) -> bool {
        true
    }

    fn deliver(&self, event: &SelectionEvent) -> Result<Delivery> {
        if let Some(message) = &event.message {
            return self.inner.type_message(event, message);
        }
        let selection = payload::Selection::from_event(event);
        let message = match event.kind {
            EventKind::Ambient => rpc::notification("selection_changed", selection.to_value()),
            EventKind::Explicit => rpc::notification("at_mentioned", payload::at_mentioned(event)),
        };
        *self.inner.latest.borrow_mut() = Some(selection);

        let clients = self.inner.clients.borrow();
        if clients.is_empty() {
            return Ok(Delivery::Dropped("no Claude Code client connected".into()));
        }
        let sent = clients
            .values()
            .filter(|tx| tx.send(message.clone()).is_ok())
            .count();
        if self.inner.verbose {
            debug!(clients = sent, %message, "sent notification");
        }
        if sent == 0 {
            Ok(Delivery::Dropped(
                "all Claude Code clients are disconnecting".into(),
            ))
        } else {
            drop(clients);
            Ok(Delivery::Delivered(self.inner.pane_for(event)))
        }
    }

    fn register_origin(&self, origin: &Origin) {
        let Some(pane) = origin.pane.as_deref().filter(|_| origin.mux == herdr::MUX) else {
            return;
        };
        let mut registered = self.inner.registered.borrow_mut();
        if !registered.iter().any(|p| p == pane) {
            registered.push(pane.to_string());
            info!(pane, "Claude registered its pane");
        }
    }

    fn client_count(&self) -> usize {
        self.inner.clients.borrow().len()
    }

    fn shutdown(&self) {
        let _ = std::fs::remove_file(&self.inner.lock_path);
    }
}

async fn accept_loop(inner: Rc<Inner>, listener: TcpListener) {
    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                debug!(%peer, "incoming connection on IDE port");
                spawn_local(handle_client(inner.clone(), stream));
            }
            Err(e) => {
                warn!("accept on IDE port failed: {e}");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
    }
}

/// Validate the upgrade request: no browser `Origin`, and the auth header
/// must match the lock-file token (constant-time).
#[allow(clippy::result_large_err)] // the Err type is tungstenite's handshake callback contract
pub fn check_upgrade(request: &Request, expected_token: &str) -> Result<(), ErrorResponse> {
    fn reject(status: u16, msg: &str) -> ErrorResponse {
        let mut r = ErrorResponse::new(Some(msg.to_string()));
        *r.status_mut() =
            tokio_tungstenite::tungstenite::http::StatusCode::from_u16(status).unwrap();
        r
    }
    if request.headers().contains_key("origin") {
        return Err(reject(403, "browser origins are not allowed"));
    }
    let Some(presented) = request.headers().get(AUTH_HEADER) else {
        return Err(reject(401, "missing authorization"));
    };
    let presented = presented.as_bytes();
    let expected = expected_token.as_bytes();
    // Compare lengths in constant time too: pad to the longer length so a
    // wrong-length token takes the same path as a wrong-content one.
    let n = presented.len().max(expected.len());
    let mut a = vec![0u8; n];
    let mut b = vec![0u8; n];
    a[..presented.len()].copy_from_slice(presented);
    b[..expected.len()].copy_from_slice(expected);
    let same_len = presented.len() == expected.len();
    if bool::from(a.ct_eq(&b)) && same_len {
        Ok(())
    } else {
        Err(reject(401, "invalid authorization"))
    }
}

#[allow(clippy::result_large_err)] // see check_upgrade
async fn handle_client(inner: Rc<Inner>, stream: TcpStream) {
    let peer = stream
        .peer_addr()
        .map(|p| p.to_string())
        .unwrap_or_default();
    let token = inner.auth_token.clone();
    let ws =
        match tokio_tungstenite::accept_hdr_async(stream, |req: &Request, mut resp: Response| {
            if tracing::enabled!(tracing::Level::DEBUG) {
                let headers: Vec<String> = req
                    .headers()
                    .iter()
                    .map(|(k, v)| {
                        if k.as_str() == AUTH_HEADER {
                            format!("{k}: <redacted>")
                        } else {
                            format!("{k}: {}", v.to_str().unwrap_or("<binary>"))
                        }
                    })
                    .collect();
                debug!(?headers, "IDE upgrade request");
            }
            check_upgrade(req, &token)?;
            // Node's `ws` client aborts if it asked for a subprotocol and the
            // server did not pick one; echo the first one requested.
            if let Some(proto) = req.headers().get("sec-websocket-protocol") {
                if let Ok(first) = proto
                    .to_str()
                    .map(|s| s.split(',').next().unwrap_or("").trim().to_string())
                {
                    if let Ok(v) = first.parse() {
                        resp.headers_mut().insert("sec-websocket-protocol", v);
                    }
                }
            }
            Ok(resp)
        })
        .await
        {
            Ok(ws) => ws,
            Err(e) => {
                warn!(%peer, "rejected IDE connection: {e}");
                return;
            }
        };

    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    let id = inner.next_client.get();
    inner.next_client.set(id + 1);
    inner.clients.borrow_mut().insert(id, tx);
    info!(client = id, %peer, "Claude Code connected");

    let (mut sink, mut source) = ws.split();
    loop {
        tokio::select! {
            incoming = source.next() => match incoming {
                Some(Ok(Message::Text(text))) => {
                    if let Some(reply) = rpc::handle(&inner, id, text.as_str()) {
                        if sink.send(Message::text(reply)).await.is_err() {
                            break;
                        }
                    }
                }
                Some(Ok(Message::Close(_))) | None => break,
                Some(Ok(_)) => {}
                Some(Err(e)) => {
                    debug!(client = id, "connection error: {e}");
                    break;
                }
            },
            outgoing = rx.recv() => match outgoing {
                Some(text) => {
                    if sink.send(Message::text(text)).await.is_err() {
                        break;
                    }
                }
                None => break,
            },
        }
    }
    inner.clients.borrow_mut().remove(&id);
    info!(client = id, "Claude Code disconnected");
}
