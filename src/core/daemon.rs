//! The daemon: holds the port, serves the ingestion socket, fans events out
//! to adapters, and cleans up after itself on every exit path.

use crate::core::adapter::{should_follow, Adapter, FocusMode, Focuser, RouteOutcome, Router};
use crate::core::build_id;
use crate::core::client;
use crate::core::event::{Origin, SelectionEvent};
use crate::core::protocol::*;
use crate::core::state::StateDir;
use anyhow::{Context, Result};
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, UnixListener, UnixStream};
use tokio::sync::{broadcast, watch};
use tokio::task::{spawn_local, LocalSet};
use tracing::{debug, error, info, warn};

pub use crate::core::build_id::VERSION;
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(4 * 60 * 60);

#[derive(Debug, Clone)]
pub struct DaemonConfig {
    pub root: PathBuf,
    /// First port to try; the daemon probes upward deterministically from here.
    pub preferred_port: u16,
    /// Exit after this long with no connected agent clients and no ingestion
    /// traffic. Zero disables idle shutdown.
    pub idle_timeout: Duration,
    /// Include selected text in status / follow output and adapters' logs.
    pub verbose: bool,
    /// When a delivery moves the user's focus to the pane that received it.
    pub focus: FocusMode,
    pub recent_capacity: usize,
    pub port_probe_limit: u32,
}

impl DaemonConfig {
    pub fn new(root: PathBuf, preferred_port: u16) -> Self {
        DaemonConfig {
            root,
            preferred_port,
            idle_timeout: DEFAULT_IDLE_TIMEOUT,
            verbose: false,
            focus: FocusMode::default(),
            recent_capacity: 50,
            port_probe_limit: 256,
        }
    }
}

// ---- exit cleanup registry ------------------------------------------------

type Cleanup = Box<dyn FnOnce() + Send + 'static>;
static AT_EXIT: Mutex<Vec<Cleanup>> = Mutex::new(Vec::new());
static PANIC_HOOK_INSTALLED: std::sync::Once = std::sync::Once::new();

/// Register a cleanup action to run when the daemon exits by any path
/// (normal shutdown, signal, or panic). Adapters use this for anything they
/// own outside the state dir. Actions run at most once, LIFO.
pub fn at_exit(f: impl FnOnce() + Send + 'static) {
    AT_EXIT
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(Box::new(f));
}

fn run_exit_cleanups() {
    let actions: Vec<Cleanup> =
        std::mem::take(&mut *AT_EXIT.lock().unwrap_or_else(|e| e.into_inner()));
    for a in actions.into_iter().rev() {
        a();
    }
}

fn install_panic_hook() {
    PANIC_HOOK_INSTALLED.call_once(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            prev(info);
            error!("daemon panicked; cleaning up and exiting");
            run_exit_cleanups();
            std::process::exit(101);
        }));
    });
}

// ---- daemon ---------------------------------------------------------------

struct Shared {
    cfg: DaemonConfig,
    state: StateDir,
    router: Router,
    focuser: Option<Box<dyn Focuser>>,
    port: u16,
    pid: u32,
    /// Identity of the binary this daemon is running, taken at startup:
    /// installing a new selx replaces that file, and a process cannot
    /// identify a file that is no longer there.
    build: String,
    started: Instant,
    last_activity: Cell<Instant>,
    events_received: Cell<u64>,
    recent: RefCell<VecDeque<EventRecord>>,
    follow_tx: broadcast::Sender<EventRecord>,
    shutdown_tx: watch::Sender<Option<String>>,
}

impl Shared {
    /// Focus follows delivery (never the keypress): an event no pane
    /// received leaves the user exactly where they were, which is what
    /// makes a dropped ambient send harmless. Exactly one focus call per
    /// event happens here rather than inside the adapters, so two of them
    /// cannot race to focus different panes.
    fn focus_after(&self, event: &SelectionEvent, outcome: &RouteOutcome) -> Option<String> {
        if !should_follow(self.cfg.focus, event, &outcome.panes) {
            return None;
        }
        match self.focuser.as_ref()?.focus(&outcome.panes, event) {
            Ok(pane) => {
                if let Some(p) = &pane {
                    debug!(pane = %p, "focus moved");
                }
                pane
            }
            Err(e) => {
                warn!("could not move focus: {e:#}");
                None
            }
        }
    }

    /// Everything the daemon decides about one send: validate it, route
    /// it, move focus, log it, record it and broadcast it. Pulled out of
    /// `handle_conn` so the decisions are reachable without a socket —
    /// `handle_conn` is left moving bytes.
    ///
    /// `Err` is the response to write back, not a failure of the daemon.
    fn handle_send(&self, event: SelectionEvent) -> Result<SendResponse, ErrorResponse> {
        let event = event.sanitized();
        if let Err(e) = event.validate() {
            return Err(ErrorResponse::new(format!("invalid event: {e}")));
        }
        if event.workspace_root != self.state.root {
            return Err(ErrorResponse::new(format!(
                "event is for workspace {} but this daemon serves {}",
                event.workspace_root.display(),
                self.state.root.display()
            )));
        }
        self.touch();
        self.events_received.set(self.events_received.get() + 1);
        let mut outcome = self.router.route(&event);
        outcome.focused = self.focus_after(&event, &outcome);
        info!(
            kind = event.kind.as_str(),
            source = %event.source,
            range = %event.display_range(),
            delivered = ?outcome.delivered,
            focused = outcome.focused.as_deref().unwrap_or("-"),
            dropped = outcome.dropped.len(),
            failed = outcome.failed.len(),
            "event routed"
        );
        for f in &outcome.failed {
            warn!(adapter = %f.adapter, "delivery failed: {}", f.error);
        }
        if outcome.agents.is_empty() {
            warn!("no agent received {}", event.display_range());
        }
        // Selected text leaves this process only at -vv: the recent list,
        // the follow stream and the adapters' logs all see the redacted
        // copy otherwise.
        let shown = if self.cfg.verbose {
            event
        } else {
            event.redacted()
        };
        let record = EventRecord {
            ts: crate::core::unix_now(),
            event: shown,
            outcome: outcome.clone(),
        };
        {
            let mut recent = self.recent.borrow_mut();
            recent.push_back(record.clone());
            while recent.len() > self.cfg.recent_capacity {
                recent.pop_front();
            }
        }
        let _ = self.follow_tx.send(record);
        Ok(SendResponse { ok: true, outcome })
    }

    /// A client naming where it runs, before it connects as an agent.
    fn register_origin(&self, origin: Origin) -> Result<OkResponse, ErrorResponse> {
        if origin.mux.is_empty() {
            return Err(ErrorResponse::new("origin.mux must name a multiplexer"));
        }
        self.touch();
        info!(
            mux = %origin.mux,
            pane = origin.pane.as_deref().unwrap_or("-"),
            "registered client origin"
        );
        self.router.register_origin(&origin);
        Ok(OkResponse { ok: true })
    }

    fn touch(&self) {
        self.last_activity.set(Instant::now());
    }
    fn shutdown(&self, reason: &str) {
        self.shutdown_tx.send_if_modified(|v| {
            if v.is_none() {
                *v = Some(reason.to_string());
                true
            } else {
                false
            }
        });
    }
    fn ping_response(&self) -> PingResponse {
        PingResponse {
            ok: true,
            pid: self.pid,
            port: self.port,
            root: self.state.root.clone(),
            version: VERSION.into(),
            build: self.build.clone(),
            clients: Some(self.router.client_count()),
        }
    }
    fn status_response(&self) -> StatusResponse {
        StatusResponse {
            ok: true,
            pid: self.pid,
            port: self.port,
            root: self.state.root.clone(),
            version: VERSION.into(),
            build: self.build.clone(),
            uptime_secs: self.started.elapsed().as_secs(),
            idle_secs: self.last_activity.get().elapsed().as_secs(),
            idle_timeout_secs: self.cfg.idle_timeout.as_secs(),
            events_received: self.events_received.get(),
            clients: self.router.client_count(),
            adapters: self.router.infos(),
            recent: self.recent.borrow().iter().cloned().collect(),
        }
    }
}

/// What the daemon has bound, offered to the adapter factory. An adapter
/// that speaks to agents over TCP takes `listener`; if nobody takes it the
/// daemon keeps holding the port so the recorded port stays reserved.
pub struct BindInfo {
    pub root: PathBuf,
    pub port: u16,
    pub state: StateDir,
    pub verbose: bool,
    pub listener: Option<TcpListener>,
    /// Set by the factory to move focus to a pane that received an event.
    /// Left `None`, deliveries never move focus.
    pub focuser: Option<Box<dyn Focuser>>,
}

/// Builds the adapter set once the daemon has bound its port. Runs inside
/// the daemon's [`LocalSet`], so adapters may `spawn_local` tasks.
pub type AdapterFactory = Box<dyn FnOnce(&mut BindInfo) -> Result<Vec<Box<dyn Adapter>>>>;

/// Build a current-thread runtime and run the daemon to completion.
pub fn run_blocking(cfg: DaemonConfig, factory: AdapterFactory) -> Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let local = LocalSet::new();
    local.block_on(&rt, run(cfg, factory))
}

/// Run the daemon. Must be called inside a [`LocalSet`].
pub async fn run(cfg: DaemonConfig, factory: AdapterFactory) -> Result<()> {
    let state = StateDir::for_root(&cfg.root);
    state.create()?;

    if let Some(p) = client::try_ping(&state) {
        anyhow::bail!(
            "a selx daemon for {} is already running (pid {}, port {})",
            state.root.display(),
            p.pid,
            p.port
        );
    }

    let (tcp, port) = bind_port(&cfg).await?;
    let sock = state.socket_path();
    let _ = std::fs::remove_file(&sock);
    let listener =
        UnixListener::bind(&sock).with_context(|| format!("binding {}", sock.display()))?;
    std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600))?;

    let pid = std::process::id();
    state.write_root()?;
    state.write_port(port)?;
    state.write_pid(pid)?;

    // From here on, every exit path must remove the runtime files.
    install_panic_hook();
    {
        let s = state.clone();
        at_exit(move || s.remove_runtime_files());
    }

    let mut bind = BindInfo {
        root: cfg.root.clone(),
        port,
        state: state.clone(),
        verbose: cfg.verbose,
        listener: Some(tcp),
        focuser: None,
    };
    let adapters = match factory(&mut bind) {
        Ok(a) => a,
        Err(e) => {
            run_exit_cleanups();
            return Err(e.context("starting adapters"));
        }
    };

    let (shutdown_tx, mut shutdown_rx) = watch::channel(None);
    let (follow_tx, _) = broadcast::channel(256);
    let shared = Rc::new(Shared {
        state: state.clone(),
        router: Router::new(adapters),
        focuser: bind.focuser.take(),
        port,
        pid,
        build: build_id::current(),
        started: Instant::now(),
        last_activity: Cell::new(Instant::now()),
        events_received: Cell::new(0),
        recent: RefCell::new(VecDeque::new()),
        follow_tx,
        shutdown_tx,
        cfg,
    });

    info!(
        root = %state.root.display(),
        port,
        pid,
        socket = %sock.display(),
        idle_timeout_secs = shared.cfg.idle_timeout.as_secs(),
        "selx daemon {VERSION} ready"
    );

    if let Some(tcp) = bind.listener.take() {
        spawn_local(hold_port(tcp));
    }
    spawn_local(watch_signals(shared.clone()));
    spawn_local(watch_idle(shared.clone()));

    loop {
        tokio::select! {
            r = listener.accept() => match r {
                Ok((stream, _)) => { spawn_local(handle_conn(shared.clone(), stream)); }
                Err(e) => { warn!("accept failed: {e}"); tokio::time::sleep(Duration::from_millis(50)).await; }
            },
            _ = shutdown_rx.changed() => break,
        }
    }

    let reason = shutdown_rx.borrow().clone().unwrap_or_default();
    info!("shutting down: {reason}");
    shared.router.shutdown();
    drop(listener);
    run_exit_cleanups();
    Ok(())
}

/// Bind the first free port in the root's deterministic sequence.
async fn bind_port(cfg: &DaemonConfig) -> Result<(TcpListener, u16)> {
    let ports = crate::core::workspace::port_sequence_from(cfg.preferred_port);
    for (n, port) in ports.take(cfg.port_probe_limit as usize).enumerate() {
        match TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await {
            Ok(l) => {
                if n > 0 {
                    info!("preferred port {} busy; using {}", cfg.preferred_port, port);
                }
                return Ok((l, port));
            }
            Err(e) => debug!("port {port} unavailable: {e}"),
        }
    }
    anyhow::bail!(
        "no free port found after {} probes from {}",
        cfg.port_probe_limit,
        cfg.preferred_port
    )
}

async fn hold_port(tcp: TcpListener) {
    // No adapter took the port: accept and drop so the backlog never fills.
    loop {
        match tcp.accept().await {
            Ok((_s, peer)) => debug!("dropped connection on agent port from {peer}"),
            Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
}

async fn watch_signals(shared: Rc<Shared>) {
    use tokio::signal::unix::{signal, SignalKind};
    let (Ok(mut term), Ok(mut int), Ok(mut hup)) = (
        signal(SignalKind::terminate()),
        signal(SignalKind::interrupt()),
        signal(SignalKind::hangup()),
    ) else {
        warn!("could not install signal handlers");
        return;
    };
    let which = tokio::select! {
        _ = term.recv() => "SIGTERM",
        _ = int.recv() => "SIGINT",
        _ = hup.recv() => "SIGHUP",
    };
    shared.shutdown(which);
}

async fn watch_idle(shared: Rc<Shared>) {
    let timeout = shared.cfg.idle_timeout;
    if timeout.is_zero() {
        return;
    }
    let tick = (timeout / 4).clamp(Duration::from_millis(200), Duration::from_secs(30));
    let mut interval = tokio::time::interval(tick);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        let idle = shared.last_activity.get().elapsed();
        if idle >= timeout && shared.router.client_count() == 0 {
            shared.shutdown(&format!("idle for {}s with no clients", idle.as_secs()));
            return;
        }
    }
}

/// One response, as the line that goes on the wire.
///
/// Serialization can only fail on a path that is not valid UTF-8; the
/// client gets an error line rather than a silently dropped connection.
fn encode<T: serde::Serialize>(v: &T) -> String {
    let mut s = serde_json::to_string(v).unwrap_or_else(|e| {
        error!("could not serialize a response: {e}");
        // A struct of two plain fields: this one cannot fail.
        serde_json::to_string(&ErrorResponse::new("response could not be serialized"))
            .unwrap_or_else(|_| r#"{"ok":false,"error":"internal error"}"#.to_string())
    });
    s.push('\n');
    s
}

async fn write_line<W: AsyncWriteExt + Unpin>(w: &mut W, line: &str) -> std::io::Result<()> {
    w.write_all(line.as_bytes()).await?;
    w.flush().await
}

async fn handle_conn(shared: Rc<Shared>, stream: UnixStream) {
    let (r, mut w) = stream.into_split();
    let mut lines = BufReader::new(r).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }
        // Every request but `follow` and `stop` answers with exactly one
        // line, so they share the write below — and with it the one rule
        // that matters here: a client that has gone away ends the
        // connection rather than the daemon.
        let response = match serde_json::from_str::<Request>(&line) {
            Err(e) => encode(&ErrorResponse::new(format!("bad request: {e}"))),
            Ok(Request::Ping) => encode(&shared.ping_response()),
            Ok(Request::Status) => encode(&shared.status_response()),
            Ok(Request::Send { event }) => match shared.handle_send(*event) {
                Ok(resp) => encode(&resp),
                Err(e) => encode(&e),
            },
            Ok(Request::RegisterOrigin { origin }) => match shared.register_origin(origin) {
                Ok(resp) => encode(&resp),
                Err(e) => encode(&e),
            },
            Ok(Request::Follow) => return follow(shared, w).await,
            Ok(Request::Stop) => {
                let _ = write_line(&mut w, &encode(&OkResponse { ok: true })).await;
                shared.shutdown("stop requested");
                return;
            }
        };
        if write_line(&mut w, &response).await.is_err() {
            return;
        }
    }
}

/// Stream events to a `selx status --follow` client until it goes away or
/// the daemon shuts down.
async fn follow<W: AsyncWriteExt + Unpin>(shared: Rc<Shared>, mut w: W) {
    let mut rx = shared.follow_tx.subscribe();
    let mut shutdown = shared.shutdown_tx.subscribe();
    if write_line(&mut w, &encode(&OkResponse { ok: true }))
        .await
        .is_err()
    {
        return;
    }
    loop {
        tokio::select! {
            f = rx.recv() => match f {
                Ok(frame) => {
                    if write_line(&mut w, &encode(&frame)).await.is_err() { return; }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => warn!("follow client lagged by {n} events"),
                Err(broadcast::error::RecvError::Closed) => return,
            },
            _ = shutdown.changed() => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::adapter::testing::{Fake, FakeFocuser};
    use crate::core::adapter::Delivery;
    use crate::core::event::EventKind;
    use std::cell::RefCell;

    /// A `Shared` with no socket, no port and no runtime: everything
    /// `handle_send` decides is reachable from here.
    fn shared(adapters: Vec<Box<dyn Adapter>>, focuser: Option<Box<dyn Focuser>>) -> Shared {
        let root = PathBuf::from("/ws");
        let mut cfg = DaemonConfig::new(root.clone(), 10000);
        cfg.recent_capacity = 3;
        let (shutdown_tx, _) = watch::channel(None);
        let (follow_tx, _) = broadcast::channel(16);
        Shared {
            state: StateDir {
                root: root.clone(),
                dir: PathBuf::from("/nonexistent"),
            },
            router: Router::new(adapters),
            focuser,
            port: 10000,
            pid: 1,
            build: "test".into(),
            started: Instant::now(),
            last_activity: Cell::new(Instant::now()),
            events_received: Cell::new(0),
            recent: RefCell::new(VecDeque::new()),
            follow_tx,
            shutdown_tx,
            cfg,
        }
    }

    fn ev(kind: EventKind) -> SelectionEvent {
        SelectionEvent::new("/ws", "/ws/a.rs", kind)
            .with_lines(1, 2)
            .with_text("SECRET")
            .with_source("test")
    }

    fn from_pane(kind: EventKind) -> SelectionEvent {
        ev(kind).with_origin(Origin {
            mux: "herdr".into(),
            session: None,
            workspace: Some("w1".into()),
            tab: Some("w1:t1".into()),
            pane: Some("w1:p1".into()),
        })
    }

    #[test]
    fn a_malformed_or_foreign_event_is_refused_without_routing() {
        let seen: Rc<RefCell<Vec<SelectionEvent>>> = Default::default();
        let s = shared(
            vec![Box::new(Fake {
                seen: seen.clone(),
                ..Fake::default()
            })],
            None,
        );

        let mut bad = ev(EventKind::Explicit);
        bad.source = String::new();
        let e = s.handle_send(bad).unwrap_err();
        assert!(e.error.contains("invalid event"), "{}", e.error);

        // A daemon serves exactly one root; another root's event is a
        // routing mistake, not something to deliver anyway.
        let elsewhere = SelectionEvent::new("/other", "/other/a.rs", EventKind::Explicit);
        let e = s.handle_send(elsewhere).unwrap_err();
        assert!(e.error.contains("this daemon serves /ws"), "{}", e.error);

        assert!(
            seen.borrow().is_empty(),
            "a refused event reached an adapter"
        );
        assert_eq!(s.events_received.get(), 0);
        assert!(s.recent.borrow().is_empty());
    }

    #[test]
    fn selected_text_is_kept_out_of_recent_and_follow_unless_verbose() {
        let s = shared(vec![Box::new(Fake::default())], None);
        let mut follow = s.follow_tx.subscribe();
        s.handle_send(ev(EventKind::Explicit)).unwrap();
        assert_eq!(s.recent.borrow()[0].event.text, None);
        assert_eq!(follow.try_recv().unwrap().event.text, None);

        let mut s = shared(vec![Box::new(Fake::default())], None);
        s.cfg.verbose = true;
        let mut follow = s.follow_tx.subscribe();
        s.handle_send(ev(EventKind::Explicit)).unwrap();
        assert_eq!(s.recent.borrow()[0].event.text.as_deref(), Some("SECRET"));
        assert_eq!(
            follow.try_recv().unwrap().event.text.as_deref(),
            Some("SECRET")
        );
    }

    #[test]
    fn recent_keeps_the_newest_events_up_to_capacity() {
        let s = shared(vec![Box::new(Fake::default())], None);
        for i in 1..=5u32 {
            s.handle_send(ev(EventKind::Ambient).with_lines(i, i))
                .unwrap();
        }
        assert_eq!(s.events_received.get(), 5);
        let recent = s.recent.borrow();
        assert_eq!(recent.len(), s.cfg.recent_capacity);
        let starts: Vec<u32> = recent
            .iter()
            .map(|r| r.event.lines.unwrap().start)
            .collect();
        assert_eq!(
            starts,
            vec![3, 4, 5],
            "oldest events are dropped, not newest"
        );
    }

    #[test]
    fn a_recorder_alone_is_reported_as_nobody_told() {
        // The log adapter accepts everything; the response must still show
        // that no agent heard it, which is what `send` warns about.
        let s = shared(
            vec![Box::new(Fake {
                name: "log",
                agent: false,
                ..Fake::default()
            })],
            None,
        );
        let resp = s.handle_send(ev(EventKind::Ambient)).unwrap();
        assert_eq!(resp.outcome.delivered, vec!["log"]);
        assert!(resp.outcome.agents.is_empty());
        assert!(resp.outcome.failed.is_empty());
    }

    #[test]
    fn a_failing_adapter_is_recorded_and_the_send_still_answers() {
        let s = shared(
            vec![
                Box::new(Fake {
                    name: "bad",
                    fail: true,
                    ..Fake::default()
                }),
                Box::new(Fake {
                    name: "good",
                    ..Fake::default()
                }),
            ],
            None,
        );
        let resp = s.handle_send(ev(EventKind::Explicit)).unwrap();
        assert_eq!(resp.outcome.delivered, vec!["good"]);
        assert_eq!(resp.outcome.failed[0].adapter, "bad");
        assert_eq!(s.events_received.get(), 1);
    }

    #[test]
    fn focus_moves_only_when_the_policy_says_so() {
        let adapters = || -> Vec<Box<dyn Adapter>> {
            vec![Box::new(Fake {
                pane: Some("w1:p2"),
                ..Fake::default()
            })]
        };

        // Delivered to a pane, from a pane, mode `delivered`: focus moves,
        // and the focuser is offered exactly the panes that received it.
        let f = FakeFocuser::default();
        let calls = f.calls.clone();
        let s = shared(adapters(), Some(Box::new(f)));
        let resp = s.handle_send(from_pane(EventKind::Ambient)).unwrap();
        assert_eq!(resp.outcome.focused.as_deref(), Some("w1:p2"));
        assert_eq!(*calls.borrow(), vec![vec!["w1:p2".to_string()]]);

        // Same delivery, but the sender named no pane: nothing to move
        // from, so nothing moves — and the focuser is never even asked.
        let f = FakeFocuser::default();
        let calls = f.calls.clone();
        let s = shared(adapters(), Some(Box::new(f)));
        let resp = s.handle_send(ev(EventKind::Ambient)).unwrap();
        assert_eq!(resp.outcome.focused, None);
        assert!(
            calls.borrow().is_empty(),
            "asked a focuser it need not have"
        );

        // Mode `explicit` ignores ambient events.
        let mut s = shared(adapters(), Some(Box::<FakeFocuser>::default()));
        s.cfg.focus = FocusMode::Explicit;
        assert_eq!(
            s.handle_send(from_pane(EventKind::Ambient))
                .unwrap()
                .outcome
                .focused,
            None
        );
        assert_eq!(
            s.handle_send(from_pane(EventKind::Explicit))
                .unwrap()
                .outcome
                .focused
                .as_deref(),
            Some("w1:p2")
        );

        // A focuser that refuses, and one that fails, both leave the user
        // put — and neither turns the send into an error.
        let s = shared(
            adapters(),
            Some(Box::new(FakeFocuser {
                refuse: true,
                ..FakeFocuser::default()
            })),
        );
        assert_eq!(
            s.handle_send(from_pane(EventKind::Explicit))
                .unwrap()
                .outcome
                .focused,
            None
        );
        let s = shared(
            adapters(),
            Some(Box::new(FakeFocuser {
                fail: true,
                ..FakeFocuser::default()
            })),
        );
        let resp = s.handle_send(from_pane(EventKind::Explicit)).unwrap();
        assert_eq!(resp.outcome.focused, None);
        assert_eq!(resp.outcome.delivered, vec!["fake"]);
    }

    #[test]
    fn nothing_reached_a_pane_so_the_focuser_is_never_asked() {
        let focuser = FakeFocuser::default();
        let calls = focuser.calls.clone();
        let s = shared(
            vec![Box::new(Fake {
                drop_all: true,
                ..Fake::default()
            })],
            Some(Box::new(focuser)),
        );
        let resp = s.handle_send(from_pane(EventKind::Explicit)).unwrap();
        assert_eq!(resp.outcome.focused, None);
        assert!(resp.outcome.panes.is_empty());
        assert!(calls.borrow().is_empty(), "a dropped send asked for focus");
    }

    #[test]
    fn every_send_reaches_every_follower() {
        let s = shared(vec![Box::new(Fake::default())], None);
        let mut a = s.follow_tx.subscribe();
        let mut b = s.follow_tx.subscribe();
        s.handle_send(ev(EventKind::Explicit)).unwrap();
        let fa = a.try_recv().unwrap();
        let fb = b.try_recv().unwrap();
        assert_eq!(fa, fb);
        assert_eq!(fa.outcome.delivered, vec!["fake"]);
        assert_eq!(fa.ts, s.recent.borrow()[0].ts);
        // A follower that arrives afterwards sees only what comes next.
        let mut late = s.follow_tx.subscribe();
        assert!(late.try_recv().is_err());
    }

    #[test]
    fn polling_a_daemon_does_not_keep_it_alive() {
        // `selx status` in a loop must not defeat the idle timeout: only
        // real traffic counts as activity.
        let s = shared(vec![Box::new(Fake::default())], None);
        let idle_since = s.last_activity.get();
        assert_eq!(s.ping_response().root, s.state.root);
        assert_eq!(s.status_response().events_received, 0);
        assert_eq!(
            s.last_activity.get(),
            idle_since,
            "a poll counted as activity"
        );

        s.handle_send(ev(EventKind::Ambient)).unwrap();
        assert!(s.last_activity.get() > idle_since, "a send did not count");
    }

    #[test]
    fn a_registered_origin_reaches_the_adapters_and_counts_as_activity() {
        let s = shared(vec![Box::new(Fake::default())], None);
        let idle_since = s.last_activity.get();
        let e = s
            .register_origin(Origin {
                mux: String::new(),
                session: None,
                workspace: None,
                tab: None,
                pane: None,
            })
            .unwrap_err();
        assert!(e.error.contains("must name a multiplexer"), "{}", e.error);
        assert_eq!(s.last_activity.get(), idle_since, "a refusal counted");

        s.register_origin(Origin {
            mux: "herdr".into(),
            session: None,
            workspace: None,
            tab: None,
            pane: Some("w1:p1".into()),
        })
        .unwrap();
        assert!(s.last_activity.get() > idle_since);
    }

    #[test]
    fn status_reports_what_the_adapters_say() {
        let s = shared(
            vec![
                Box::new(Fake {
                    name: "log",
                    agent: false,
                    ..Fake::default()
                }),
                Box::new(Fake {
                    name: "claude-ide",
                    ambient: false,
                    clients: 2,
                    ..Fake::default()
                }),
            ],
            None,
        );
        let st = s.status_response();
        assert_eq!(st.clients, 2);
        assert_eq!(st.adapters.len(), 2);
        assert_eq!(st.adapters[1].name, "claude-ide");
        assert!(!st.adapters[1].supports_ambient);
        assert_eq!(st.adapters[1].clients, 2);
        assert_eq!(st.idle_timeout_secs, DEFAULT_IDLE_TIMEOUT.as_secs());
    }

    #[test]
    fn a_response_that_cannot_be_serialized_becomes_an_error_line() {
        // Only a non-UTF-8 path can do this. The client must get a line it
        // can parse, not a dropped connection.
        use std::os::unix::ffi::OsStrExt;
        let bad = PathBuf::from(std::ffi::OsStr::from_bytes(b"/ws/\xff.rs"));
        let line = encode(&SelectionEvent::new("/ws", bad, EventKind::Ambient));
        assert!(line.ends_with('\n'));
        let v: serde_json::Value = serde_json::from_str(&line).expect("still a JSON line");
        assert_eq!(v["ok"], false);
        assert!(v["error"].as_str().unwrap().contains("serialized"));
    }

    #[test]
    fn every_response_is_one_line_terminated_by_a_newline() {
        let s = shared(vec![Box::new(Fake::default())], None);
        for line in [
            encode(&s.ping_response()),
            encode(&s.status_response()),
            encode(&s.handle_send(ev(EventKind::Explicit)).unwrap()),
            encode(&OkResponse { ok: true }),
            encode(&ErrorResponse::new("nope")),
        ] {
            assert_eq!(line.matches('\n').count(), 1, "{line}");
            assert!(line.ends_with('\n'), "{line}");
            serde_json::from_str::<serde_json::Value>(&line).expect("parseable");
        }
    }

    #[test]
    fn an_ambient_event_never_reaches_an_adapter_that_cannot_express_it() {
        let strict: Rc<RefCell<Vec<SelectionEvent>>> = Default::default();
        let s = shared(
            vec![Box::new(Fake {
                name: "text-inject",
                ambient: false,
                seen: strict.clone(),
                ..Fake::default()
            })],
            None,
        );
        let resp = s.handle_send(ev(EventKind::Ambient)).unwrap();
        assert!(strict.borrow().is_empty());
        assert_eq!(resp.outcome.dropped[0].adapter, "text-inject");
        // ...but the delivery itself still counts as traffic.
        assert_eq!(s.events_received.get(), 1);

        let resp = s.handle_send(ev(EventKind::Explicit)).unwrap();
        assert_eq!(resp.outcome.delivered, vec!["text-inject"]);
        assert_eq!(strict.borrow().len(), 1);
        assert_eq!(
            strict.borrow()[0].text.as_deref(),
            Some("SECRET"),
            "adapters see the event as sent; redaction is for what selx keeps"
        );
    }

    #[test]
    fn delivery_is_reported_verbatim_by_the_router() {
        // A guard on the fake itself, so the tests above mean what they say.
        let f = Fake {
            pane: Some("w1:p9"),
            ..Fake::default()
        };
        assert!(matches!(
            f.deliver(&ev(EventKind::Explicit)).unwrap(),
            Delivery::Delivered(Some(p)) if p == "w1:p9"
        ));
    }
}
