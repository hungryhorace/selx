//! Filesystem locations selx and its adapters need, and the one private
//! write every one of them uses.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// The user's home directory.
///
/// `directories` first, `$HOME` second, `.` last. The fallback chain
/// matters: several callers build a *default* path from this and would
/// otherwise have to decide separately what to do without a home, and a
/// relative path that fails to open is a better outcome than a panic in a
/// daemon that has already started.
pub fn home_dir() -> PathBuf {
    directories::BaseDirs::new()
        .map(|b| b.home_dir().to_path_buf())
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Write `contents` to `path` atomically, readable only by this user.
///
/// Every file selx owns goes through here: state files, and the Claude
/// discovery lock file with its auth token. Written to a pid-suffixed
/// temporary with `create_new` and 0600, fsynced, then renamed — so a
/// reader sees either the old file or the whole new one, never a partial
/// one, and two processes writing the same path cannot share a temporary.
pub fn write_private(path: &Path, contents: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let dir = path.parent().unwrap_or(Path::new("."));
    let name = path.file_name().unwrap_or(std::ffi::OsStr::new("selx"));
    let tmp = dir.join(format!(
        "{}.tmp.{}",
        name.to_string_lossy(),
        std::process::id()
    ));
    // A leftover temporary from a killed process must not fail the write,
    // but `create_new` is what keeps a *live* writer from sharing ours.
    let _ = std::fs::remove_file(&tmp);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .with_context(|| format!("creating {}", tmp.display()))?;
    if let Err(e) = f.write_all(contents).and_then(|()| f.sync_all()) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("writing {}", tmp.display()));
    }
    drop(f);
    std::fs::rename(&tmp, path).with_context(|| {
        let _ = std::fs::remove_file(&tmp);
        format!("renaming into {}", path.display())
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn a_private_write_replaces_the_file_and_leaves_no_temporary() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("secret");

        write_private(&p, b"first").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "first");
        assert_eq!(
            std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );

        // Overwriting an existing file is a rename over it, not a failure.
        write_private(&p, b"second").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "second");

        // Only the file itself is left behind.
        let left: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(left, vec!["secret".to_string()]);
    }

    #[test]
    fn a_leftover_temporary_does_not_block_the_next_write() {
        // A process killed mid-write leaves its temporary behind; the same
        // pid comes round again eventually.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("pid");
        let stale = dir.path().join(format!("pid.tmp.{}", std::process::id()));
        std::fs::write(&stale, "junk").unwrap();
        write_private(&p, b"1234\n").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "1234\n");
        assert!(!stale.exists());
    }

    #[test]
    fn an_unwritable_directory_is_an_error_naming_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let e = write_private(&dir.path().join("no/such/dir/file"), b"x").unwrap_err();
        assert!(format!("{e:#}").contains("no/such/dir"), "{e:#}");
    }

    #[test]
    fn home_is_absolute_when_there_is_one() {
        let home = home_dir();
        assert!(
            home.is_absolute() || home == Path::new("."),
            "{}",
            home.display()
        );
    }
}
