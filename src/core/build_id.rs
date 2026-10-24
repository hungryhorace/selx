//! Which build of selx a process is running.
//!
//! (Not a cargo build script — those live at the package root.)
//!
//! The crate version alone cannot answer this. A daemon outlives the
//! command that started it, and installing over an existing binary leaves
//! the version untouched, so a long-lived daemon and a freshly installed
//! CLI both call themselves `0.1.0` while running different code — the
//! failure this module exists to catch. What does change is the executable
//! file, so a build id is the version plus the size and modification time
//! of the file the process was started from.
//!
//! Compute it early: a daemon whose binary is replaced under it can no
//! longer identify the file it came from.

use std::path::Path;
use std::time::UNIX_EPOCH;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Build id of the running process. Always answers something: when the
/// executable cannot be identified this is the bare version, which
/// [`differs`] treats as "unknown" rather than as a mismatch.
pub fn current() -> String {
    from_exe().unwrap_or_else(|| VERSION.to_string())
}

/// Build id from the executable file, or `None` if it cannot be stat'd.
fn from_exe() -> Option<String> {
    from_path(&std::env::current_exe().ok()?)
}

fn from_path(path: &Path) -> Option<String> {
    let md = std::fs::metadata(path).ok()?;
    let mtime = md.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
    Some(format!(
        "{VERSION}+{}-{}.{:09}",
        md.len(),
        mtime.as_secs(),
        mtime.subsec_nanos()
    ))
}

/// Is `theirs` a different build from ours? `None` means the question
/// cannot be answered, and an unanswerable question must never be read as
/// a mismatch: repeatedly "correcting" a difference we cannot see would
/// restart a healthy daemon on every command.
///
/// Unidentified builds are the ones we decline to judge — both a process
/// that cannot stat its own executable and one that cannot stat ours. The
/// single exception is an empty id, which only a daemon predating this
/// check can send, and which is therefore certainly out of date.
pub fn differs(theirs: &str) -> Option<bool> {
    if theirs.is_empty() {
        return Some(true);
    }
    if !theirs.contains('+') {
        return None;
    }
    Some(theirs != from_exe()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_id_names_the_version_and_the_file() {
        let mine = current();
        assert!(mine.starts_with(VERSION), "{mine}");
        // The test binary exists, so this must be the fingerprint form.
        assert!(mine.contains('+'), "{mine}");
        assert_eq!(mine, current(), "the same binary must id the same twice");
    }

    #[test]
    fn a_file_that_changes_gets_a_new_id() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("selx");
        std::fs::write(&f, "one").unwrap();
        let first = from_path(&f).unwrap();
        std::fs::write(&f, "two but longer").unwrap();
        assert_ne!(first, from_path(&f).unwrap());
        assert_eq!(from_path(&dir.path().join("absent")), None);
    }

    #[test]
    fn only_a_known_different_build_counts_as_a_difference() {
        assert_eq!(differs(&current()), Some(false));
        assert_eq!(differs("0.0.1+1-2.000000003"), Some(true));
        // A daemon from before this check existed.
        assert_eq!(differs(""), Some(true));
        // A daemon that could not identify itself: not our place to judge.
        assert_eq!(differs(VERSION), None);
    }
}
