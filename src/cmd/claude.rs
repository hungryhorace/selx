//! `selx claude`: run Claude Code wired to this workspace's daemon.

use crate::core::client::{Client, DEFAULT_TIMEOUT};
use crate::core::StateDir;
use anyhow::{Context, Result};
use std::ffi::{OsStr, OsString};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;

pub fn run(root: Option<&Path>, args: Vec<OsString>, verbose: u8) -> Result<u8> {
    super::init_cli_logging(verbose);
    let root = super::resolve_root(root)?;
    let ensured = super::ensure_root(&root)?;

    register_origin(&root);

    let bin = std::env::var_os("SELX_CLAUDE_BIN").unwrap_or_else(|| "claude".into());
    update_on_start(&bin);
    let reload = reload_settings(&root, &args)?;

    let mut cmd = Command::new(&bin);
    // Ahead of the user's own arguments, which may end in a `--`.
    if let Some(settings) = &reload {
        cmd.arg("--settings").arg(settings);
    }
    cmd.args(&args)
        .env("CLAUDE_CODE_SSE_PORT", ensured.port.to_string())
        .env("ENABLE_IDE_INTEGRATION", "true")
        // Updates happen above, before the session starts, never underneath
        // a running one.
        .env("DISABLE_AUTOUPDATER", "1");
    tracing::info!(port = ensured.port, root = %root.display(), "exec {}", Path::new(&bin).display());
    // Only returns on failure.
    let err = cmd.exec();
    Err(err).with_context(|| {
        format!(
            "could not run {}; set SELX_CLAUDE_BIN if it is not on PATH",
            Path::new(&bin).display()
        )
    })
}

/// Bring Claude up to date now, while nothing is running, so the session
/// about to start never has the binary swapped underneath it (its own
/// background updater is switched off in the environment we exec it with).
/// `SELX_CLAUDE_UPDATE=0` skips this. Best effort: an update that fails
/// (offline, no permission) is reported by `claude` itself, and the session
/// starts on whatever is installed.
fn update_on_start(bin: &OsStr) {
    if std::env::var_os("SELX_CLAUDE_UPDATE").is_some_and(|v| v == "0") {
        tracing::debug!("SELX_CLAUDE_UPDATE=0: not updating claude");
        return;
    }
    tracing::info!("running {} update", Path::new(bin).display());
    match Command::new(bin).arg("update").status() {
        Ok(status) if status.success() => {}
        Ok(status) => tracing::warn!("claude update exited with {status}; starting anyway"),
        // Not found is exec's error to explain, with the hint about SELX_CLAUDE_BIN.
        Err(e) => tracing::debug!("could not run claude update: {e}"),
    }
}

/// The tools after which the editor is reloaded: the ones that write a
/// file and say which. Bash is left out on purpose — it can change any
/// number of files without naming them, and a reload after every shell
/// command is noise.
pub const EDIT_TOOLS: &str = "Edit|Write|NotebookEdit";

/// Claude's budget for one hook run, in seconds. A reload is a handful of
/// herdr round trips, each capped at [`HerdrClient::DEFAULT_TIMEOUT`];
/// this only matters when herdr is wedged.
///
/// [`HerdrClient::DEFAULT_TIMEOUT`]: crate::adapters::herdr::HerdrClient::DEFAULT_TIMEOUT
const HOOK_TIMEOUT_SECS: u64 = 10;

/// The `--settings` that installs the editor reload hooks, when they are
/// wanted (see [`super::reload`]).
///
/// Not outside herdr: the hook finds the editor through herdr, so there it
/// could only ever do nothing, once per edit. Not when the user passed a
/// `--settings` of their own: it may carry these hooks already, and two
/// would reload twice. Not when `SELX_RELOAD` or `[reload] mode` says off.
fn reload_settings(root: &Path, args: &[OsString]) -> Result<Option<String>> {
    if crate::adapters::herdr::origin_from_env().is_none() {
        tracing::debug!("not in a herdr pane: no editor reload hooks");
        return Ok(None);
    }
    if has_settings_flag(args) {
        tracing::info!("--settings given: not adding the editor reload hooks");
        return Ok(None);
    }
    if !super::reload::enabled(root)? {
        tracing::debug!("reload is off: no editor reload hooks");
        return Ok(None);
    }
    let selx = std::env::current_exe().context("locating selx for Claude's hooks")?;
    let settings = hook_settings(&selx, root);
    if settings.is_none() {
        tracing::info!("selx or the workspace has a non-UTF-8 path: no editor reload hooks");
    }
    Ok(settings)
}

/// Claude Code settings JSON installing `selx reload --claude-hook` after
/// every file edit and at the end of every turn (which is when a reload the
/// editor's screen deferred gets another try). `None` when a path is not
/// UTF-8 and so cannot be written into JSON. Pure.
///
/// The executable is named by absolute path, because the shell Claude runs
/// hooks through may not have selx on its `PATH`; and the root is passed
/// explicitly, because the hook runs in whatever directory Claude is in.
/// The hooks are synchronous on purpose: two async runs could type into
/// the editor at once.
pub fn hook_settings(selx: &Path, root: &Path) -> Option<String> {
    let command = format!(
        "{} reload --claude-hook --root {}",
        sh_quote(selx.to_str()?),
        sh_quote(root.to_str()?)
    );
    let hook = serde_json::json!({
        "type": "command",
        "command": command,
        "timeout": HOOK_TIMEOUT_SECS,
    });
    let settings = serde_json::json!({
        "hooks": {
            "PostToolUse": [{ "matcher": EDIT_TOOLS, "hooks": [hook.clone()] }],
            "Stop": [{ "hooks": [hook] }],
        }
    });
    Some(settings.to_string())
}

/// `s` as one `sh` word.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Did the user pass `--settings` to Claude? Only options count: anything
/// after a `--` is an argument, even if it looks like one. Pure.
pub fn has_settings_flag(args: &[OsString]) -> bool {
    args.iter()
        .map(|a| a.to_string_lossy())
        .take_while(|a| a != "--")
        .any(|a| a == "--settings" || a.starts_with("--settings="))
}

/// Tell the daemon which pane this Claude will run in, before `exec`
/// replaces us: Claude itself knows nothing about the multiplexer, and the
/// daemon belongs to no pane, so this process is the only one that can say.
/// Best effort — nothing about running Claude depends on it, and a Claude
/// that never registers is still found by kind.
fn register_origin(root: &Path) {
    let Some(origin) = crate::adapters::herdr::origin_from_env() else {
        return;
    };
    let state = StateDir::for_root(root);
    let result = Client::connect_state(&state, DEFAULT_TIMEOUT)
        .and_then(|mut c| c.register_origin(origin.clone()));
    match result {
        Ok(()) => tracing::debug!(
            pane = origin.pane.as_deref().unwrap_or("-"),
            "registered pane"
        ),
        Err(e) => tracing::debug!("could not register this pane with the daemon: {e:#}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    #[test]
    fn the_hook_settings_run_this_selx_after_edits_and_at_turn_end() {
        let json = hook_settings(Path::new("/opt/my tools/selx"), Path::new("/w/it's")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let edit = &v["hooks"]["PostToolUse"][0];
        assert_eq!(edit["matcher"], "Edit|Write|NotebookEdit");
        let stop = &v["hooks"]["Stop"][0];
        assert!(stop.get("matcher").is_none(), "Stop takes no matcher");
        for entry in [edit, stop] {
            let hook = &entry["hooks"][0];
            assert_eq!(hook["type"], "command");
            // Synchronous: two async runs could type into Helix at once.
            assert!(hook.get("async").is_none());
            // Both paths survive `sh`: a space in one, a quote in the other.
            assert_eq!(
                hook["command"],
                r#"'/opt/my tools/selx' reload --claude-hook --root '/w/it'\''s'"#
            );
        }
    }

    #[test]
    fn a_settings_flag_of_the_users_own_is_spotted() {
        assert!(has_settings_flag(&args(&["--settings", "x.json"])));
        assert!(has_settings_flag(&args(&["--resume", "--settings={}"])));
        assert!(!has_settings_flag(&args(&[])));
        assert!(!has_settings_flag(&args(&["--resume", "my-settings.json"])));
        // After `--` it is the prompt, not an option.
        assert!(!has_settings_flag(&args(&["--", "--settings"])));
    }
}
