pub mod claude;
pub mod daemon;
pub mod ensure;
pub mod open;
pub mod reload;
pub mod send;
pub mod status;
pub mod stop;

use crate::adapters::claude_ide;
use crate::core::ensure::{EnsureOptions, EnsureOutcome, StaleInfo};
use anyhow::Result;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

/// Initialise `tracing` on stderr. `SELX_LOG` overrides `default`.
///
/// `ansi` is off for a daemon writing to `daemon.log`, where escape codes
/// are noise in a file nobody reads through a terminal.
///
/// These are `-v` diagnostics only. Anything the user is meant to read
/// goes through [`crate::outln!`] or [`crate::warnln!`] instead.
pub fn init_logging(default: &str, ansi: bool) {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_env("SELX_LOG").unwrap_or_else(|_| EnvFilter::new(default));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_ansi(ansi)
        .with_writer(std::io::stderr)
        .try_init();
}

/// Logging for a CLI command: quiet by default, since its output is the
/// answer and the diagnostics are an aside.
pub fn init_cli_logging(verbose: u8) {
    init_logging(
        match verbose {
            0 => "warn",
            1 => "info",
            _ => "debug",
        },
        true,
    );
}

/// Command line for the daemon process, spawned by `ensure`.
pub fn daemon_command(root: &Path, port: u16) -> Command {
    let exe = std::env::current_exe().unwrap_or_else(|_| "selx".into());
    let mut c = Command::new(exe);
    c.arg("daemon")
        .arg("--root")
        .arg(root)
        .arg("--port")
        .arg(port.to_string());
    c
}

/// `ideName` written to the Claude lock file (`SELX_IDE_NAME` overrides).
pub fn ide_name() -> String {
    std::env::var("SELX_IDE_NAME")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| claude_ide::DEFAULT_IDE_NAME.to_string())
}

/// Called by `ensure` after a stale daemon's state was removed: remove the
/// Claude IDE lock file that daemon left behind (a `kill -9`'d daemon cannot
/// clean it up itself). Never touches another IDE's live lock file.
pub fn on_stale(info: &StaleInfo) {
    tracing::debug!(?info, "stale daemon state cleaned");
    if let Some(port) = info.port {
        let dir = claude_ide::lock::default_lock_dir();
        if claude_ide::lock::remove_if_stale(&dir, port, info.pid, &ide_name()) {
            tracing::info!(port, "removed orphaned Claude IDE lock file");
        }
    }
}

/// Resolve the root (cwd-based) and ensure a daemon for it.
///
/// Every command that talks to a daemon comes through here, so this is
/// also where an installed-over daemon is reported: a daemon runs for
/// hours and keeps whatever binary started it, so without this a fresh
/// `selx` silently drives old code.
pub fn ensure_root(root: &Path) -> Result<EnsureOutcome> {
    let out = crate::core::ensure::ensure(EnsureOptions {
        root: root.to_path_buf(),
        launcher: &daemon_command,
        on_stale: Some(&on_stale),
        startup_timeout: Duration::from_secs(10),
    })?;
    if out.restarted {
        tracing::info!(pid = out.pid, "replaced a daemon running an older selx");
    }
    if let Some(old) = &out.outdated {
        let why = match old.clients {
            Some(n) => format!("{n} agent client(s) are connected to it"),
            None => "it would not say how many agents are connected to it".to_string(),
        };
        crate::warnln!(
            "daemon {} for {} is running an older selx than this one; it was left \
             alone because {why}, so run `selx stop` here once they are done",
            old.pid,
            out.root.display(),
        );
    }
    Ok(out)
}

pub fn cwd() -> Result<std::path::PathBuf> {
    std::env::current_dir().map_err(|e| anyhow::anyhow!("cannot determine current directory: {e}"))
}

pub fn resolve_root(explicit: Option<&Path>) -> Result<std::path::PathBuf> {
    crate::core::workspace::resolve_root(explicit, &cwd()?)
}

/// `Xm Ys` style durations for status output.
pub fn human_duration(secs: u64) -> String {
    let (d, h, m, s) = (
        secs / 86400,
        (secs / 3600) % 24,
        (secs / 60) % 60,
        secs % 60,
    );
    match (d, h, m) {
        (0, 0, 0) => format!("{s}s"),
        (0, 0, _) => format!("{m}m{s:02}s"),
        (0, _, _) => format!("{h}h{m:02}m"),
        _ => format!("{d}d{h:02}h"),
    }
}

/// `HH:MM:SS` (UTC) from unix seconds; enough for tailing events.
pub fn clock(ts: u64) -> String {
    let s = ts % 86400;
    format!("{:02}:{:02}:{:02}Z", s / 3600, (s / 60) % 60, s % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_read_the_way_a_person_would_say_them() {
        // Each unit takes over exactly where the one below it runs out, and
        // the lower unit is zero-padded so the widths line up in `status`.
        for (secs, want) in [
            (0, "0s"),
            (1, "1s"),
            (59, "59s"),
            (60, "1m00s"),
            (61, "1m01s"),
            (3599, "59m59s"),
            (3600, "1h00m"),
            (3660, "1h01m"),
            (86399, "23h59m"),
            (86400, "1d00h"),
            (90000, "1d01h"),
            (14400, "4h00m"),
        ] {
            assert_eq!(human_duration(secs), want, "{secs}s");
        }
        // Nothing overflows or panics at the far end.
        assert!(human_duration(u64::MAX).ends_with('h'));
    }

    #[test]
    fn the_clock_is_utc_wall_time_from_unix_seconds() {
        assert_eq!(clock(0), "00:00:00Z");
        assert_eq!(clock(1), "00:00:01Z");
        assert_eq!(clock(61), "00:01:01Z");
        assert_eq!(clock(86399), "23:59:59Z");
        // The date is not shown, so midnight comes back round.
        assert_eq!(clock(86400), "00:00:00Z");
        // A real timestamp: 2026-08-18T12:34:56Z.
        assert_eq!(clock(1787056496), "12:34:56Z");
    }
}
