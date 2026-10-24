//! `selx reload`: make the editor already running in this herdr workspace
//! re-read its buffers from disk, because something other than the user —
//! Claude, usually — changed the files underneath it.
//!
//! Helix (25.07.1) has no file watcher, so a buffer Claude has just edited
//! keeps showing the old text until someone types `:reload`. selx types it
//! instead, the way `selx open` types `:open`: into the editor's pane, over
//! herdr's socket. `selx claude` installs this as a Claude Code hook that
//! runs after every file edit and at the end of every turn.
//!
//! The one command Helix has for this, `:reload-all`, silently throws away
//! unsaved changes, and nothing in Helix will say which buffers have any.
//! The screen will: with `editor.bufferline = "always"` the top row lists
//! every buffer and marks the modified ones `[+]`. So before typing, this
//! reads the pane's screen and reloads only when the screen proves every
//! buffer is clean and the user is not typing into it ([`helix_verdict`]).
//! Anything it cannot prove is "not now": the reload is remembered, and
//! tried again at Claude's next edit or turn end.

use super::open::{self, Undeliverable};
use crate::config::{Config, ReloadMode};
use crate::core::StateDir;
use anyhow::Result;
use std::io::Read;
use std::path::{Path, PathBuf};

pub struct ReloadArgs {
    /// The editor binary whose pane to look for.
    pub editor: String,
    pub root: Option<PathBuf>,
    /// Read a Claude Code hook payload on stdin, and never fail.
    pub claude_hook: bool,
}

/// What the editor's screen says about typing a reload into it now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Reload,
    /// Not now, and why — phrased to follow "not reloading: ".
    Defer(&'static str),
}

/// What a Claude Code hook payload asks for.
#[derive(Debug, PartialEq, Eq)]
pub enum HookRequest {
    /// An editing tool changed this file.
    Edited(PathBuf),
    /// Claude finished its turn: time to catch up on a deferred reload.
    TurnEnded,
    /// Anything else, including a payload that is not one.
    Nothing,
}

pub fn run(args: ReloadArgs, verbose: u8) -> Result<u8> {
    super::init_cli_logging(verbose);
    if args.claude_hook {
        // A failing hook is reported to Claude or the user on every edit;
        // nothing that can go wrong here is worth that.
        hook(&args);
        return Ok(crate::exit::OK);
    }
    let root = super::resolve_root(args.root.as_deref())?;
    match attempt(&args.editor, &StateDir::for_root(&root)) {
        Ok(pane) => {
            tracing::info!(pane, "reloaded");
            Ok(crate::exit::OK)
        }
        Err(why) => {
            crate::warnln!("not reloading: {why}");
            Ok(crate::exit::ERROR)
        }
    }
}

/// Is reloading on for this workspace? The config file, then `SELX_RELOAD`.
pub fn enabled(root: &Path) -> Result<bool> {
    let mode = Config::load()?
        .for_root(root)
        .apply_process_env()?
        .reload_mode;
    Ok(mode.unwrap_or_default() == ReloadMode::On)
}

/// The Claude Code hook: everything is logged, nothing is an error.
fn hook(args: &ReloadArgs) {
    let mut payload = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut payload) {
        tracing::debug!("could not read the hook payload: {e}");
        return;
    }
    let request = hook_request(&payload);
    if request == HookRequest::Nothing {
        tracing::debug!("the hook payload asks for nothing");
        return;
    }
    let root = match super::resolve_root(args.root.as_deref()) {
        Ok(root) => root,
        Err(e) => {
            tracing::debug!("no workspace root: {e:#}");
            return;
        }
    };
    match enabled(&root) {
        Ok(true) => {}
        Ok(false) => {
            tracing::debug!("reload is off for {}", root.display());
            return;
        }
        Err(e) => {
            tracing::debug!("not reloading: {e:#}");
            return;
        }
    }
    let state = StateDir::for_root(&root);
    match request {
        HookRequest::Edited(file) => {
            // Claude may name the file through a symlink the root does not
            // go through (macOS's /tmp is /private/tmp).
            let file = std::fs::canonicalize(&file).unwrap_or(file);
            if !concerns_workspace(&file, &root) {
                tracing::debug!(file = %file.display(), "edit outside the workspace");
                return;
            }
        }
        HookRequest::TurnEnded => {
            if !state.reload_pending_file().exists() {
                tracing::debug!("turn ended with no reload owed");
                return;
            }
        }
        HookRequest::Nothing => return,
    }
    match attempt(&args.editor, &state) {
        Ok(pane) => tracing::info!(pane, "reloaded"),
        Err(why) => tracing::info!("not reloading: {why}"),
    }
}

/// Read a hook payload. Pure.
///
/// `Edit` and `Write` name their file in `tool_input.file_path`,
/// `NotebookEdit` in `tool_input.notebook_path`; a relative one (never
/// seen, but not promised against) is taken against the payload's `cwd`.
/// A `Stop` is the end of a turn. Anything else — a tool with no file, a
/// hook event this does not handle, stdin that is not JSON — is nothing.
pub fn hook_request(payload: &str) -> HookRequest {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(payload) else {
        return HookRequest::Nothing;
    };
    let input = &v["tool_input"];
    if let Some(file) = input["file_path"]
        .as_str()
        .or(input["notebook_path"].as_str())
        .filter(|f| !f.is_empty())
    {
        let file = Path::new(file);
        return match v["cwd"].as_str() {
            Some(cwd) if file.is_relative() => HookRequest::Edited(Path::new(cwd).join(file)),
            _ => HookRequest::Edited(file.to_path_buf()),
        };
    }
    match v["hook_event_name"].as_str() {
        Some("Stop") => HookRequest::TurnEnded,
        _ => HookRequest::Nothing,
    }
}

/// Should a change to `file` reload the editor for `root`? Component-wise,
/// so `/w/selx-2` is not inside `/w/selx`. Pure.
///
/// An edit to `~/.claude/CLAUDE.md`, or to another worktree, is not this
/// workspace's editor's business.
pub fn concerns_workspace(file: &Path, root: &Path) -> bool {
    file.starts_with(root)
}

/// Why a reload did not happen.
#[derive(Debug)]
enum NotReloaded {
    /// The editor's screen said "not now".
    Deferred(&'static str),
    /// There was no editor to type into, or typing failed.
    Undeliverable(Undeliverable),
}

impl From<Undeliverable> for NotReloaded {
    fn from(u: Undeliverable) -> NotReloaded {
        NotReloaded::Undeliverable(u)
    }
}

impl std::fmt::Display for NotReloaded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NotReloaded::Deferred(why) => f.write_str(why),
            NotReloaded::Undeliverable(u) => write!(f, "{u}"),
        }
    }
}

/// Reload once, one process at a time per workspace, and remember whether
/// a reload is still owed.
///
/// Owed means the editor is there and stale: its screen said "not now",
/// or herdr failed mid-way. No editor at all owes nothing — one started
/// later reads the files fresh.
fn attempt(editor: &str, state: &StateDir) -> std::result::Result<String, NotReloaded> {
    // Held from reading the screen to the last keystroke: see
    // `StateDir::lock_reload`.
    let _lock = state.lock_reload().map_err(Undeliverable::Failed)?;
    let result = reload_once(editor);
    let owed = matches!(
        result,
        Err(NotReloaded::Deferred(_)) | Err(NotReloaded::Undeliverable(Undeliverable::Failed(_)))
    );
    let pending = state.reload_pending_file();
    let marked = if owed {
        crate::core::paths::write_private(&pending, b"")
    } else {
        match std::fs::remove_file(&pending) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    };
    if let Err(e) = marked {
        tracing::debug!("could not update {}: {e:#}", pending.display());
    }
    result
}

/// Find the editor, read its screen, and type the reload if it says yes.
fn reload_once(binary: &str) -> std::result::Result<String, NotReloaded> {
    let editor =
        open::editor(binary).ok_or_else(|| Undeliverable::NotDrivable(binary.to_string()))?;
    let (client, pane) = open::find_editor_pane(editor)?;
    let screen = client
        .read_visible(&pane.pane_id)
        .map_err(Undeliverable::Failed)?;
    if let Verdict::Defer(why) = (editor.reload_verdict)(&screen, pane.focused) {
        return Err(NotReloaded::Deferred(why));
    }
    open::type_commands(&client, &pane.pane_id, &[editor.reload_command.to_string()])
        .map_err(Undeliverable::Failed)?;
    Ok(pane.pane_id)
}

// ---- reading a Helix screen -------------------------------------------------

/// May `:reload-all` be typed into this Helix now? Pure.
///
/// `screen` is the pane's visible text, one row per line; `focused` is
/// whether the user's keyboard is on the pane. The principle is that the
/// screen must *prove* the reload safe — `:reload-all` reaches buffers
/// that are not on screen, so "no `[+]` in sight" is not enough on its
/// own. It reloads only when:
///
/// 1. there is a Helix statusline (the last row, or the one above a
///    message) — a prompt replaces it, a custom layout is unreadable;
/// 2. the first row is a bufferline: no line number of its own, a line
///    number on the row under it, made up entirely of ` name ` labels,
///    one of them the file the statusline names;
/// 3. the bufferline cannot be cut off: labels that do not fit are not
///    drawn, and the one that reaches the right edge is clipped (maybe
///    through its `[+]`), so a bufferline as wide as the widest row may
///    be hiding buffers;
/// 4. no buffer is marked `[+]`;
/// 5. and, when the pane has the user's keyboard, they are not typing into
///    it: normal mode, no message or popup on screen.
///
/// Every rule errs towards "not now", whose cost is a reload that comes
/// later.
pub fn helix_verdict(screen: &str, focused: bool) -> Verdict {
    use Verdict::Defer;
    let rows: Vec<&str> = screen.lines().map(str::trim_end).collect();
    let Some(last) = rows.iter().rposition(|r| !r.is_empty()) else {
        return Defer("the editor's screen is empty");
    };
    let Some((at, status)) = [last, last.saturating_sub(1)]
        .into_iter()
        .find_map(|i| helix_status(rows[i]).map(|s| (i, s)))
    else {
        return Defer("no Helix statusline on screen (a prompt is open, or it is customized)");
    };

    let (top, second) = (rows[0], rows.get(1).copied().unwrap_or(""));
    let file = status.file.rsplit('/').next().unwrap_or(status.file);
    let labels =
        match bufferline_labels(top) {
            Some(labels)
                if !has_line_number(top)
                    && has_line_number(second)
                    && labels.iter().any(|l| l.name == file) =>
            {
                labels
            }
            _ => return Defer(
                "no bufferline to check for unsaved buffers (set editor.bufferline = \"always\")",
            ),
        };
    let widest = rows[1..]
        .iter()
        .map(|r| r.chars().count())
        .max()
        .unwrap_or(0);
    if max_columns(top) + 2 >= widest {
        return Defer("the bufferline may be cut off, hiding buffers");
    }
    if status.modified || labels.iter().any(|l| l.modified) {
        return Defer("a buffer has unsaved changes");
    }
    if focused {
        if !status.normal {
            return Defer("you are typing in the editor");
        }
        if at != last {
            return Defer("the editor is showing a message");
        }
        if rows.iter().any(|r| r.contains('┌')) {
            return Defer("a picker or popup is open in the editor");
        }
    }
    Verdict::Reload
}

/// The parts of Helix's default statusline this reads.
struct Status<'a> {
    normal: bool,
    /// As the statusline shows it: relative to Helix's working directory.
    file: &'a str,
    modified: bool,
}

/// ` NOR   docs/plan.md [+]      1 sel  91:1` → mode, file, modified.
fn helix_status(row: &str) -> Option<Status<'_>> {
    let mut words = row.split_whitespace();
    let mode = words.next()?;
    if !["NOR", "INS", "SEL"].contains(&mode) {
        return None;
    }
    // While a language server is busy a one-glyph spinner sits between the
    // mode and the file name.
    let file = words.find(|w| !is_spinner(w))?;
    Some(Status {
        normal: mode == "NOR",
        file,
        modified: row.split_whitespace().any(|w| w == "[+]"),
    })
}

fn is_spinner(word: &str) -> bool {
    let mut chars = word.chars();
    matches!((chars.next(), chars.next()), (Some(c), None) if !c.is_ascii())
}

/// One bufferline entry.
struct Label<'a> {
    name: &'a str,
    modified: bool,
}

/// ` a.txt[+]  b.txt` → its labels, or `None` if the row is not shaped
/// like a bufferline. Helix draws each buffer as ` name ` or ` name[+] `
/// (the basename), side by side.
fn bufferline_labels(row: &str) -> Option<Vec<Label<'_>>> {
    row.strip_prefix(' ')?
        .split("  ")
        .map(|part| {
            if part.is_empty() || part.starts_with(' ') || part.ends_with(' ') {
                return None;
            }
            Some(match part.strip_suffix("[+]") {
                Some(name) => Label {
                    name,
                    modified: true,
                },
                None => Label {
                    name: part,
                    modified: false,
                },
            })
        })
        .collect()
}

/// Does this row start with Helix's line-number gutter — `   96  text`,
/// with a diagnostic glyph (`●`) in front or a diff mark behind?
fn has_line_number(row: &str) -> bool {
    let rest = row.trim_start_matches(|c: char| c == ' ' || !c.is_ascii());
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    digits > 0 && matches!(rest.as_bytes().get(digits), None | Some(b' '))
}

/// The most columns a row can take: anything outside ASCII may be drawn
/// double-width. An overestimate is the safe side for the bufferline.
fn max_columns(row: &str) -> usize {
    row.chars().map(|c| if c.is_ascii() { 1 } else { 2 }).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Screens as `pane.read` returns them from Helix 25.07.1 in a 120x40
    // herdr pane with `bufferline = "always"`, captured while writing this.

    /// A statusline as Helix draws one across the pane.
    fn status(left: &str, right: &str) -> String {
        format!(
            "{left}{}{right}",
            " ".repeat(118 - left.len() - right.len())
        )
    }

    /// A 40-row screen: `top`, then `body`, blank rows, the statusline on
    /// row 38 and `message` (often empty) on row 39.
    fn screen(top: &str, body: &[&str], statusline: &str, message: &str) -> String {
        let mut rows: Vec<String> = std::iter::once(top)
            .chain(body.iter().copied())
            .map(String::from)
            .collect();
        rows.resize(38, String::new());
        rows.push(statusline.to_string());
        rows.push(message.to_string());
        rows.join("\n")
    }

    const TEXT: &[&str] = &["    1  alpha", "    2  beta", "    ~"];

    fn on_a(top: &str, left: &str) -> String {
        screen(top, TEXT, &status(left, "1 sel  1:1"), "")
    }

    fn clean() -> String {
        on_a(" a.txt  b.txt", " NOR   a.txt")
    }

    #[test]
    fn a_clean_screen_reloads_whether_or_not_the_user_is_on_it() {
        assert_eq!(helix_verdict(&clean(), false), Verdict::Reload);
        assert_eq!(helix_verdict(&clean(), true), Verdict::Reload);
        // The same with the file named by a path, as the statusline shows
        // it relative to Helix's working directory.
        let s = on_a(" NOTES.md  main.rs", " NOR   src/main.rs");
        assert_eq!(helix_verdict(&s, true), Verdict::Reload);
    }

    #[test]
    fn a_modified_buffer_anywhere_defers() {
        // On b.txt, with a.txt modified in the background: only the
        // bufferline knows.
        let s = on_a(" a.txt[+]  b.txt", " NOR   b.txt");
        assert_eq!(
            helix_verdict(&s, false),
            Verdict::Defer("a buffer has unsaved changes")
        );
        // The focused buffer, as both show it.
        let s = on_a(" a.txt[+]  b.txt", " NOR   a.txt [+]");
        assert!(matches!(helix_verdict(&s, false), Verdict::Defer(_)));
        // The statusline alone is enough.
        let s = on_a(" a.txt  b.txt", " NOR   a.txt [+]");
        assert!(matches!(helix_verdict(&s, false), Verdict::Defer(_)));
    }

    #[test]
    fn typing_defers_only_while_the_user_is_on_the_pane() {
        for mode in ["INS", "SEL"] {
            let s = on_a(" a.txt  b.txt", &format!(" {mode}   a.txt"));
            assert_eq!(
                helix_verdict(&s, true),
                Verdict::Defer("you are typing in the editor"),
                "{mode}"
            );
            // Nobody is typing into a pane without the keyboard, and with
            // nothing typed the Esc in front of the reload costs nothing.
            assert_eq!(helix_verdict(&s, false), Verdict::Reload, "{mode}");
        }
    }

    #[test]
    fn a_message_or_a_popup_defers_while_the_user_is_on_the_pane() {
        let s = screen(
            " a.txt  b.txt",
            TEXT,
            &status(" NOR   a.txt", "1 sel  1:1"),
            "Loaded 2 files.",
        );
        assert_eq!(helix_verdict(&s, false), Verdict::Reload);
        assert!(matches!(helix_verdict(&s, true), Verdict::Defer(_)));

        let picker = [
            "    1  one",
            "    2 ┌──────────────┐┌──────────────┐",
            "    ~ │         33/33 ││ Xalpha       │",
            "      │ > a.txt       ││ beta         │",
            "      └──────────────┘└──────────────┘",
        ];
        let s = screen(
            " a.txt  b.txt",
            &picker,
            &status(" NOR   b.txt", "1 sel  1:1"),
            "",
        );
        assert_eq!(
            helix_verdict(&s, true),
            Verdict::Defer("a picker or popup is open in the editor")
        );
        assert_eq!(helix_verdict(&s, false), Verdict::Reload);
    }

    #[test]
    fn an_open_prompt_hides_the_statusline_and_defers() {
        // `:rel` typed: the completion menu covers the statusline.
        let mut rows = vec![" a.txt[+]  b.txt", "    1  one", "    2  two", "    ~"];
        rows.resize(33, "");
        rows.extend([
            "reload                                 run-shell-command",
            "reload-all                             write-all",
            ":rel",
        ]);
        let s = rows.join("\n");
        assert!(matches!(helix_verdict(&s, false), Verdict::Defer(_)));
        assert!(matches!(helix_verdict("", false), Verdict::Defer(_)));
    }

    #[test]
    fn without_a_bufferline_nothing_proves_the_other_buffers_clean() {
        let no_bufferline = "no bufferline to check for unsaved buffers \
                             (set editor.bufferline = \"always\")";
        // `bufferline = "never"`: the first row is the file.
        let s = screen(
            "    1  alpha",
            &["    2  beta"],
            &status(" NOR   a.txt", "1 sel  1:1"),
            "",
        );
        assert_eq!(helix_verdict(&s, false), Verdict::Defer(no_bufferline));
        // A file whose first line looks exactly like a bufferline still has
        // its line number in front of it.
        let s = screen(
            "    1  a.txt  b.txt",
            &["    2  beta"],
            &status(" NOR   a.txt", "1 sel  1:1"),
            "",
        );
        assert_eq!(helix_verdict(&s, false), Verdict::Defer(no_bufferline));
        // Line numbers turned off (or the view starting on a soft-wrapped
        // continuation): the row under the "bufferline" has no number, so
        // there is no telling the two apart.
        let s = screen(
            " a.txt  b.txt",
            &["alpha", "beta"],
            &status(" NOR   a.txt", "1 sel  1:1"),
            "",
        );
        assert_eq!(helix_verdict(&s, false), Verdict::Defer(no_bufferline));
        // Label-shaped, but not naming the file on screen.
        let s = on_a(" x.txt  y.txt", " NOR   a.txt");
        assert_eq!(helix_verdict(&s, false), Verdict::Defer(no_bufferline));
        // A name with a space in it cannot be told from two words.
        let s = on_a(" my file.txt  b.txt", " NOR   my file.txt");
        assert_eq!(helix_verdict(&s, false), Verdict::Defer(no_bufferline));
    }

    #[test]
    fn a_bufferline_that_may_be_cut_off_defers() {
        let cut = "the bufferline may be cut off, hiding buffers";
        // 30 buffers in 120 columns, as captured: the last label is clipped
        // mid-name, and the eight after it are not drawn at all.
        let top = " a.txt[+]  b.txt  long-file-name-number-1.txt  long-file-name-number-2.txt  \
                   long-file-name-number-3.txt  long-file-name";
        let s = on_a(top, " NOR   a.txt");
        assert_eq!(helix_verdict(&s, false), Verdict::Defer(cut));
        // The same, clean as far as can be seen: the clip could have taken
        // a `[+]`, or buffers past the edge could have one.
        let top = " a.txt  b.txt  long-file-name-number-1.txt  long-file-name-number-2.txt  \
                   long-file-name-number-3.txt  long-file-name-nu";
        let s = on_a(top, " NOR   a.txt");
        assert_eq!(helix_verdict(&s, false), Verdict::Defer(cut));
        // Double-width names are counted as such.
        let wide = format!(" a.txt  {}", "漢".repeat(56));
        let s = on_a(&wide, " NOR   a.txt");
        assert_eq!(helix_verdict(&s, false), Verdict::Defer(cut));
    }

    #[test]
    fn scratch_buffers_and_the_spinner_are_read_through() {
        let s = screen(
            " [scratch]",
            &["    1"],
            &status(" NOR   [scratch]", "1 sel  1:1"),
            "",
        );
        assert_eq!(helix_verdict(&s, true), Verdict::Reload);
        let s = on_a(" a.txt  b.txt", " NOR ⣾ a.txt");
        assert_eq!(helix_verdict(&s, true), Verdict::Reload);
    }

    #[test]
    fn line_numbers_are_told_from_file_names() {
        for row in [
            "    1  alpha",
            "   96  text",
            "  12",
            "● 7  x",
            "   96 ▍changed",
        ] {
            assert!(has_line_number(row), "{row:?}");
        }
        for row in [" a.txt  b.txt", " 2024-notes.md", "    ~", "", " [scratch]"] {
            assert!(!has_line_number(row), "{row:?}");
        }
    }

    fn payload(v: serde_json::Value) -> String {
        v.to_string()
    }

    #[test]
    fn hook_payloads_name_the_edited_file() {
        for (tool, key) in [
            ("Edit", "file_path"),
            ("Write", "file_path"),
            ("NotebookEdit", "notebook_path"),
        ] {
            let p = payload(serde_json::json!({
                "session_id": "abc",
                "cwd": "/w/selx",
                "hook_event_name": "PostToolUse",
                "tool_name": tool,
                "tool_input": { key: "/w/selx/src/main.rs", "old_string": "a" },
                "tool_response": { "success": true },
            }));
            assert_eq!(
                hook_request(&p),
                HookRequest::Edited(PathBuf::from("/w/selx/src/main.rs")),
                "{tool}"
            );
        }
        // A relative path is taken against the session's directory.
        let p = payload(serde_json::json!({
            "cwd": "/w/selx",
            "hook_event_name": "PostToolUse",
            "tool_input": { "file_path": "src/main.rs" },
        }));
        assert_eq!(
            hook_request(&p),
            HookRequest::Edited(PathBuf::from("/w/selx/src/main.rs"))
        );
    }

    #[test]
    fn a_stop_ends_the_turn_and_everything_else_is_nothing() {
        let stop = payload(serde_json::json!({
            "session_id": "abc",
            "cwd": "/w/selx",
            "hook_event_name": "Stop",
            "stop_hook_active": false,
        }));
        assert_eq!(hook_request(&stop), HookRequest::TurnEnded);
        let bash = payload(serde_json::json!({
            "hook_event_name": "PostToolUse",
            "tool_name": "Bash",
            "tool_input": { "command": "cargo test" },
        }));
        assert_eq!(hook_request(&bash), HookRequest::Nothing);
        for junk in [
            "",
            "not json",
            "[]",
            "{}",
            r#"{"tool_input":{"file_path":""}}"#,
        ] {
            assert_eq!(hook_request(junk), HookRequest::Nothing, "{junk:?}");
        }
    }

    #[test]
    fn only_edits_inside_the_workspace_concern_it() {
        let root = Path::new("/w/selx");
        assert!(concerns_workspace(Path::new("/w/selx/src/main.rs"), root));
        assert!(concerns_workspace(Path::new("/w/selx"), root));
        assert!(!concerns_workspace(
            Path::new("/w/selx-2/src/main.rs"),
            root
        ));
        assert!(!concerns_workspace(
            Path::new("/home/me/.claude/CLAUDE.md"),
            root
        ));
    }
}
