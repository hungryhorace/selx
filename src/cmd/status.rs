use crate::core::client::{self, Client, DEFAULT_TIMEOUT};
use crate::core::protocol::{EventRecord, StatusResponse};
use crate::core::StateDir;
use crate::exit;
use anyhow::Result;
use std::path::Path;

pub fn run(root: Option<&Path>, json: bool, follow: bool, verbose: u8) -> Result<u8> {
    super::init_cli_logging(verbose);
    let root = super::resolve_root(root)?;
    let state = StateDir::for_root(&root);

    let Some(status) = client::try_status(&state, DEFAULT_TIMEOUT) else {
        if json {
            crate::outln!(
                "{}",
                serde_json::json!({ "running": false, "root": root, "state_dir": state.dir, "stale_pid": state.read_pid() })
            );
        } else {
            crate::outln!("selx daemon: not running");
            crate::outln!("  root:      {}", root.display());
            crate::outln!("  state dir: {}", state.dir.display());
            if let Some(pid) = state.read_pid() {
                crate::outln!(
                    "  note:      stale pid file ({pid}); `selx ensure` will clean it up"
                );
            }
        }
        return Ok(exit::NOT_RUNNING);
    };

    if json {
        crate::outln!("{}", serde_json::to_string(&status)?);
    } else {
        print_human(&status);
    }

    if follow {
        let client = Client::connect_state(&state, DEFAULT_TIMEOUT)?;
        if !json {
            crate::outln!("--- following (Ctrl-C to stop) ---");
        }
        client.follow(|frame| {
            if json {
                crate::outln!("{}", serde_json::to_string(&frame).unwrap_or_default());
            } else {
                crate::outln!("{}", format_frame(&frame));
            }
            true
        })?;
        if !json {
            crate::outln!("--- daemon went away ---");
        }
    }
    Ok(exit::OK)
}

fn print_human(s: &StatusResponse) {
    crate::outln!("selx daemon: running (v{})", s.version);
    // `status` never restarts anything, so it is the one place that can
    // report this without acting on it.
    if crate::core::build_id::differs(&s.build) == Some(true) {
        crate::outln!("  note:      running an older selx binary than this one;");
        crate::outln!("             it restarts on the next command once its clients disconnect,");
        crate::outln!("             or `selx stop` now to restart it immediately");
    }
    crate::outln!("  root:      {}", s.root.display());
    crate::outln!("  port:      {}", s.port);
    crate::outln!("  pid:       {}", s.pid);
    crate::outln!("  uptime:    {}", super::human_duration(s.uptime_secs));
    let idle = if s.idle_timeout_secs == 0 {
        "never".to_string()
    } else {
        format!(
            "in {}",
            super::human_duration(s.idle_timeout_secs.saturating_sub(s.idle_secs))
        )
    };
    crate::outln!("  clients:   {}   (idle shutdown {idle})", s.clients);
    let adapters: Vec<String> = s
        .adapters
        .iter()
        .map(|a| {
            format!(
                "{}{}",
                a.name,
                if a.supports_ambient {
                    ""
                } else {
                    " [explicit-only]"
                }
            )
        })
        .collect();
    crate::outln!(
        "  adapters:  {}",
        if adapters.is_empty() {
            "none".into()
        } else {
            adapters.join(", ")
        }
    );
    crate::outln!("  events:    {} received", s.events_received);
    if !s.recent.is_empty() {
        crate::outln!("recent events:");
        for r in &s.recent {
            crate::outln!("  {}", format_frame(r));
        }
    }
}

pub fn format_frame(f: &EventRecord) -> String {
    let mut line = format!(
        "{}  {:<8}  {:<8}  {}",
        super::clock(f.ts),
        f.event.kind.as_str(),
        f.event.source,
        f.event.display_range()
    );
    if !f.outcome.delivered.is_empty() {
        line.push_str(&format!("  → {}", f.outcome.delivered.join(",")));
    }
    if let Some(pane) = &f.outcome.focused {
        line.push_str(&format!("  [focus {pane}]"));
    }
    if !f.outcome.dropped.is_empty() {
        let d: Vec<&str> = f
            .outcome
            .dropped
            .iter()
            .map(|d| d.adapter.as_str())
            .collect();
        line.push_str(&format!("  (dropped by {})", d.join(",")));
    }
    if !f.outcome.failed.is_empty() {
        let d: Vec<&str> = f
            .outcome
            .failed
            .iter()
            .map(|d| d.adapter.as_str())
            .collect();
        line.push_str(&format!("  (FAILED: {})", d.join(",")));
    }
    if let Some(t) = &f.event.text {
        line.push_str(&format!("  {:?}", t));
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::adapter::{Dropped, Failed, RouteOutcome};
    use crate::core::{EventKind, SelectionEvent};

    fn frame(outcome: RouteOutcome) -> EventRecord {
        EventRecord {
            ts: 1787056496,
            event: SelectionEvent::new("/ws", "/ws/src/main.rs", EventKind::Explicit)
                .with_lines(40, 62)
                .with_source("helix"),
            outcome,
        }
    }

    #[test]
    fn a_bare_frame_is_time_kind_source_and_range() {
        let line = format_frame(&frame(RouteOutcome::default()));
        assert_eq!(line, "12:34:56Z  explicit  helix     src/main.rs:40-62");
    }

    #[test]
    fn each_suffix_appears_only_when_it_has_something_to_say() {
        // Delivered.
        let line = format_frame(&frame(RouteOutcome {
            delivered: vec!["log".into(), "claude-ide".into()],
            ..RouteOutcome::default()
        }));
        assert!(line.ends_with("  \u{2192} log,claude-ide"), "{line}");

        // Focus, after delivery.
        let line = format_frame(&frame(RouteOutcome {
            delivered: vec!["text-inject".into()],
            focused: Some("w1:p2".into()),
            ..RouteOutcome::default()
        }));
        assert!(
            line.ends_with("\u{2192} text-inject  [focus w1:p2]"),
            "{line}"
        );

        // Dropped and failed name their adapters, and failure shouts.
        let line = format_frame(&frame(RouteOutcome {
            dropped: vec![Dropped {
                adapter: "text-inject".into(),
                reason: "no agent".into(),
            }],
            failed: vec![Failed {
                adapter: "claude-ide".into(),
                error: "boom".into(),
            }],
            ..RouteOutcome::default()
        }));
        assert!(line.contains("(dropped by text-inject)"), "{line}");
        assert!(line.contains("(FAILED: claude-ide)"), "{line}");
        // The reasons themselves are not in the one-line form.
        assert!(!line.contains("no agent"), "{line}");
        assert!(!line.contains("boom"), "{line}");
    }

    #[test]
    fn selected_text_is_shown_only_when_the_daemon_sent_it() {
        // The daemon redacts below -vv, so this is what "text present"
        // looks like — quoted and escaped, never spilling onto a second
        // line and breaking the one-frame-per-line contract.
        let mut f = frame(RouteOutcome::default());
        assert!(!format_frame(&f).contains('"'));
        f.event = f.event.with_text("fn main() {\n}");
        let line = format_frame(&f);
        assert!(line.ends_with(r#"  "fn main() {\n}""#), "{line}");
        assert_eq!(line.lines().count(), 1);
    }

    #[test]
    fn a_whole_file_event_has_no_line_suffix() {
        let mut f = frame(RouteOutcome::default());
        f.event.lines = None;
        assert!(format_frame(&f).ends_with("src/main.rs"));
        f.event = f.event.with_lines(7, 7);
        assert!(format_frame(&f).ends_with("src/main.rs:7"));
    }
}
