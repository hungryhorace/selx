//! TextInjectAdapter: for agents with no protocol (Codex, Gemini CLI,
//! opencode, …) running as TUIs in herdr panes.
//!
//! Picks a pane with [`crate::adapters::herdr::choose_target`] and types a short
//! reference into its input, e.g. `Regarding src/capture.rs lines 40-62
//! (currently selected): `. It never presses Enter — the user finishes the
//! prompt.
//!
//! `supports_ambient()` is `false` by design: typed text cannot express a
//! weak "may or may not be relevant" signal, so ambient events are dropped
//! by the router before they ever reach this adapter.

use crate::adapters::herdr::{choose_target, default_socket, HerdrClient, CLAUDE_KIND};
use crate::core::{Adapter, Delivery, EventKind, SelectionEvent};
use anyhow::{Context, Result};
use std::path::PathBuf;
use tracing::{debug, info};

pub const ADAPTER_NAME: &str = "text-inject";
/// Agents that have their own adapter and must not receive injected text.
pub const DEFAULT_EXCLUDE: &[&str] = &[CLAUDE_KIND];

pub struct TextInjectOptions {
    pub root: PathBuf,
    /// Explicit herdr API socket; `None` discovers herdr at every delivery
    /// so a herdr started after the daemon is still found. See
    /// [`HerdrClient::discover`].
    pub socket: Option<PathBuf>,
    /// Preferred target: a live agent name, a pane id (`w1:p2`) or an agent
    /// kind label. Overrides locality entirely.
    pub target: Option<String>,
    /// Agent kind labels never targeted.
    pub exclude: Vec<String>,
}

impl TextInjectOptions {
    pub fn new(root: PathBuf) -> Self {
        TextInjectOptions {
            root,
            socket: None,
            target: None,
            exclude: DEFAULT_EXCLUDE.iter().map(|s| s.to_string()).collect(),
        }
    }
}

pub struct TextInjectAdapter {
    opts: TextInjectOptions,
}

impl TextInjectAdapter {
    pub fn new(opts: TextInjectOptions) -> Self {
        // Logged at startup because "why did it type into *that* pane?" is
        // the question this adapter provokes, and the answer is mostly
        // which of the CLI flag / environment / config file layers won.
        info!(
            target = opts.target.as_deref().unwrap_or("(locality)"),
            exclude = %opts.exclude.join(","),
            "text injection configured"
        );
        TextInjectAdapter { opts }
    }
}

impl Adapter for TextInjectAdapter {
    fn name(&self) -> &str {
        ADAPTER_NAME
    }

    fn supports_ambient(&self) -> bool {
        false
    }

    fn deliver(&self, event: &SelectionEvent) -> Result<Delivery> {
        // The router already filters ambient events; keep the invariant
        // visible here too rather than silently typing weak context.
        if event.kind != EventKind::Explicit {
            return Ok(Delivery::Dropped(
                "text injection only carries explicit events".into(),
            ));
        }
        let Some(client) = HerdrClient::discover(self.opts.socket.as_deref()) else {
            return Ok(Delivery::Dropped(format!(
                "herdr is not running (no socket at {})",
                self.opts
                    .socket
                    .clone()
                    .unwrap_or_else(default_socket)
                    .display()
            )));
        };
        let agents = client.list_agents().context("listing herdr agents")?;
        let chosen = match choose_target(
            &agents,
            &self.opts.root,
            event.origin.as_ref(),
            self.opts.target.as_deref(),
            &self.opts.exclude,
        ) {
            Ok(a) => a,
            Err(reason) => {
                debug!(agents = agents.len(), "no injection target: {reason}");
                return Ok(Delivery::Dropped(reason));
            }
        };
        let text = format_reference(event);
        client
            .send_text(&chosen.pane_id, &text)
            .with_context(|| format!("sending text to herdr pane {}", chosen.pane_id))?;
        info!(
            pane = %chosen.pane_id,
            agent = chosen.agent.as_deref().unwrap_or("?"),
            name = chosen.name.as_deref().unwrap_or(""),
            status = %chosen.agent_status,
            "injected reference to {}",
            event.display_range()
        );
        Ok(Delivery::Delivered(Some(chosen.pane_id.clone())))
    }
}

/// The text typed into the agent's input: the sender's own sentence when
/// it gave one (already one safe line, see `SelectionEvent::message`),
/// else a reference to the file and lines. No trailing newline, ever.
pub fn format_reference(event: &SelectionEvent) -> String {
    if let Some(m) = &event.message {
        return m.clone();
    }
    let path = event.display_path();
    let s = match event.lines {
        None => format!("Regarding {path} (currently selected): "),
        Some(l) if l.start == l.end => {
            format!("Regarding {path} line {} (currently selected): ", l.start)
        }
        Some(l) => format!(
            "Regarding {path} lines {}-{} (currently selected): ",
            l.start, l.end
        ),
    };
    s.replace(['\n', '\r'], " ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::LineRange;

    #[test]
    fn reference_text_never_submits() {
        let mut ev = SelectionEvent::new("/ws", "/ws/src/capture.rs", EventKind::Explicit)
            .with_lines(40, 62)
            .with_text("ignored\n")
            .with_source("helix");
        assert_eq!(
            format_reference(&ev),
            "Regarding src/capture.rs lines 40-62 (currently selected): "
        );
        ev.lines = Some(LineRange { start: 40, end: 40 });
        assert_eq!(
            format_reference(&ev),
            "Regarding src/capture.rs line 40 (currently selected): "
        );
        ev.lines = None;
        assert_eq!(
            format_reference(&ev),
            "Regarding src/capture.rs (currently selected): "
        );
        assert!(!format_reference(&ev).contains('\n'));
        assert_eq!(
            format_reference(&ev.with_message("Regarding map3 --rect 1,2,3,4: ")),
            "Regarding map3 --rect 1,2,3,4: "
        );
    }

    #[test]
    fn adapter_declares_no_ambient_support_and_drops_when_herdr_absent() {
        let mut opts = TextInjectOptions::new("/ws".into());
        opts.socket = Some("/nonexistent/herdr.sock".into());
        let a = TextInjectAdapter::new(opts);
        assert!(!a.supports_ambient());
        let ev = SelectionEvent::new("/ws", "/ws/a.rs", EventKind::Explicit)
            .with_lines(1, 1)
            .with_source("t");
        match a.deliver(&ev).unwrap() {
            Delivery::Dropped(r) => assert!(r.contains("not running"), "{r}"),
            other => panic!("{other:?}"),
        }
        let ambient = SelectionEvent {
            kind: EventKind::Ambient,
            ..ev
        };
        assert!(matches!(a.deliver(&ambient).unwrap(), Delivery::Dropped(_)));
    }
}
