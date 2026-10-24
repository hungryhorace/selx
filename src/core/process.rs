//! Small, portable (Linux + macOS) process helpers.

use anyhow::{Context, Result};
use rustix::process::{kill_process, test_kill_process, Pid, Signal};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn pid(raw: u32) -> Option<Pid> {
    // rustix asserts on negative raw pids; anything above i32::MAX is bogus.
    i32::try_from(raw).ok().and_then(Pid::from_raw)
}

/// Is there a process with this pid we could signal? `EPERM` counts as alive.
pub fn pid_alive(raw: u32) -> bool {
    let Some(p) = pid(raw) else { return false };
    match test_kill_process(p) {
        Ok(()) => true,
        Err(rustix::io::Errno::PERM) => true,
        Err(_) => false,
    }
}

pub fn send_term(raw: u32) {
    if let Some(p) = pid(raw) {
        let _ = kill_process(p, Signal::TERM);
    }
}

pub fn send_kill(raw: u32) {
    if let Some(p) = pid(raw) {
        let _ = kill_process(p, Signal::KILL);
    }
}

/// Poll until the pid is gone or `timeout` elapses. Returns `true` if gone.
pub fn wait_gone(raw: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while pid_alive(raw) {
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    true
}

/// SIGTERM, wait `grace`, then SIGKILL. Returns `true` if the process is gone.
pub fn terminate(raw: u32, grace: Duration) -> bool {
    if !pid_alive(raw) {
        return true;
    }
    send_term(raw);
    if wait_gone(raw, grace) {
        return true;
    }
    send_kill(raw);
    wait_gone(raw, Duration::from_secs(2))
}

/// Spawn `cmd` fully detached from the calling terminal: new session
/// (`setsid`), stdin from /dev/null, stdout+stderr appended to `log`.
///
/// The returned [`Child`] must be polled with `try_wait` while the caller
/// cares whether it is alive: an exited-but-unreaped child is a zombie, and
/// zombies still answer `kill(pid, 0)`. Once the caller exits, init reaps it.
///
/// The crate denies `unsafe_code`; this is the one place it is allowed, for
/// `pre_exec` below.
#[allow(unsafe_code)]
pub fn spawn_detached(mut cmd: Command, log: &Path) -> Result<Child> {
    use std::os::unix::process::CommandExt;
    let logf = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(log)
        .with_context(|| format!("opening {}", log.display()))?;
    let errf = logf.try_clone()?;
    cmd.stdin(Stdio::null())
        .stdout(Stdio::from(logf))
        .stderr(Stdio::from(errf));
    // SAFETY: setsid is async-signal-safe and touches no Rust state.
    unsafe {
        cmd.pre_exec(|| {
            rustix::process::setsid().map_err(std::io::Error::from)?;
            Ok(())
        });
    }
    cmd.spawn().context("spawning daemon")
}

/// Tail of a log file for error messages.
pub fn tail(path: &Path, lines: usize) -> String {
    let Ok(s) = std::fs::read_to_string(path) else {
        return String::new();
    };
    let v: Vec<&str> = s.lines().collect();
    let start = v.len().saturating_sub(lines);
    v[start..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pid_no_process_can_have_is_not_alive() {
        // 0 means "every process in my group" to `kill(2)`; selx must never
        // signal that, so the conversion rejects it before rustix sees it.
        assert!(!pid_alive(0));
        // Above `i32::MAX` there is no pid to ask about, and rustix asserts
        // on a negative raw pid — the `i32::try_from` guard is what stops it.
        assert!(!pid_alive(u32::MAX));
        assert!(pid_alive(std::process::id()), "we are alive");
    }

    #[test]
    fn terminating_a_process_that_is_already_gone_succeeds() {
        let mut child = Command::new("sh")
            .args(["-c", "exit 0"])
            .stdin(Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        child.wait().unwrap();
        assert!(terminate(pid, Duration::from_millis(50)));
        assert!(wait_gone(pid, Duration::from_millis(50)));
    }

    #[test]
    fn a_running_process_is_terminated_within_the_grace_period() {
        // Not a child of ours: an unreaped child would be a zombie, and a
        // zombie still answers `kill(pid, 0)`.
        let out = Command::new("sh")
            .args(["-c", "sleep 30 >/dev/null 2>&1 & echo $!"])
            .output()
            .unwrap();
        let pid: u32 = String::from_utf8_lossy(&out.stdout).trim().parse().unwrap();
        assert!(pid_alive(pid));

        let started = Instant::now();
        assert!(terminate(pid, Duration::from_secs(2)));
        assert!(!pid_alive(pid));
        // `sleep` dies on SIGTERM, so the grace period is never spent.
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn tail_asks_for_more_than_is_there_and_for_a_file_that_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("daemon.log");

        // A missing log is an empty tail, not an error: it is only ever
        // read to decorate a message that is already about a failure.
        assert_eq!(tail(&f, 5), "");

        std::fs::write(&f, "one\ntwo\nthree\n").unwrap();
        assert_eq!(tail(&f, 5), "one\ntwo\nthree");
        assert_eq!(tail(&f, 2), "two\nthree");
        assert_eq!(tail(&f, 0), "");
        assert_eq!(tail(&f, 1), "three");

        std::fs::write(&f, "").unwrap();
        assert_eq!(tail(&f, 5), "");
    }

    #[test]
    fn a_detached_child_writes_to_the_log_and_leaves_our_session() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("daemon.log");
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "echo out; echo err >&2"]);
        let mut child = spawn_detached(cmd, &log).unwrap();
        child.wait().unwrap();
        let written = std::fs::read_to_string(&log).unwrap();
        assert!(
            written.contains("out") && written.contains("err"),
            "{written}"
        );
        // 0600: the log carries file paths, and at -vv selected text.
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&log).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "{mode:o}");
    }
}
