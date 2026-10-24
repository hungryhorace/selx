//! `ensure`: idempotent, race-safe "make sure a healthy daemon serves this
//! root", with full stale-state cleanup.

use crate::core::build_id;
use crate::core::client;
use crate::core::process;
use crate::core::state::StateDir;
use crate::core::workspace;
use anyhow::{Context, Result};
use rustix::fs::{flock, FlockOperation};
use serde::Serialize;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

/// Builds the command that runs the daemon for `(root, preferred_port)`.
/// Kept as a callback so this crate never learns the CLI's argument names.
pub type Launcher<'a> = &'a dyn Fn(&Path, u16) -> Command;

/// Information about stale state found by `ensure`, offered to a hook so
/// higher layers can remove anything they own outside the state dir.
#[derive(Debug, Clone)]
pub struct StaleInfo {
    pub root: PathBuf,
    pub state_dir: PathBuf,
    pub pid: Option<u32>,
    pub port: Option<u16>,
}

pub struct EnsureOptions<'a> {
    pub root: PathBuf,
    pub launcher: Launcher<'a>,
    pub on_stale: Option<&'a dyn Fn(&StaleInfo)>,
    pub startup_timeout: Duration,
}

/// A daemon serving this root that was started from a different selx
/// binary than the caller's, and was left running because agents are
/// connected to it: replacing it would drop their sessions.
#[derive(Debug, Clone, Serialize)]
pub struct OutdatedDaemon {
    pub pid: u32,
    /// What it reports as its build; empty if it predates the field.
    pub build: String,
    /// Connected agent clients — the reason it was spared. `None` if it
    /// would not say, which counts as busy.
    pub clients: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EnsureOutcome {
    pub root: PathBuf,
    pub port: u16,
    pub pid: u32,
    /// `true` if this call started the daemon.
    pub started: bool,
    /// `true` if it started this daemon by replacing an outdated one.
    pub restarted: bool,
    /// Set when an outdated daemon was found and left running; callers
    /// should tell the user, because it is serving them stale behaviour.
    pub outdated: Option<OutdatedDaemon>,
    pub state_dir: PathBuf,
    pub socket: PathBuf,
}

pub fn ensure(opts: EnsureOptions<'_>) -> Result<EnsureOutcome> {
    let state = StateDir::for_root(&opts.root);
    state.create()?;

    // Serialize concurrent ensures for this root. The loser blocks here and
    // then sees the winner's healthy daemon.
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(state.lock_file())
        .with_context(|| format!("opening {}", state.lock_file().display()))?;
    flock(&lock, FlockOperation::LockExclusive).context("locking ensure.lock")?;

    let mut restarted = false;
    if let Some(p) = client::try_ping(&state) {
        let differs = build_id::differs(&p.build);
        if differs.is_none() {
            debug!(build = %p.build, "cannot compare the daemon's build with our own");
        }
        match judge(differs, p.clients) {
            Existing::Reuse => return Ok(outcome(&state, p.port, p.pid, false)),
            Existing::Spare { clients } => {
                // Reported through the outcome, not logged: the caller
                // knows how to say this to whoever is at the keyboard.
                debug!(
                    pid = p.pid,
                    ?clients,
                    "daemon is running an older selx binary; left running"
                );
                let mut out = outcome(&state, p.port, p.pid, false);
                out.outdated = Some(OutdatedDaemon {
                    pid: p.pid,
                    build: p.build,
                    clients,
                });
                return Ok(out);
            }
            Existing::Replace => {
                info!(
                    pid = p.pid,
                    "daemon is running an older selx binary and has no clients; restarting it"
                );
                stop(&state, Duration::from_secs(5))?;
                restarted = true;
            }
        }
    }

    clean_stale(&state, opts.on_stale)?;

    let port = workspace::port_candidate(&state.root, 0);
    let cmd = (opts.launcher)(&state.root, port);
    let mut child = process::spawn_detached(cmd, &state.daemon_log())?;
    let pid = child.id();
    debug!(pid, "spawned daemon");

    let deadline = Instant::now() + opts.startup_timeout;
    loop {
        if let Some(p) = client::try_ping(&state) {
            info!(root = %state.root.display(), port = p.port, pid = p.pid, "daemon started");
            let mut out = outcome(&state, p.port, p.pid, true);
            out.restarted = restarted;
            return Ok(out);
        }
        if let Ok(Some(status)) = child.try_wait() {
            anyhow::bail!(
                "daemon exited during startup ({status}); last log lines from {}:\n{}",
                state.daemon_log().display(),
                process::tail(&state.daemon_log(), 15)
            );
        }
        if Instant::now() >= deadline {
            process::terminate(pid, Duration::from_millis(500));
            let _ = child.try_wait();
            anyhow::bail!(
                "daemon did not become healthy within {:?}; see {}",
                opts.startup_timeout,
                state.daemon_log().display()
            );
        }
        std::thread::sleep(Duration::from_millis(30));
    }
}

/// What to do with a daemon that is already answering for this root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Existing {
    /// Keep it: it runs our binary, or we cannot tell that it does not.
    Reuse,
    /// Replace it: it runs a different binary and nobody is listening.
    Replace,
    /// Leave it running and tell the caller: a different binary, but
    /// agents are connected. `clients` is what it reported.
    Spare { clients: Option<usize> },
}

/// The outdated-daemon decision. Both arguments come from one `ping`:
/// `differs` is [`build_id::differs`] on the daemon's reported build, and
/// `clients` is what it said it was serving — `None` from a daemon that
/// predates the field, and so will not say.
///
/// Replacing a daemon drops every agent connected to it, so an outdated one
/// is only replaced while nobody is listening — and a daemon that will not
/// say how many agents it serves counts as busy, because the cost of
/// guessing wrong is someone's live session.
fn judge(differs: Option<bool>, clients: Option<usize>) -> Existing {
    match differs {
        Some(true) => match clients {
            Some(0) => Existing::Replace,
            clients => Existing::Spare { clients },
        },
        // Same build, or a build neither side can identify: assume it is
        // ours. Repeatedly "correcting" a difference we cannot see would
        // restart a healthy daemon on every command.
        Some(false) | None => Existing::Reuse,
    }
}

fn outcome(state: &StateDir, port: u16, pid: u32, started: bool) -> EnsureOutcome {
    EnsureOutcome {
        root: state.root.clone(),
        port,
        pid,
        started,
        restarted: false,
        outdated: None,
        state_dir: state.dir.clone(),
        socket: state.socket_path(),
    }
}

/// Remove every trace of a daemon that is not answering. Caller holds the
/// ensure lock and has already established that the socket is unhealthy.
fn clean_stale(state: &StateDir, on_stale: Option<&dyn Fn(&StaleInfo)>) -> Result<()> {
    let pid = state.read_pid();
    let port = state.read_port();
    let had_state = pid.is_some() || port.is_some() || state.socket_path().exists();

    if let Some(pid) = pid.filter(|p| process::pid_alive(*p)) {
        // The pid file names a live process that doesn't answer. It might be
        // a daemon still starting (no other ensure can be spawning one while
        // we hold the lock, but a previous ensure may have timed out).
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if client::try_ping(state).is_some() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        warn!(
            pid,
            "pid file names a live process that does not answer; terminating it"
        );
        if !process::terminate(pid, Duration::from_secs(1)) {
            anyhow::bail!("could not terminate unresponsive process {pid}");
        }
    }

    if had_state {
        info!(
            ?pid,
            ?port,
            "cleaning stale daemon state in {}",
            state.dir.display()
        );
    }
    state.remove_runtime_files();
    if let Some(hook) = on_stale {
        hook(&StaleInfo {
            root: state.root.clone(),
            state_dir: state.dir.clone(),
            pid,
            port,
        });
    }
    Ok(())
}

/// Ask the daemon for `state` to stop, then make sure it is gone and its
/// state is cleaned. Returns `true` if a daemon was running.
pub fn stop(state: &StateDir, grace: Duration) -> Result<bool> {
    let pid = state.read_pid();
    let running = match client::Client::connect_state(state, Duration::from_secs(2)) {
        Ok(mut c) => {
            let _ = c.stop();
            true
        }
        Err(_) => pid.map(process::pid_alive).unwrap_or(false),
    };
    if let Some(pid) = pid {
        if !process::wait_gone(pid, grace) {
            warn!(pid, "daemon did not exit after stop request; killing");
            process::terminate(pid, Duration::from_millis(500));
        }
    }
    state.remove_runtime_files();
    Ok(running)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    /// The whole outdated-daemon table, which was previously a comment and
    /// one end-to-end test. The "unknown counts as busy" rule is the one
    /// worth pinning: guessing wrong there drops someone's live session.
    #[test]
    fn only_an_idle_daemon_on_a_different_build_is_replaced() {
        // Our build, or one we cannot compare: keep it, whatever it is
        // serving.
        for clients in [Some(0), Some(3), None] {
            assert_eq!(judge(Some(false), clients), Existing::Reuse);
            assert_eq!(judge(None, clients), Existing::Reuse);
        }

        assert_eq!(judge(Some(true), Some(0)), Existing::Replace);
        assert_eq!(
            judge(Some(true), Some(3)),
            Existing::Spare { clients: Some(3) }
        );
        // Would not say how many: busy, not idle.
        assert_eq!(judge(Some(true), None), Existing::Spare { clients: None });
    }

    /// A pid file naming a live process that never answers: `clean_stale`
    /// waits its 2 s, gives up, and terminates it. The `kill -9` end-to-end
    /// test cannot reach this branch — it leaves a *dead* pid behind.
    #[test]
    fn a_live_process_that_does_not_answer_is_terminated() {
        let base = tempfile::tempdir().unwrap();
        let root = base.path().join("ws");
        std::fs::create_dir(&root).unwrap();
        let state = StateDir {
            root: root.clone(),
            dir: base.path().join("state"),
        };
        state.create().unwrap();

        // Something alive that will never bind the socket, and — like a
        // real daemon, which outlives the `ensure` that spawned it — not a
        // child of this process: an unreaped child would be a zombie, and a
        // zombie still answers `kill(pid, 0)`.
        let pid = orphan_sleep();
        state.write_pid(pid).unwrap();
        state.write_port(12345).unwrap();

        let seen = Cell::new(None);
        let started = Instant::now();
        clean_stale(&state, Some(&|info: &StaleInfo| seen.set(info.pid))).unwrap();

        // It waited for the daemon that never came, then killed it.
        assert!(started.elapsed() >= Duration::from_secs(2), "gave up early");
        assert!(
            process::wait_gone(pid, Duration::from_secs(2)),
            "still alive"
        );

        // And the state it left is gone, with the hook told what was there.
        assert_eq!(seen.get(), Some(pid));
        assert_eq!(state.read_pid(), None);
        assert_eq!(state.read_port(), None);
    }

    /// A `sleep` reparented to init, so this process is not its parent.
    fn orphan_sleep() -> u32 {
        let out = Command::new("sh")
            .args(["-c", "sleep 30 >/dev/null 2>&1 & echo $!"])
            .output()
            .expect("spawn sh");
        let pid: u32 = String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse()
            .expect("pid from sh");
        assert!(process::pid_alive(pid), "orphan {pid} died immediately");
        pid
    }

    #[test]
    fn a_dead_pid_is_cleaned_without_waiting() {
        let base = tempfile::tempdir().unwrap();
        let root = base.path().join("ws");
        std::fs::create_dir(&root).unwrap();
        let state = StateDir {
            root,
            dir: base.path().join("state"),
        };
        state.create().unwrap();

        // Reap a real child so the pid is certainly dead and not a zombie.
        let mut child = Command::new("sleep")
            .arg("0")
            .stdin(std::process::Stdio::null())
            .spawn()
            .expect("spawn sleep");
        let pid = child.id();
        let _ = child.wait();
        state.write_pid(pid).unwrap();

        let started = Instant::now();
        clean_stale(&state, None).unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "waited on a dead pid"
        );
        assert_eq!(state.read_pid(), None);
    }
}
