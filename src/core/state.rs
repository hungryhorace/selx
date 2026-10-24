//! Per-root runtime state directory.
//!
//! Layout: `<base>/<root-hash>/` containing `root`, `pid`, `port`,
//! `ingest.sock`, `ensure.lock`, `events.log`, `daemon.log`, and — for
//! `selx reload`, which runs without a daemon — `reload.lock` and
//! `reload-pending`.
//! `<base>` is `$SELX_STATE_DIR` if set, else `$XDG_RUNTIME_DIR/selx`, else
//! the per-user cache dir (`~/.cache/selx/run` on Linux and on WSL without a
//! runtime dir, `~/Library/Caches/selx/run` on macOS).

use crate::core::paths::write_private;
use crate::core::workspace::root_hash;
use anyhow::{Context, Result};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateDir {
    pub root: PathBuf,
    pub dir: PathBuf,
}

/// Where all per-root state directories live.
pub fn base_dir() -> PathBuf {
    base_dir_from(
        std::env::var_os("SELX_STATE_DIR"),
        std::env::var_os("XDG_RUNTIME_DIR"),
    )
}

/// The fallback chain behind [`base_dir`], separated from the environment so
/// it is testable (and so the macOS / no-`$XDG_RUNTIME_DIR` path is exercised
/// by CI on every platform rather than only where it happens to be the
/// default).
fn base_dir_from(
    state_override: Option<std::ffi::OsString>,
    runtime_dir: Option<std::ffi::OsString>,
) -> PathBuf {
    if let Some(p) = state_override.filter(|s| !s.is_empty()) {
        return PathBuf::from(p);
    }
    if let Some(p) = runtime_dir.filter(|s| !s.is_empty()) {
        let p = PathBuf::from(p);
        if p.is_dir() {
            return p.join("selx");
        }
    }
    if let Some(dirs) = directories::BaseDirs::new() {
        return dirs.cache_dir().join("selx").join("run");
    }
    std::env::temp_dir().join("selx")
}

impl StateDir {
    pub fn for_root(root: &Path) -> StateDir {
        StateDir {
            root: root.to_path_buf(),
            dir: base_dir().join(root_hash(root)),
        }
    }

    /// Enumerate every state dir under the base dir that records a root.
    pub fn all() -> Vec<StateDir> {
        Self::all_in(&base_dir())
    }

    /// [`all`](Self::all) against an explicit base, so the "what counts as
    /// a state dir" rule is testable without the environment.
    ///
    /// Deliberately forgiving: a missing base dir, a directory with no
    /// `root` file, and an empty one are all just "not a daemon", because
    /// this backs `selx stop --all` — which must not fail over debris.
    pub fn all_in(base: &Path) -> Vec<StateDir> {
        let mut out = Vec::new();
        let Ok(rd) = fs::read_dir(base) else {
            return out;
        };
        for entry in rd.flatten() {
            let dir = entry.path();
            if let Ok(root) = fs::read_to_string(dir.join("root")) {
                let root = root.trim_end();
                if !root.is_empty() {
                    out.push(StateDir {
                        root: PathBuf::from(root),
                        dir,
                    });
                }
            }
        }
        out
    }

    /// Create base and per-root dirs with mode 0700.
    pub fn create(&self) -> Result<()> {
        self.check_socket_path_len()?;
        let base = self.dir.parent().unwrap_or(&self.dir);
        for d in [base, &self.dir] {
            fs::create_dir_all(d).with_context(|| format!("creating {}", d.display()))?;
            fs::set_permissions(d, fs::Permissions::from_mode(0o700))
                .with_context(|| format!("chmod 0700 {}", d.display()))?;
        }
        Ok(())
    }

    pub fn root_file(&self) -> PathBuf {
        self.dir.join("root")
    }
    pub fn pid_file(&self) -> PathBuf {
        self.dir.join("pid")
    }
    pub fn port_file(&self) -> PathBuf {
        self.dir.join("port")
    }
    pub fn socket_path(&self) -> PathBuf {
        self.dir.join("ingest.sock")
    }
    pub fn lock_file(&self) -> PathBuf {
        self.dir.join("ensure.lock")
    }
    pub fn events_log(&self) -> PathBuf {
        self.dir.join("events.log")
    }
    pub fn daemon_log(&self) -> PathBuf {
        self.dir.join("daemon.log")
    }
    /// Present while an editor reload is owed: one was asked for and the
    /// editor's screen said "not now". Nothing to do with the daemon, so
    /// [`remove_runtime_files`](Self::remove_runtime_files) leaves it.
    pub fn reload_pending_file(&self) -> PathBuf {
        self.dir.join("reload-pending")
    }
    pub fn reload_lock_file(&self) -> PathBuf {
        self.dir.join("reload.lock")
    }

    /// Serialize editor reloads for this root: an exclusive lock on
    /// `reload.lock`, held until the returned file is dropped. Claude runs
    /// edits in parallel, so its hooks can fire at once, and two processes
    /// typing into one editor would interleave their keystrokes.
    pub fn lock_reload(&self) -> Result<fs::File> {
        use rustix::fs::{flock, FlockOperation};
        use std::os::unix::fs::OpenOptionsExt;
        self.create()?;
        let path = self.reload_lock_file();
        let lock = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        flock(&lock, FlockOperation::LockExclusive).context("locking reload.lock")?;
        Ok(lock)
    }

    pub fn read_pid(&self) -> Option<u32> {
        fs::read_to_string(self.pid_file())
            .ok()?
            .trim()
            .parse()
            .ok()
    }
    pub fn read_port(&self) -> Option<u16> {
        fs::read_to_string(self.port_file())
            .ok()?
            .trim()
            .parse()
            .ok()
    }

    pub fn write_root(&self) -> Result<()> {
        write_private(
            &self.root_file(),
            format!("{}\n", self.root.display()).as_bytes(),
        )
    }
    pub fn write_pid(&self, pid: u32) -> Result<()> {
        write_private(&self.pid_file(), format!("{pid}\n").as_bytes())
    }
    pub fn write_port(&self, port: u16) -> Result<()> {
        write_private(&self.port_file(), format!("{port}\n").as_bytes())
    }

    /// Remove the files that describe a *running* daemon. Leaves `root`,
    /// `ensure.lock` and logs in place.
    pub fn remove_runtime_files(&self) {
        for p in [self.pid_file(), self.port_file(), self.socket_path()] {
            let _ = fs::remove_file(p);
        }
    }

    /// Remove everything, including the directory itself.
    pub fn remove_all(&self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// Portable upper bound for `sockaddr_un.sun_path` (104 on macOS, 108 on
/// Linux) minus the trailing NUL.
pub const MAX_SOCKET_PATH: usize = 103;

impl StateDir {
    /// Unix socket paths have a hard length limit; fail early with advice
    /// rather than letting the daemon die at bind time.
    pub fn check_socket_path_len(&self) -> Result<()> {
        use std::os::unix::ffi::OsStrExt;
        let len = self.socket_path().as_os_str().as_bytes().len();
        if len > MAX_SOCKET_PATH {
            anyhow::bail!(
                "state directory path is too long for a unix socket ({len} bytes, max {MAX_SOCKET_PATH}): {}\n\
                 set SELX_STATE_DIR to a shorter directory (e.g. /tmp/selx-$USER)",
                self.dir.display()
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    #[test]
    fn base_dir_fallback_chain() {
        let tmp = tempfile::tempdir().unwrap();
        let some = |p: &Path| Some(p.as_os_str().to_os_string());

        // Explicit override wins, even over a usable runtime dir.
        assert_eq!(
            base_dir_from(some(tmp.path()), some(tmp.path())),
            tmp.path()
        );
        // A runtime dir that exists is used.
        assert_eq!(
            base_dir_from(None, some(tmp.path())),
            tmp.path().join("selx")
        );
        // No runtime dir, a vanished one, or an empty variable (macOS; some
        // WSL setups) all fall back to the per-user cache dir.
        let missing = tmp.path().join("gone");
        for rt in [None, some(&missing), Some(OsString::new())] {
            let p = base_dir_from(None, rt);
            assert!(p.is_absolute(), "{}", p.display());
            assert!(p.ends_with("selx/run"), "{}", p.display());
        }
    }

    #[test]
    fn enumeration_takes_the_dirs_that_name_a_root_and_ignores_the_debris() {
        let base = tempfile::tempdir().unwrap();
        let base = base.path();

        // A daemon's state dir: names a root.
        fs::create_dir(base.join("aaaa")).unwrap();
        fs::write(base.join("aaaa").join("root"), "/work/one\n").unwrap();
        // A second one, to show every match is returned.
        fs::create_dir(base.join("bbbb")).unwrap();
        fs::write(base.join("bbbb").join("root"), "/work/two").unwrap();
        // An empty `root` file names nothing: not a workspace.
        fs::create_dir(base.join("cccc")).unwrap();
        fs::write(base.join("cccc").join("root"), "\n").unwrap();
        // A directory with no `root` file at all, and a stray file.
        fs::create_dir(base.join("dddd")).unwrap();
        fs::write(base.join("loose"), "x").unwrap();

        let mut found: Vec<PathBuf> = StateDir::all_in(base).into_iter().map(|s| s.root).collect();
        found.sort();
        assert_eq!(
            found,
            vec![PathBuf::from("/work/one"), PathBuf::from("/work/two")]
        );
        // The trailing newline is stripped, not carried into the root.
        assert_eq!(
            StateDir::all_in(base)
                .iter()
                .find(|s| s.root == Path::new("/work/one"))
                .map(|s| s.dir.clone()),
            Some(base.join("aaaa"))
        );

        // A base dir that does not exist is "no daemons", not an error:
        // `selx stop --all` runs before anything has ever been started.
        assert!(StateDir::all_in(&base.join("never")).is_empty());
    }

    #[test]
    fn files_roundtrip_with_private_modes() {
        let base = tempfile::tempdir().unwrap();
        // Avoid touching the process-wide env: build the StateDir by hand.
        let root = PathBuf::from("/some/root");
        let sd = StateDir {
            root: root.clone(),
            dir: base.path().join("x").join(root_hash(&root)),
        };
        sd.create().unwrap();
        let mode = fs::metadata(&sd.dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        sd.write_pid(1234).unwrap();
        sd.write_port(23456).unwrap();
        sd.write_root().unwrap();
        assert_eq!(sd.read_pid(), Some(1234));
        assert_eq!(sd.read_port(), Some(23456));
        let mode = fs::metadata(sd.pid_file()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        sd.remove_runtime_files();
        assert_eq!(sd.read_pid(), None);
        assert!(sd.root_file().exists());
    }
}
