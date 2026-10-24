use crate::adapters::claude_ide::{lock, ClaudeIdeAdapter, ClaudeIdeOptions};
use crate::adapters::herdr::PaneFocuser;
use crate::adapters::text_inject::{TextInjectAdapter, TextInjectOptions};
use crate::adapters::LogAdapter;
use crate::core::daemon::{self, BindInfo, DaemonConfig};
use crate::core::{workspace, Adapter, StateDir};
use crate::exit;
use anyhow::Result;
use std::path::PathBuf;
use std::time::Duration;

pub fn run(
    root: PathBuf,
    port: Option<u16>,
    foreground: bool,
    idle_timeout: Option<u64>,
    verbose: u8,
) -> Result<u8> {
    init_daemon_logging(foreground, verbose);

    let root = workspace::resolve_root(Some(&root), &root)?;
    let state = StateDir::for_root(&root);
    let mut cfg = DaemonConfig::new(
        root.clone(),
        port.unwrap_or_else(|| workspace::port_candidate(&root, 0)),
    );
    // CLI flag > environment > `[[workspace]]` > top level > default. The
    // first leg is here, because only this function has the flags; the
    // rest is `config`'s, in one place for every setting.
    let settings = crate::config::Config::load()?
        .for_root(&root)
        .apply_process_env()?;
    cfg.idle_timeout = idle_timeout
        .or(settings.idle_timeout_secs)
        .map_or(daemon::DEFAULT_IDLE_TIMEOUT, Duration::from_secs);
    cfg.focus = settings.focus_mode.unwrap_or_default();
    // Selected text only ever reaches logs/status at -vv.
    cfg.verbose = verbose >= 2;

    if foreground {
        eprintln!(
            "selx daemon for {} (state dir {})",
            root.display(),
            state.dir.display()
        );
    }

    let ide_name = super::ide_name();
    daemon::run_blocking(
        cfg,
        Box::new(move |bind: &mut BindInfo| {
            let mut adapters: Vec<Box<dyn Adapter>> = vec![Box::new(LogAdapter::new(
                bind.state.events_log(),
                bind.verbose,
            ))];
            let listener = bind
                .listener
                .take()
                .expect("daemon offers its listener exactly once");
            adapters.push(Box::new(ClaudeIdeAdapter::start(ClaudeIdeOptions {
                root: bind.root.clone(),
                port: bind.port,
                listener,
                lock_dir: lock::default_lock_dir(),
                ide_name,
                verbose: bind.verbose,
                herdr_socket: None,
            })?));
            adapters.push(Box::new(TextInjectAdapter::new(text_inject_options(
                bind.root.clone(),
                &settings,
            ))));
            // One focuser for the whole daemon: focus is decided once per
            // event, after routing, never by an adapter mid-delivery.
            bind.focuser = Some(Box::new(PaneFocuser::new()));
            Ok(adapters)
        }),
    )?;
    Ok(exit::OK)
}

/// herdr text injection settings, as the daemon's adapter wants them.
/// Everything about *where* they came from is settled by then; the default
/// exclude list (`claude` — it has its own adapter) belongs to the adapter.
fn text_inject_options(root: PathBuf, settings: &crate::config::Resolved) -> TextInjectOptions {
    let mut opts = TextInjectOptions::new(root);
    opts.target = settings.inject_target.clone();
    if let Some(exclude) = &settings.inject_exclude {
        opts.exclude = exclude.clone();
    }
    opts
}

/// Logging for the daemon: chattier than a CLI command by default, because
/// its log *is* how anyone sees what it did — nobody is watching it work.
/// Colour only when it is attached to a terminal; detached, stderr is
/// `daemon.log`.
fn init_daemon_logging(foreground: bool, verbose: u8) {
    super::init_logging(
        match verbose {
            0 => "info",
            1 => "debug",
            _ => "trace",
        },
        foreground,
    );
}
