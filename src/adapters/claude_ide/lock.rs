//! The discovery lock file: `<config>/ide/<port>.lock`.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct LockFile {
    pub pid: u32,
    pub workspace_folders: Vec<PathBuf>,
    pub ide_name: String,
    pub transport: String,
    #[serde(default)]
    pub running_in_windows: bool,
    pub auth_token: String,
}

impl LockFile {
    pub fn new(root: PathBuf, ide_name: &str, auth_token: String) -> Self {
        LockFile {
            pid: std::process::id(),
            workspace_folders: vec![root],
            ide_name: ide_name.to_string(),
            transport: "ws".into(),
            running_in_windows: false,
            auth_token,
        }
    }
}

/// `$CLAUDE_CONFIG_DIR/ide` if set, else `~/.claude/ide`.
pub fn default_lock_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("CLAUDE_CONFIG_DIR").filter(|s| !s.is_empty()) {
        return PathBuf::from(d).join("ide");
    }
    crate::core::paths::home_dir().join(".claude").join("ide")
}

pub fn path(dir: &Path, port: u16) -> PathBuf {
    dir.join(format!("{port}.lock"))
}

/// Write atomically with 0600 permissions into a 0700 directory.
///
/// The file carries the auth token Claude presents, so the atomic private
/// write is [`crate::core::paths::write_private`] — the same one the state
/// files use. Only the directory's mode is this module's business.
pub fn write(dir: &Path, port: u16, lock: &LockFile) -> Result<PathBuf> {
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    // create_dir_all's mode only applies to new dirs; tighten an existing one.
    let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
    let final_path = path(dir, port);
    let json = serde_json::to_string(lock)?;
    crate::core::paths::write_private(&final_path, json.as_bytes())?;
    Ok(final_path)
}

pub fn read(dir: &Path, port: u16) -> Result<LockFile> {
    let p = path(dir, port);
    let s = fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))?;
    serde_json::from_str(&s).with_context(|| format!("parsing {}", p.display()))
}

pub fn remove(dir: &Path, port: u16) -> bool {
    fs::remove_file(path(dir, port)).is_ok()
}

/// Remove `<port>.lock` only if it belongs to a dead selx daemon: written
/// by `stale_pid`, or by our `ide_name` with a pid that no longer exists.
/// Never touches another IDE's live lock file on the same port.
pub fn remove_if_stale(dir: &Path, port: u16, stale_pid: Option<u32>, ide_name: &str) -> bool {
    let Ok(lock) = read(dir, port) else {
        return false;
    };
    let ours = Some(lock.pid) == stale_pid
        || (lock.ide_name == ide_name && !crate::core::process::pid_alive(lock.pid));
    ours && remove(dir, port)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_matches_observed_format() {
        let dir = tempfile::tempdir().unwrap();
        let lock = LockFile::new("/ws".into(), "selx", "tok".into());
        let p = write(dir.path(), 12345, &lock).unwrap();
        assert_eq!(p, dir.path().join("12345.lock"));
        let raw = fs::read_to_string(&p).unwrap();
        // Same keys the VS Code extension writes.
        for key in [
            "\"pid\"",
            "\"workspaceFolders\"",
            "\"ideName\"",
            "\"transport\":\"ws\"",
            "\"runningInWindows\":false",
            "\"authToken\":\"tok\"",
        ] {
            assert!(raw.contains(key), "{raw}");
        }
        assert_eq!(read(dir.path(), 12345).unwrap(), lock);
        assert_eq!(
            fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert!(!dir
            .path()
            .join(format!("12345.lock.tmp.{}", std::process::id()))
            .exists());
    }

    #[test]
    fn parses_real_vscode_lock_file() {
        let raw = r#"{"pid":134055,"workspaceFolders":["/home/me/Projects/selx"],"ideName":"Visual Studio Code","transport":"ws","runningInWindows":false,"authToken":"e3183c91-1582-4aa8-8a16-eea277993f33"}"#;
        let l: LockFile = serde_json::from_str(raw).unwrap();
        assert_eq!(l.ide_name, "Visual Studio Code");
        assert_eq!(
            l.workspace_folders,
            vec![PathBuf::from("/home/me/Projects/selx")]
        );
    }

    #[test]
    fn stale_removal_is_conservative() {
        let dir = tempfile::tempdir().unwrap();
        // A live process (ourselves) with a foreign ideName: never removed.
        let mut lock = LockFile::new("/ws".into(), "Visual Studio Code", "t".into());
        write(dir.path(), 1, &lock).unwrap();
        assert!(!remove_if_stale(dir.path(), 1, None, "selx"));
        assert!(!remove_if_stale(dir.path(), 1, Some(999_999), "selx"));
        assert!(path(dir.path(), 1).exists());
        // Foreign ideName but the pid is the one we know died: removed.
        assert!(remove_if_stale(dir.path(), 1, Some(lock.pid), "selx"));
        assert!(!path(dir.path(), 1).exists());
        // Our ideName with a dead pid: removed.
        lock.ide_name = "selx".into();
        lock.pid = i32::MAX as u32; // above any real pid_max, never alive
        write(dir.path(), 2, &lock).unwrap();
        assert!(remove_if_stale(dir.path(), 2, None, "selx"));
        // Our ideName but alive: kept.
        lock.pid = std::process::id();
        write(dir.path(), 3, &lock).unwrap();
        assert!(!remove_if_stale(dir.path(), 3, None, "selx"));
        // Missing file: false, no panic.
        assert!(!remove_if_stale(dir.path(), 4, None, "selx"));
    }
}
