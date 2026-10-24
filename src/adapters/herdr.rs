//! Everything selx knows about herdr: its socket API, its agent model, and
//! how an event's origin turns into a target pane.
//!
//! This is deliberately not part of `text_inject`. Injection is one thing an
//! adapter does with a pane; finding the pane is another, and the Claude
//! path needs the second without the first.

use crate::core::{Focuser, Origin, SelectionEvent};
use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tracing::{debug, warn};

/// The agent kind label herdr gives a Claude Code pane.
pub const CLAUDE_KIND: &str = "claude";

/// The value `selx send` writes into [`Origin::mux`] for a herdr pane.
/// Nothing here interprets an origin carrying any other multiplexer.
pub const MUX: &str = "herdr";

// ---- event origin -----------------------------------------------------------

/// The herdr coordinates of the pane the current process is running in, or
/// `None` outside herdr. Called by `selx send`, never by the daemon: the
/// daemon is a long-lived per-root process and its own `HERDR_*` variables
/// point at whichever pane happened to start it.
pub fn origin_from_env() -> Option<Origin> {
    let var = |k: &str| std::env::var(k).ok().filter(|s| !s.is_empty());
    let workspace = var("HERDR_WORKSPACE_ID");
    let pane = var("HERDR_PANE_ID");
    if workspace.is_none() && pane.is_none() {
        return None;
    }
    Some(Origin {
        mux: MUX.to_string(),
        session: var("HERDR_SESSION"),
        workspace,
        tab: var("HERDR_TAB_ID"),
        pane,
    })
}

/// Did this event come from a herdr pane? Weaker than [`HerdrOrigin::read`],
/// which also needs a workspace to be local to: this only asks whether the
/// sender is sitting in *this* multiplexer, which is what focus turns on.
pub fn from_herdr(origin: Option<&Origin>) -> bool {
    origin.is_some_and(|o| o.mux == MUX)
}

/// A herdr reading of an [`Origin`]. Constructing one is the single place
/// where core's opaque strings become herdr identifiers, and it succeeds
/// only for an origin that came from herdr and named a workspace — without
/// a workspace there is nothing to be local to, so targeting falls back to
/// the filesystem rule (see [`choose_target`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HerdrOrigin<'a> {
    pub workspace: &'a str,
    pub tab: Option<&'a str>,
    pub pane: Option<&'a str>,
}

impl<'a> HerdrOrigin<'a> {
    pub fn read(origin: Option<&'a Origin>) -> Option<HerdrOrigin<'a>> {
        let o = origin?;
        if o.mux != MUX {
            return None;
        }
        Some(HerdrOrigin {
            workspace: o.workspace.as_deref()?,
            tab: o.tab.as_deref(),
            pane: o.pane.as_deref(),
        })
    }
}

// ---- agent model ------------------------------------------------------------

/// One live agent as reported by `agent.list`.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct HerdrAgent {
    /// Agent kind label (`codex`, `claude`, …). `None` means unrecognized.
    #[serde(default)]
    pub agent: Option<String>,
    /// User-assigned live name, if any.
    #[serde(default)]
    pub name: Option<String>,
    pub agent_status: String,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub foreground_cwd: Option<String>,
    pub pane_id: String,
    pub workspace_id: String,
    /// Required by herdr's schema today; optional here so that if it ever
    /// stops being sent, ranking degrades to recency instead of failing to
    /// parse the agent list at all.
    #[serde(default)]
    pub tab_id: Option<String>,
    #[serde(default)]
    pub focused: bool,
    #[serde(default)]
    pub revision: u64,
    #[serde(default)]
    pub state_change_seq: u64,
}

impl HerdrAgent {
    /// Does this pane's shell/agent live inside `root`?
    pub fn in_workspace(&self, root: &Path) -> bool {
        [&self.foreground_cwd, &self.cwd]
            .into_iter()
            .flatten()
            .any(|d| Path::new(d).starts_with(root))
    }
}

// ---- socket -----------------------------------------------------------------

/// herdr's config directory: the directory of `$HERDR_CONFIG_PATH`, else
/// `~/.config/herdr`.
pub fn herdr_config_dir() -> PathBuf {
    if let Some(p) = std::env::var_os("HERDR_CONFIG_PATH").filter(|s| !s.is_empty()) {
        if let Some(dir) = Path::new(&p).parent() {
            return dir.to_path_buf();
        }
    }
    crate::core::paths::home_dir().join(".config").join("herdr")
}

/// `$HERDR_SOCKET_PATH` (set inside herdr panes), else the named session's
/// socket for `$HERDR_SESSION`, else the default session's socket.
pub fn default_socket() -> PathBuf {
    if let Some(p) = std::env::var_os("HERDR_SOCKET_PATH").filter(|s| !s.is_empty()) {
        return PathBuf::from(p);
    }
    let dir = herdr_config_dir();
    match std::env::var("HERDR_SESSION")
        .ok()
        .filter(|s| !s.is_empty())
    {
        Some(session) => dir.join("sessions").join(session).join("herdr.sock"),
        None => dir.join("herdr.sock"),
    }
}

/// Minimal client for herdr's newline-delimited JSON API
/// (`{"id","method","params"}` → `{"id","result"}` | `{"id","error"}`).
/// The server closes the connection after each response, so every call
/// opens its own.
pub struct HerdrClient {
    socket: PathBuf,
    timeout: Duration,
}

impl HerdrClient {
    /// Budget for one herdr call. Long enough for a local socket round
    /// trip that includes herdr rendering a pane; short enough that a
    /// wedged herdr cannot hold up a selection event. Nothing selx does
    /// depends on the answer arriving — a slow or absent herdr costs a
    /// delivery target or a focus move, never the event itself.
    pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(3);

    pub fn new(socket: PathBuf, timeout: Duration) -> Self {
        HerdrClient { socket, timeout }
    }

    /// A client for the running herdr, or `None` when there is not one.
    ///
    /// `override_socket` is the caller's explicit socket (tests, and the
    /// daemon's own configuration); `None` resolves [`default_socket`] on
    /// every call, so a herdr started *after* the daemon is still found —
    /// which is why this cannot be done once at startup.
    ///
    /// The absence of a socket is not an error: herdr is optional, and
    /// every caller here treats "no herdr" as "nothing to deliver to" or
    /// "no focus to move", never as a failure to report.
    pub fn discover(override_socket: Option<&Path>) -> Option<HerdrClient> {
        let socket = match override_socket {
            Some(p) => p.to_path_buf(),
            None => default_socket(),
        };
        if !socket.exists() {
            debug!(socket = %socket.display(), "herdr is not running");
            return None;
        }
        Some(HerdrClient::new(socket, HerdrClient::DEFAULT_TIMEOUT))
    }

    /// The socket this client talks to. For messages that must name it.
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    pub fn call(&self, method: &str, params: Value) -> Result<Value> {
        let stream = UnixStream::connect(&self.socket)
            .with_context(|| format!("connecting to herdr at {}", self.socket.display()))?;
        stream.set_read_timeout(Some(self.timeout))?;
        stream.set_write_timeout(Some(self.timeout))?;
        let mut w = stream.try_clone()?;
        let mut line =
            json!({ "id": format!("selx:{method}"), "method": method, "params": params })
                .to_string();
        line.push('\n');
        w.write_all(line.as_bytes())?;
        w.flush()?;
        let mut reply = String::new();
        BufReader::new(stream).read_line(&mut reply)?;
        if reply.is_empty() {
            return Err(anyhow!(
                "herdr closed the connection without answering {method}"
            ));
        }
        let v: Value = serde_json::from_str(&reply).context("unparseable herdr response")?;
        if let Some(err) = v.get("error") {
            return Err(anyhow!(
                "herdr {method}: {} ({})",
                err["message"].as_str().unwrap_or("error"),
                err["code"].as_str().unwrap_or("?")
            ));
        }
        v.get("result")
            .cloned()
            .ok_or_else(|| anyhow!("herdr response has no result"))
    }

    pub fn ping(&self) -> Result<()> {
        self.call("ping", json!({})).map(|_| ())
    }

    pub fn list_agents(&self) -> Result<Vec<HerdrAgent>> {
        let r = self.call("agent.list", json!({}))?;
        serde_json::from_value(r["agents"].clone()).context("parsing agent list")
    }

    /// Focus a pane by id. Cross-workspace: focusing a pane in another
    /// workspace switches to that workspace. Only reachable over the socket
    /// API — herdr's `pane focus` CLI moves directionally.
    pub fn focus_pane(&self, pane_id: &str) -> Result<()> {
        self.call("pane.focus", json!({ "pane_id": pane_id }))
            .map(|_| ())
    }

    pub fn send_text(&self, pane_id: &str, text: &str) -> Result<()> {
        self.call(
            "pane.send_text",
            json!({ "pane_id": pane_id, "text": text }),
        )
        .map(|_| ())
    }

    /// Press keys in a pane, by herdr's logical key names (`esc`, `enter`,
    /// `ctrl+c`, or a single character).
    ///
    /// Not the same delivery as [`send_text`](Self::send_text), which goes
    /// through bracketed paste: a pasted `:` reaches a full-screen editor as
    /// *text for the buffer*, where a pressed `:` reaches it as the command
    /// it means. Anything that has to be interpreted as a keystroke has to
    /// come through here.
    pub fn send_keys(&self, pane_id: &str, keys: &[&str]) -> Result<()> {
        self.call(
            "pane.send_keys",
            json!({ "pane_id": pane_id, "keys": keys }),
        )
        .map(|_| ())
    }

    /// What the pane shows right now, as plain text: one line per screen
    /// row, trailing blanks stripped. This is how selx reads an editor's
    /// state (mode, unsaved buffers) — no editor it drives will say.
    pub fn read_visible(&self, pane_id: &str) -> Result<String> {
        let r = self.call(
            "pane.read",
            json!({ "pane_id": pane_id, "source": "visible" }),
        )?;
        r["read"]["text"]
            .as_str()
            .map(String::from)
            .ok_or_else(|| anyhow!("herdr pane.read returned no text"))
    }

    /// Every pane, or only those in `workspace`.
    pub fn list_panes(&self, workspace: Option<&str>) -> Result<Vec<HerdrPane>> {
        let params = match workspace {
            Some(w) => json!({ "workspace_id": w }),
            None => json!({}),
        };
        let r = self.call("pane.list", params)?;
        serde_json::from_value(r["panes"].clone()).context("parsing pane list")
    }

    /// The names of the pane's foreground processes (`hx`, `lazygit`, …).
    ///
    /// This is how a pane running an editor is recognized: herdr classifies
    /// *agents*, and an editor is not one, so there is nothing to ask for by
    /// kind. An empty list is a normal answer — a pane sitting at its shell
    /// has no foreground process worth naming.
    pub fn pane_processes(&self, pane_id: &str) -> Result<Vec<String>> {
        let r = self.call("pane.process_info", json!({ "pane_id": pane_id }))?;
        let procs = r["process_info"]["foreground_processes"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|p| p["name"].as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        Ok(procs)
    }
}

/// One pane as reported by `pane.list`. Only the identity: what a pane is
/// *running* comes from [`HerdrClient::pane_processes`], which is a separate
/// call per pane.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct HerdrPane {
    pub pane_id: String,
    #[serde(default)]
    pub tab_id: Option<String>,
    #[serde(default)]
    pub workspace_id: Option<String>,
    /// Does the user's keyboard go to this pane right now?
    #[serde(default)]
    pub focused: bool,
}

// ---- target selection -------------------------------------------------------

/// How candidate panes compare: same tab as the sender, then recency, then
/// pane churn, then focus. Ordered descending — bigger is better.
pub type Rank = (bool, u64, u64, bool);

/// Rank key for candidate panes, compared descending: sharing a tab with
/// the editor beats everything, and the recency keys only break ties within
/// a tab. That ordering is what stops a second agent of the same kind — one
/// grinding through a long task off in another tab, bumping its status
/// counter all the while — from stealing the send from the agent the user
/// is actually talking to.
pub fn rank_key(agent: &HerdrAgent, origin: Option<HerdrOrigin>) -> Rank {
    let same_tab = match (origin.and_then(|o| o.tab), agent.tab_id.as_deref()) {
        (Some(t), Some(a)) => t == a,
        _ => false,
    };
    (
        same_tab,
        agent.state_change_seq,
        agent.revision,
        agent.focused,
    )
}

/// Rank `candidates` in place, best first.
pub fn rank(candidates: &mut [&HerdrAgent], origin: Option<HerdrOrigin>) {
    candidates.sort_by_key(|a| std::cmp::Reverse(rank_key(a, origin)));
}

/// Pick the pane to inject into. Pure so it can be unit tested; this is
/// where all the judgement about locality lives.
///
/// Candidates are recognized agents whose kind is not excluded. A pinned
/// `target` (live name, pane id, or kind label) is an explicit override and
/// names that agent wherever it lives, skipping the locality rules
/// entirely. Otherwise locality decides: with a herdr `origin`, only agents
/// in the *same herdr workspace* qualify — a hard filter, and the pane's
/// `cwd` is not consulted at all, so an agent that has `cd`'d out of the
/// repo is still reachable. With no usable origin (a plain terminal, a
/// test), the older filesystem rule applies instead: agents whose `cwd` or
/// `foreground_cwd` lies under `root`.
///
/// Agents waiting at an approval/question dialog (`blocked`) are never
/// typed into — the keystrokes would corrupt the dialog. That exclusion is
/// specific to injection; focus deliberately does the opposite.
pub fn choose_target<'a>(
    agents: &'a [HerdrAgent],
    root: &Path,
    origin: Option<&Origin>,
    target: Option<&str>,
    exclude: &[String],
) -> std::result::Result<&'a HerdrAgent, String> {
    let origin = HerdrOrigin::read(origin);
    let recognized: Vec<&HerdrAgent> = agents
        .iter()
        .filter(|a| {
            a.agent
                .as_deref()
                .map(|l| !exclude.iter().any(|x| x == l))
                .unwrap_or(false)
        })
        .collect();

    let mut candidates: Vec<&HerdrAgent> = if let Some(t) = target {
        let by_name: Vec<&HerdrAgent> = recognized
            .iter()
            .copied()
            .filter(|a| a.name.as_deref() == Some(t) || a.pane_id == t)
            .collect();
        let found = if by_name.is_empty() {
            recognized
                .iter()
                .copied()
                .filter(|a| a.agent.as_deref() == Some(t))
                .collect()
        } else {
            by_name
        };
        if found.is_empty() {
            return Err(format!(
                "configured inject target {t:?} is not a live herdr agent"
            ));
        }
        found
    } else {
        match origin {
            Some(o) => recognized
                .iter()
                .copied()
                .filter(|a| a.workspace_id == o.workspace)
                .collect(),
            None => recognized
                .iter()
                .copied()
                .filter(|a| a.in_workspace(root))
                .collect(),
        }
    };

    if candidates.is_empty() {
        return Err(match (recognized.is_empty(), origin) {
            (true, _) => "no eligible agent pane in herdr".to_string(),
            (false, Some(o)) => format!("no agent pane in herdr workspace {}", o.workspace),
            (false, None) => format!("no agent pane inside {}", root.display()),
        });
    }
    candidates.retain(|a| a.agent_status != "blocked");
    if candidates.is_empty() {
        return Err("candidate agent panes are blocked on an approval or question".into());
    }
    rank(&mut candidates, origin);
    if candidates.len() > 1 {
        let names: Vec<&str> = candidates.iter().map(|a| a.pane_id.as_str()).collect();
        warn!(chosen = %candidates[0].pane_id, ?names, "several agent panes match; using the best-ranked (set SELX_INJECT_TARGET to pin one)");
    }
    Ok(candidates[0])
}

/// The pane holding the Claude that received an event, if it can be found.
///
/// `registered` panes — those that ran `selx claude` for this daemon — come
/// first, because kind-matching finds panes that *look like* Claude, which
/// is not the same set as the clients that actually received anything: a
/// bare `claude` started without the wrapper appears in `agent.list` but
/// holds no connection to this daemon, and focusing it would send the user
/// to a pane that was told nothing. A registered pane counts only while it
/// is still live in `agent.list`.
///
/// With nothing registered — a daemon restarted under a running Claude, or
/// a Claude connected some other way — any pane herdr calls `claude` will
/// do. Either way a herdr origin confines the answer to its own workspace,
/// so focus never jumps workspaces, and the §4.2 ranking picks between
/// several. `blocked` is deliberately not excluded: a Claude waiting on a
/// permission prompt is exactly the one to be sent to.
pub fn resolve_claude_pane<'a>(
    agents: &'a [HerdrAgent],
    registered: &[String],
    origin: Option<HerdrOrigin>,
) -> Option<&'a HerdrAgent> {
    let local = |a: &HerdrAgent| origin.is_none_or(|o| a.workspace_id == o.workspace);
    let mut candidates: Vec<&HerdrAgent> = agents
        .iter()
        .filter(|a| local(a) && registered.contains(&a.pane_id))
        .collect();
    if candidates.is_empty() {
        candidates = agents
            .iter()
            .filter(|a| local(a) && a.agent.as_deref() == Some(CLAUDE_KIND))
            .collect();
    }
    rank(&mut candidates, origin);
    candidates.first().copied()
}

// ---- focus ------------------------------------------------------------------

/// Which of the panes that received an event should hold the focus.
///
/// Pure, so the policy is unit-testable. Reached only for events that came
/// from a herdr pane (see [`from_herdr`]): never the pane the send came from
/// (the user is already looking at it), otherwise the best-ranked pane by
/// the same key targeting uses. `agents` may be empty or incomplete — a pane
/// no agent claims is still focusable, it just ranks below any that one
/// does, and ties keep adapter order.
///
/// Note what is *not* here: `blocked` agents are excluded from injection but
/// deliberately kept for focus. A pane parked on a permission prompt is
/// exactly where the user wants to be sent.
pub fn choose_focus<'a>(
    panes: &'a [String],
    agents: &[HerdrAgent],
    origin: Option<HerdrOrigin>,
) -> Option<&'a str> {
    let mut best: Option<(&str, Option<Rank>)> = None;
    for pane in panes {
        if origin.and_then(|o| o.pane) == Some(pane.as_str()) {
            continue;
        }
        let key = agents
            .iter()
            .find(|a| a.pane_id == *pane)
            .map(|a| rank_key(a, origin));
        if best.is_none_or(|(_, b)| key > b) {
            best = Some((pane.as_str(), key));
        }
    }
    best.map(|(p, _)| p)
}

/// Moves herdr's focus to the pane that received an event.
#[derive(Default)]
pub struct PaneFocuser {
    /// Explicit socket; `None` discovers herdr per call. See
    /// [`HerdrClient::discover`].
    pub socket: Option<PathBuf>,
}

impl PaneFocuser {
    pub fn new() -> PaneFocuser {
        PaneFocuser::default()
    }
}

impl Focuser for PaneFocuser {
    fn focus(&self, panes: &[String], event: &SelectionEvent) -> Result<Option<String>> {
        // Only move a herdr pane on behalf of someone sitting in herdr.
        // Another multiplexer's coordinates say the user is looking
        // somewhere else entirely, and core has already ruled out an event
        // with no origin at all.
        if !from_herdr(event.origin.as_ref()) {
            debug!("send did not come from a herdr pane; leaving focus alone");
            return Ok(None);
        }
        let Some(client) = HerdrClient::discover(self.socket.as_deref()) else {
            return Ok(None);
        };
        let origin = HerdrOrigin::read(event.origin.as_ref());
        // Ranking is only needed to break a tie between several panes, and
        // it is the only reason to ask herdr for the agent list at all.
        let agents = if panes.len() > 1 {
            client.list_agents().context("listing herdr agents")?
        } else {
            Vec::new()
        };
        let Some(pane) = choose_focus(panes, &agents, origin) else {
            debug!(?panes, "nothing to focus");
            return Ok(None);
        };
        client
            .focus_pane(pane)
            .with_context(|| format!("focusing herdr pane {pane}"))?;
        Ok(Some(pane.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(pane: &str, kind: Option<&str>, status: &str, cwd: &str, seq: u64) -> HerdrAgent {
        HerdrAgent {
            agent: kind.map(String::from),
            name: None,
            agent_status: status.into(),
            cwd: Some(cwd.into()),
            foreground_cwd: Some(cwd.into()),
            pane_id: pane.into(),
            workspace_id: "w1".into(),
            tab_id: Some("w1:t1".into()),
            focused: false,
            revision: 0,
            state_change_seq: seq,
        }
    }

    /// `agent()` placed in a specific workspace and tab.
    fn at(pane: &str, workspace: &str, tab: &str, seq: u64) -> HerdrAgent {
        HerdrAgent {
            workspace_id: workspace.into(),
            tab_id: Some(tab.into()),
            ..agent(pane, Some("codex"), "idle", "/ws", seq)
        }
    }

    fn origin(workspace: Option<&str>, tab: Option<&str>, pane: Option<&str>) -> Origin {
        Origin {
            mux: MUX.into(),
            session: Some("selx-test".into()),
            workspace: workspace.map(String::from),
            tab: tab.map(String::from),
            pane: pane.map(String::from),
        }
    }

    fn excl() -> Vec<String> {
        vec!["claude".to_string()]
    }

    #[test]
    fn picks_workspace_agent_and_skips_claude_and_outsiders() {
        let root = Path::new("/ws");
        let agents = vec![
            agent("w1:p1", Some("claude"), "idle", "/ws", 9),
            agent("w1:p2", Some("codex"), "idle", "/ws/src", 3),
            agent("w1:p3", Some("gemini"), "idle", "/elsewhere", 8),
            agent("w1:p4", None, "unknown", "/ws", 7),
        ];
        assert_eq!(
            choose_target(&agents, root, None, None, &excl())
                .unwrap()
                .pane_id,
            "w1:p2"
        );
    }

    #[test]
    fn most_recently_active_wins_within_a_tab() {
        let root = Path::new("/ws");
        let agents = vec![
            agent("w1:p1", Some("codex"), "idle", "/ws", 2),
            agent("w1:p2", Some("opencode"), "working", "/ws", 5),
        ];
        assert_eq!(
            choose_target(&agents, root, None, None, &excl())
                .unwrap()
                .pane_id,
            "w1:p2"
        );
    }

    #[test]
    fn same_tab_beats_a_more_recently_active_agent_elsewhere() {
        let root = Path::new("/ws");
        // p2 is the long-running job in another tab, churning its status;
        // p1 shares the editor's tab and is the one being talked to.
        let agents = vec![
            at("w1:p1", "w1", "w1:t1", 1),
            at("w1:p2", "w1", "w1:t2", 99),
        ];
        let o = origin(Some("w1"), Some("w1:t1"), Some("w1:p9"));
        assert_eq!(
            choose_target(&agents, root, Some(&o), None, &excl())
                .unwrap()
                .pane_id,
            "w1:p1"
        );
        // Without the tab coordinate there is nothing to prefer, so recency
        // decides as before.
        let o = origin(Some("w1"), None, Some("w1:p9"));
        assert_eq!(
            choose_target(&agents, root, Some(&o), None, &excl())
                .unwrap()
                .pane_id,
            "w1:p2"
        );
    }

    #[test]
    fn workspace_filter_is_hard() {
        let root = Path::new("/ws");
        // Perfect on every other axis — in the repo, focused, most recently
        // active — but in another herdr workspace.
        let mut elsewhere = at("w2:p1", "w2", "w2:t1", 99);
        elsewhere.focused = true;
        let agents = vec![at("w1:p1", "w1", "w1:t7", 1), elsewhere];
        let o = origin(Some("w1"), Some("w1:t1"), Some("w1:p9"));
        assert_eq!(
            choose_target(&agents, root, Some(&o), None, &excl())
                .unwrap()
                .pane_id,
            "w1:p1"
        );

        // And it excludes rather than falling back: an origin in a
        // workspace with no agent has no target at all.
        let o = origin(Some("w3"), None, None);
        let err = choose_target(&agents, root, Some(&o), None, &excl()).unwrap_err();
        assert!(err.contains("herdr workspace w3"), "{err}");
    }

    #[test]
    fn cwd_is_not_consulted_when_the_origin_is_known() {
        let root = Path::new("/ws");
        // The agent has cd'd out of the repo; it is still in the workspace,
        // so it is still the target.
        let agents = vec![agent("w1:p1", Some("codex"), "idle", "/tmp/scratch", 1)];
        let o = origin(Some("w1"), None, None);
        assert_eq!(
            choose_target(&agents, root, Some(&o), None, &excl())
                .unwrap()
                .pane_id,
            "w1:p1"
        );
        // Same agent, no origin: the filesystem rule excludes it.
        assert!(choose_target(&agents, root, None, None, &excl()).is_err());
    }

    #[test]
    fn an_unusable_origin_falls_back_to_the_filesystem_rule() {
        let root = Path::new("/ws");
        let agents = vec![
            at("w1:p1", "w1", "w1:t1", 1),
            HerdrAgent {
                cwd: Some("/elsewhere".into()),
                foreground_cwd: Some("/elsewhere".into()),
                ..at("w2:p1", "w2", "w2:t1", 9)
            },
        ];
        // Another multiplexer's coordinates mean nothing here.
        let tmux = Origin {
            mux: "tmux".into(),
            ..origin(Some("w2"), Some("w2:t1"), Some("w2:p1"))
        };
        assert_eq!(
            choose_target(&agents, root, Some(&tmux), None, &excl())
                .unwrap()
                .pane_id,
            "w1:p1"
        );
        // A herdr origin that never learned its workspace is no better.
        let partial = origin(None, Some("w2:t1"), Some("w2:p1"));
        assert_eq!(
            choose_target(&agents, root, Some(&partial), None, &excl())
                .unwrap()
                .pane_id,
            "w1:p1"
        );
        assert_eq!(HerdrOrigin::read(Some(&partial)), None);
        assert_eq!(HerdrOrigin::read(None), None);
    }

    #[test]
    fn blocked_agents_are_never_typed_into() {
        let root = Path::new("/ws");
        let agents = vec![agent("w1:p1", Some("codex"), "blocked", "/ws", 2)];
        let err = choose_target(&agents, root, None, None, &excl()).unwrap_err();
        assert!(err.contains("blocked"), "{err}");
        let agents = vec![
            agent("w1:p1", Some("codex"), "blocked", "/ws", 9),
            agent("w1:p2", Some("codex"), "idle", "/ws", 1),
        ];
        assert_eq!(
            choose_target(&agents, root, None, None, &excl())
                .unwrap()
                .pane_id,
            "w1:p2"
        );
    }

    #[test]
    fn explicit_target_by_name_pane_or_kind() {
        let root = Path::new("/ws");
        let mut named = agent("w1:p9", Some("codex"), "idle", "/other", 1);
        named.name = Some("reviewer".into());
        let agents = vec![agent("w1:p1", Some("gemini"), "idle", "/ws", 5), named];
        assert_eq!(
            choose_target(&agents, root, None, Some("reviewer"), &excl())
                .unwrap()
                .pane_id,
            "w1:p9"
        );
        assert_eq!(
            choose_target(&agents, root, None, Some("w1:p9"), &excl())
                .unwrap()
                .pane_id,
            "w1:p9"
        );
        assert_eq!(
            choose_target(&agents, root, None, Some("gemini"), &excl())
                .unwrap()
                .pane_id,
            "w1:p1"
        );
        assert!(choose_target(&agents, root, None, Some("nope"), &excl())
            .unwrap_err()
            .contains("not a live"));
        // Excluded kinds cannot be targeted even explicitly.
        let claude = vec![agent("w1:p1", Some("claude"), "idle", "/ws", 5)];
        assert!(choose_target(&claude, root, None, Some("w1:p1"), &excl()).is_err());
        assert!(choose_target(&claude, root, None, Some("w1:p1"), &[]).is_ok());
    }

    #[test]
    fn a_pinned_target_outranks_locality() {
        let root = Path::new("/ws");
        let agents = vec![at("w1:p1", "w1", "w1:t1", 9), at("w2:p1", "w2", "w2:t1", 1)];
        let o = origin(Some("w1"), Some("w1:t1"), Some("w1:p9"));
        assert_eq!(
            choose_target(&agents, root, Some(&o), Some("w2:p1"), &excl())
                .unwrap()
                .pane_id,
            "w2:p1"
        );
    }

    #[test]
    fn empty_reasons_are_specific() {
        let root = Path::new("/ws");
        assert!(choose_target(&[], root, None, None, &excl())
            .unwrap_err()
            .contains("no eligible"));
        let agents = vec![agent("w1:p1", Some("codex"), "idle", "/other", 1)];
        assert!(choose_target(&agents, root, None, None, &excl())
            .unwrap_err()
            .contains("inside /ws"));
    }

    #[test]
    fn origin_is_read_from_the_herdr_environment() {
        // Env is process-global, so only the pure shape is asserted here;
        // the live capture is covered by the CLI end-to-end tests.
        let o = origin(Some("w1"), Some("w1:t2"), Some("w1:p3"));
        let h = HerdrOrigin::read(Some(&o)).unwrap();
        assert_eq!(h.workspace, "w1");
        assert_eq!(h.tab, Some("w1:t2"));
        assert_eq!(h.pane, Some("w1:p3"));
    }

    #[test]
    fn agents_parse_without_a_tab_id() {
        // herdr's schema requires tab_id today; ranking must degrade to
        // recency rather than fail if that ever changes.
        let a: HerdrAgent = serde_json::from_str(
            r#"{"agent":"codex","agent_status":"idle","pane_id":"w1:p1","workspace_id":"w1","state_change_seq":4}"#,
        )
        .unwrap();
        assert_eq!(a.tab_id, None);
        let o = origin(Some("w1"), Some("w1:t1"), None);
        let h = HerdrOrigin::read(Some(&o));
        assert_eq!(rank_key(&a, h), (false, 4, 0, false));
    }

    #[test]
    fn socket_resolution_prefers_explicit_env() {
        // Only checks the pure suffix logic; env is process-global so we don't mutate it.
        let dir = herdr_config_dir();
        assert!(dir.ends_with("herdr"));
        assert!(default_socket().ends_with("herdr.sock"));
    }

    fn panes(list: &[&str]) -> Vec<String> {
        list.iter().map(|p| p.to_string()).collect()
    }

    #[test]
    fn focus_goes_to_the_only_pane_that_received_the_event() {
        let ps = panes(&["w1:p2"]);
        let o = origin(Some("w1"), Some("w1:t1"), Some("w1:p1"));
        let h = HerdrOrigin::read(Some(&o));
        // No agent list needed to focus a single pane.
        assert_eq!(choose_focus(&ps, &[], h), Some("w1:p2"));
        assert_eq!(choose_focus(&ps, &[], None), Some("w1:p2"));
    }

    #[test]
    fn the_sending_pane_is_never_focused() {
        let o = origin(Some("w1"), Some("w1:t1"), Some("w1:p1"));
        let h = HerdrOrigin::read(Some(&o));
        // A send that somehow reached the editor's own pane moves nothing.
        assert_eq!(choose_focus(&panes(&["w1:p1"]), &[], h), None);
        assert_eq!(
            choose_focus(&panes(&["w1:p1", "w1:p2"]), &[], h),
            Some("w1:p2")
        );
        // Without an origin there is no pane to protect.
        assert_eq!(choose_focus(&panes(&["w1:p1"]), &[], None), Some("w1:p1"));
    }

    #[test]
    fn several_delivered_panes_rank_like_targets_do() {
        let ps = panes(&["w1:p2", "w1:p3"]);
        let agents = vec![
            at("w1:p2", "w1", "w1:t1", 1),
            at("w1:p3", "w1", "w1:t2", 99),
        ];
        let o = origin(Some("w1"), Some("w1:t1"), Some("w1:p1"));
        assert_eq!(
            choose_focus(&ps, &agents, HerdrOrigin::read(Some(&o))),
            Some("w1:p2"),
            "the pane sharing the editor's tab wins"
        );
        // No tab to prefer: recency decides, as it does for targeting.
        let o = origin(Some("w1"), None, Some("w1:p1"));
        assert_eq!(
            choose_focus(&ps, &agents, HerdrOrigin::read(Some(&o))),
            Some("w1:p3")
        );
        // A pane no agent claims ranks below one that is claimed, but is
        // still focusable on its own, and ties keep adapter order.
        let ps = panes(&["w1:p9", "w1:p3"]);
        assert_eq!(choose_focus(&ps, &agents, None), Some("w1:p3"));
        assert_eq!(
            choose_focus(&panes(&["w1:p9", "w1:p8"]), &agents, None),
            Some("w1:p9")
        );
    }

    #[test]
    fn a_blocked_agent_is_still_focused() {
        // The inverse of the injection rule: a pane parked on a permission
        // prompt is exactly where the user wants to be sent.
        let blocked = HerdrAgent {
            agent_status: "blocked".into(),
            ..at("w1:p2", "w1", "w1:t1", 5)
        };
        let agents = vec![blocked, at("w1:p3", "w1", "w1:t2", 9)];
        let ps = panes(&["w1:p2", "w1:p3"]);
        let o = origin(Some("w1"), Some("w1:t1"), Some("w1:p1"));
        assert_eq!(
            choose_focus(&ps, &agents, HerdrOrigin::read(Some(&o))),
            Some("w1:p2")
        );
    }

    #[test]
    fn only_a_herdr_sender_moves_a_herdr_pane() {
        let o = origin(Some("w1"), Some("w1:t1"), Some("w1:p1"));
        assert!(from_herdr(Some(&o)));
        // A send from a plain terminal, or from another multiplexer, leaves
        // herdr's focus where the user left it.
        assert!(!from_herdr(None));
        assert!(!from_herdr(Some(&Origin {
            mux: "tmux".into(),
            ..o.clone()
        })));
        // Weaker than a targeting origin: a pane that never learned its
        // workspace is still a seat in herdr.
        let partial = origin(None, None, Some("w1:p1"));
        assert!(from_herdr(Some(&partial)));
        assert_eq!(HerdrOrigin::read(Some(&partial)), None);
    }

    fn claude(pane: &str, tab: &str, seq: u64) -> HerdrAgent {
        HerdrAgent {
            agent: Some(CLAUDE_KIND.into()),
            tab_id: Some(tab.into()),
            ..agent(pane, Some(CLAUDE_KIND), "idle", "/ws", seq)
        }
    }

    #[test]
    fn a_registered_pane_beats_a_bare_claude_that_got_nothing() {
        // Both look like Claude to herdr and share the editor's tab; only
        // w1:p2 ran `selx claude`, so only it is connected to this daemon.
        let agents = vec![claude("w1:p2", "w1:t1", 1), claude("w1:p3", "w1:t1", 99)];
        let o = origin(Some("w1"), Some("w1:t1"), Some("w1:p1"));
        let h = HerdrOrigin::read(Some(&o));
        assert_eq!(
            resolve_claude_pane(&agents, &["w1:p2".to_string()], h)
                .unwrap()
                .pane_id,
            "w1:p2"
        );
        // Nothing registered: any Claude-looking pane will do, best ranked.
        assert_eq!(
            resolve_claude_pane(&agents, &[], h).unwrap().pane_id,
            "w1:p3"
        );
        // Registered but no longer live: fall back rather than point at a
        // pane that has gone.
        assert_eq!(
            resolve_claude_pane(&agents, &["w1:p9".to_string()], h)
                .unwrap()
                .pane_id,
            "w1:p3"
        );
    }

    #[test]
    fn claude_panes_rank_and_stay_in_the_senders_workspace() {
        let registered = vec!["w1:p2".to_string(), "w1:p3".to_string()];
        // Two registered Claudes: the one sharing the editor's tab wins,
        // however busy the other has been.
        let agents = vec![claude("w1:p2", "w1:t1", 1), claude("w1:p3", "w1:t2", 99)];
        let o = origin(Some("w1"), Some("w1:t1"), Some("w1:p1"));
        assert_eq!(
            resolve_claude_pane(&agents, &registered, HerdrOrigin::read(Some(&o)))
                .unwrap()
                .pane_id,
            "w1:p2"
        );
        // A Claude in another herdr workspace is never the answer, even
        // registered: focus must not jump workspaces.
        let elsewhere = vec![HerdrAgent {
            workspace_id: "w2".into(),
            ..claude("w2:p1", "w2:t1", 5)
        }];
        assert_eq!(
            resolve_claude_pane(
                &elsewhere,
                &["w2:p1".to_string()],
                HerdrOrigin::read(Some(&o))
            ),
            None
        );
        // Without an origin there is no workspace to be confined to.
        assert_eq!(
            resolve_claude_pane(&elsewhere, &["w2:p1".to_string()], None)
                .unwrap()
                .pane_id,
            "w2:p1"
        );
    }

    #[test]
    fn a_claude_on_a_permission_prompt_is_still_the_answer() {
        let blocked = HerdrAgent {
            agent_status: "blocked".into(),
            ..claude("w1:p2", "w1:t1", 1)
        };
        let agents = vec![blocked];
        let o = origin(Some("w1"), Some("w1:t1"), Some("w1:p1"));
        assert_eq!(
            resolve_claude_pane(&agents, &["w1:p2".to_string()], HerdrOrigin::read(Some(&o)))
                .unwrap()
                .pane_id,
            "w1:p2"
        );
    }

    #[test]
    fn no_claude_pane_at_all_is_not_an_error() {
        let agents = vec![agent("w1:p2", Some("codex"), "idle", "/ws", 1)];
        assert_eq!(resolve_claude_pane(&agents, &[], None), None);
        assert_eq!(resolve_claude_pane(&[], &["w1:p2".into()], None), None);
    }
}
