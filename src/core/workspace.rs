//! Workspace-root resolution and the deterministic root → port mapping.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Resolve the workspace root per the single rule used everywhere:
/// explicit `--root` if given; else the git worktree root of `start`;
/// else `start` itself. Always canonicalized.
///
/// The git probe degrades gracefully: no `git` binary or a non-repo folder is
/// the fallback path, not an error.
pub fn resolve_root(explicit: Option<&Path>, start: &Path) -> Result<PathBuf> {
    if let Some(r) = explicit {
        return canonical(r)
            .with_context(|| format!("--root {} is not a usable directory", r.display()));
    }
    if let Some(top) = git_toplevel(start) {
        return Ok(top);
    }
    canonical(start).with_context(|| format!("{} is not a usable directory", start.display()))
}

/// Root resolution for `selx send`: start from the file's directory; if that
/// is outside any repository, fall back to `cwd`'s resolution (a file's own
/// directory is not a stable pairing key).
pub fn resolve_root_for_file(explicit: Option<&Path>, file: &Path, cwd: &Path) -> Result<PathBuf> {
    if let Some(r) = explicit {
        return resolve_root(Some(r), cwd);
    }
    let dir = if file.is_dir() {
        Some(file)
    } else {
        file.parent()
    };
    if let Some(dir) = dir.filter(|d| d.is_dir()) {
        if let Some(top) = git_toplevel(dir) {
            return Ok(top);
        }
    }
    resolve_root(None, cwd)
}

/// `git rev-parse --show-toplevel` for `dir`, canonicalized. `None` if git is
/// missing, `dir` is not inside a work tree, or anything else goes wrong.
pub fn git_toplevel(dir: &Path) -> Option<PathBuf> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--show-toplevel"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    let s = s.trim_end_matches(['\n', '\r']);
    if s.is_empty() {
        return None;
    }
    canonical(Path::new(s)).ok()
}

fn canonical(p: &Path) -> Result<PathBuf> {
    let c = std::fs::canonicalize(p)?;
    if !c.is_dir() {
        anyhow::bail!("not a directory");
    }
    Ok(c)
}

/// Make `p` absolute against `cwd` and canonicalize when it exists; when it
/// does not exist (deleted file, unsaved buffer) canonicalize the longest
/// existing prefix and append the rest lexically.
pub fn absolutize(p: &Path, cwd: &Path) -> PathBuf {
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    };
    if let Ok(c) = std::fs::canonicalize(&abs) {
        return c;
    }
    // Resolve `.`/`..` lexically first so the walk below only ever sees
    // plain components (a path ending in `..` has no `file_name`).
    let abs = lexical_normalize(&abs);
    let mut existing = abs.clone();
    let mut rest = Vec::new();
    loop {
        if let Ok(mut out) = std::fs::canonicalize(&existing) {
            for part in rest.into_iter().rev() {
                out.push(part);
            }
            return out;
        }
        match (existing.file_name(), existing.parent()) {
            (Some(name), Some(parent)) => {
                rest.push(name.to_os_string());
                existing = parent.to_path_buf();
            }
            _ => return abs,
        }
    }
}

fn lexical_normalize(p: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

// ---- hashing -------------------------------------------------------------

/// FNV-1a 64-bit. Inline rather than a dependency: the mapping from root to
/// port must be stable across Rust versions and builds, which std's
/// `DefaultHasher` does not guarantee.
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut h = OFFSET;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(PRIME);
    }
    h
}

/// Hash of the canonical root path, as 16 lowercase hex chars. Names the
/// per-root state directory.
pub fn root_hash(root: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    format!("{:016x}", fnv1a64(root.as_os_str().as_bytes()))
}

/// Same range the IDE extensions use.
pub const PORT_MIN: u16 = 10000;
pub const PORT_MAX: u16 = 65535;
const PORT_SPAN: u64 = (PORT_MAX - PORT_MIN) as u64 + 1;

/// The `n`-th candidate port for `root`: `n = 0` is the preferred port, the
/// one every client derives independently. On collision the daemon probes
/// upward from it via [`port_sequence_from`], which is the same order.
pub fn port_candidate(root: &Path, n: u32) -> u16 {
    use std::os::unix::ffi::OsStrExt;
    let h = fnv1a64(root.as_os_str().as_bytes());
    PORT_MIN + ((h.wrapping_add(n as u64)) % PORT_SPAN) as u16
}

/// The probe order: `first`, then upward, wrapping back to [`PORT_MIN`]
/// past [`PORT_MAX`]. The one definition of "next candidate" — the daemon
/// binds by walking this.
pub fn port_sequence_from(first: u16) -> impl Iterator<Item = u16> {
    let mut port = first;
    std::iter::from_fn(move || {
        let cur = port;
        port = if port == PORT_MAX { PORT_MIN } else { port + 1 };
        Some(cur)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnv_known_vectors() {
        // Standard FNV-1a 64 test vectors.
        assert_eq!(fnv1a64(b""), 0xcbf29ce484222325);
        assert_eq!(fnv1a64(b"a"), 0xaf63dc4c8601ec8c);
        assert_eq!(fnv1a64(b"foobar"), 0x85944171f73967e8);
    }

    #[test]
    fn port_is_stable_and_in_range() {
        // Pinned: if this changes, every existing user's port changes.
        assert_eq!(port_candidate(Path::new("/home/user/project"), 0), 60983);
        assert_eq!(
            root_hash(Path::new("/home/user/project")),
            "f8a71a04e8340307"
        );
        for i in 0..1000 {
            let p = port_candidate(Path::new(&format!("/tmp/ws{i}")), 0);
            assert!((PORT_MIN..=PORT_MAX).contains(&p));
        }
    }

    #[test]
    fn ports_are_spread() {
        let mut seen = std::collections::HashSet::new();
        for i in 0..500 {
            seen.insert(port_candidate(Path::new(&format!("/home/u/repo-{i}")), 0));
        }
        assert!(
            seen.len() > 480,
            "poor spread: {} unique of 500",
            seen.len()
        );
    }

    #[test]
    fn probe_sequence_wraps_within_range() {
        let first = port_candidate(Path::new("/x"), 0);
        let seq: Vec<u16> = port_sequence_from(first).take(100_000).collect();
        assert!(seq.iter().all(|p| (PORT_MIN..=PORT_MAX).contains(p)));
        assert_eq!(seq[0], first);
        assert_eq!(
            seq[1],
            if first == PORT_MAX {
                PORT_MIN
            } else {
                first + 1
            }
        );
        // Wraps rather than overflowing, and comes back round.
        let from_top: Vec<u16> = port_sequence_from(PORT_MAX).take(2).collect();
        assert_eq!(from_top, vec![PORT_MAX, PORT_MIN]);
        assert_eq!(seq[PORT_SPAN as usize], first);
    }

    #[test]
    fn non_git_folder_falls_back_to_itself() {
        let tmp = tempfile::tempdir().unwrap();
        // Make sure the temp dir isn't inside a repo (CI checkouts sometimes put TMPDIR in-tree).
        if git_toplevel(tmp.path()).is_some() {
            return;
        }
        let root = resolve_root(None, tmp.path()).unwrap();
        assert_eq!(root, std::fs::canonicalize(tmp.path()).unwrap());
    }

    #[test]
    fn subfolder_pairs_with_worktree_root() {
        let tmp = tempfile::tempdir().unwrap();
        let ok = Command::new("git")
            .args(["init", "-q"])
            .arg(tmp.path())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            eprintln!("git unavailable; skipping");
            return;
        }
        let sub = tmp.path().join("a/b");
        std::fs::create_dir_all(&sub).unwrap();
        let expected = std::fs::canonicalize(tmp.path()).unwrap();
        assert_eq!(resolve_root(None, &sub).unwrap(), expected);

        // File inside the repo → repo root, regardless of cwd.
        let f = sub.join("x.rs");
        std::fs::write(&f, "").unwrap();
        assert_eq!(
            resolve_root_for_file(None, &f, Path::new("/")).unwrap(),
            expected
        );

        // Explicit root wins.
        let other = tempfile::tempdir().unwrap();
        assert_eq!(
            resolve_root_for_file(Some(other.path()), &f, Path::new("/")).unwrap(),
            std::fs::canonicalize(other.path()).unwrap()
        );
    }

    #[test]
    fn absolutize_handles_missing_files() {
        let tmp = tempfile::tempdir().unwrap();
        let canon = std::fs::canonicalize(tmp.path()).unwrap();
        let p = absolutize(Path::new("sub/../gone.rs"), tmp.path());
        assert_eq!(p, canon.join("gone.rs"));
        let p = absolutize(Path::new("./deep/er/gone.rs"), &canon);
        assert_eq!(p, canon.join("deep/er/gone.rs"));
    }

    /// macOS temp dirs live under `/var` → `/private/var`; reproduce that
    /// with a symlinked directory so the case is covered on Linux too.
    #[test]
    fn absolutize_canonicalizes_symlinked_prefix_of_missing_files() {
        let tmp = tempfile::tempdir().unwrap();
        let canon = std::fs::canonicalize(tmp.path()).unwrap();
        std::fs::create_dir(canon.join("real")).unwrap();
        std::os::unix::fs::symlink(canon.join("real"), canon.join("link")).unwrap();
        let via_link = canon.join("link");

        // Missing file directly under the symlink.
        assert_eq!(
            absolutize(Path::new("gone.rs"), &via_link),
            canon.join("real/gone.rs")
        );
        // `..` in the missing part must not defeat canonicalization.
        assert_eq!(
            absolutize(Path::new("sub/../gone.rs"), &via_link),
            canon.join("real/gone.rs")
        );
        // Existing file through the symlink resolves fully.
        std::fs::write(canon.join("real/here.rs"), "").unwrap();
        assert_eq!(
            absolutize(Path::new("here.rs"), &via_link),
            canon.join("real/here.rs")
        );
    }
}
