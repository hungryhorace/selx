use crate::core::client::{Client, DEFAULT_TIMEOUT};
use crate::core::workspace;
use crate::core::{EventKind, LineRange, SelectionEvent, StateDir};
use crate::exit;
use anyhow::{Context, Result};
use std::io::Read;
use std::path::PathBuf;

pub struct SendArgs {
    /// `None` for `selx say`: a message about no file.
    pub file: Option<PathBuf>,
    /// `None` means the whole file.
    pub lines: Option<LineRange>,
    pub root: Option<PathBuf>,
    pub explicit: bool,
    pub source: String,
    pub text_stdin: bool,
    pub message: Option<String>,
    pub json: bool,
}

pub fn run(args: SendArgs, verbose: u8) -> Result<u8> {
    super::init_cli_logging(verbose);
    let cwd = super::cwd()?;
    let kind = if args.explicit {
        EventKind::Explicit
    } else {
        EventKind::Ambient
    };
    let (root, event) = match (&args.file, &args.message) {
        (Some(f), _) => {
            let file = workspace::absolutize(f, &cwd);
            let root = workspace::resolve_root_for_file(args.root.as_deref(), &file, &cwd)?;
            (root.clone(), SelectionEvent::new(root, file, kind))
        }
        (None, Some(m)) => {
            let root = workspace::resolve_root(args.root.as_deref(), &cwd)?;
            (root.clone(), SelectionEvent::message(root, m))
        }
        (None, None) => anyhow::bail!("a send with no file must say something"),
    };
    let mut event = event
        .with_source(args.source)
        .with_line_range(args.lines)
        // Only this process can answer "where did this come from": it is a
        // child of the editor, while the daemon is a long-lived per-root
        // process that may belong to no pane at all.
        .with_origin_opt(crate::adapters::herdr::origin_from_env());
    if args.text_stdin {
        let mut text = String::new();
        std::io::stdin()
            .read_to_string(&mut text)
            .context("reading selection text from stdin")?;
        event = event.with_text(text);
    }
    if let Err(e) = event.validate() {
        anyhow::bail!("invalid selection: {e}");
    }

    let ensured = super::ensure_root(&root)?;
    let state = StateDir::for_root(&root);
    let mut client = Client::connect_state(&state, DEFAULT_TIMEOUT).with_context(|| {
        format!(
            "daemon for {} started (pid {}) but its socket is not reachable",
            root.display(),
            ensured.pid
        )
    })?;
    let resp = client.send(event.clone())?;

    if args.json {
        crate::outln!("{}", serde_json::to_string(&resp.outcome)?);
    } else {
        for f in &resp.outcome.failed {
            crate::warnln!("adapter {} failed: {}", f.adapter, f.error);
        }
        // Both kinds, not just explicit: with focus following delivery, a
        // dropped ambient send shows up as focus not moving, and that needs
        // an explanation. Recorders like the event log do not count.
        if resp.outcome.agents.is_empty() {
            crate::warnln!("no agent received {}", event.display_range());
        }
    }
    Ok(exit::OK)
}
