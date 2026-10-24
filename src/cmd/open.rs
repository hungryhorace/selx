//! `selx open`: put a file in the editor that is already running, instead
//! of starting a second one on top of whoever asked.
//!
//! lazygit's `e`, and anything else that shells out to an editor, runs a
//! fresh process in its own pane — so in a herdr workspace where an editor
//! is already open one tab over, you get a second editor stacked on top of
//! lazygit rather than a new buffer in the one you were working in.
//!
//! Helix has no server or remote-open mode (25.07.1: no `--remote`, no
//! socket), and neither do most terminal editors, so delivery here means
//! typing `:open <path>` into the pane the editor runs in, over herdr's
//! socket API, and moving focus to it.
//!
//! Everything this cannot deliver — no editor pane, not called from a herdr
//! pane, an editor selx cannot drive, arguments only a real editor process
//! can honour — `exec`s the editor with the caller's own arguments. The
//! command is a drop-in for the editor it fronts: at worst you get exactly
//! what you would have got without it.

use super::reload::Verdict;
use crate::adapters::herdr::{HerdrClient, HerdrPane};
use anyhow::{Context, Result};
use std::ffi::OsString;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// An editor selx can type into.
///
/// Adding one is a row here plus a look at [`quote`]: the command names
/// generalize, the quoting rules do not — Helix parses command arguments as
/// shellwords, where the vim family does its own backslash escaping.
pub struct Editor {
    /// What `--editor` names, and the binary `exec`d when delivery is off.
    pub binary: &'static str,
    /// Process names a pane running it reports. Helix is `hx` from most
    /// packages and `helix` from a few (snap installs both names).
    pub processes: &'static [&'static str],
    /// The editor's own command for opening a file in a new buffer.
    pub open_command: &'static str,
    /// The editor's own command for re-reading every buffer from disk.
    pub reload_command: &'static str,
    /// Whether typing `reload_command` now is safe, judged from the pane's
    /// screen and whether the user's keyboard is on it. See
    /// [`super::reload`] for why this has to be read off the screen.
    pub reload_verdict: fn(screen: &str, focused: bool) -> Verdict,
}

/// The editors selx knows how to drive. Anything not here can still be
/// named by `--editor`; it just always gets `exec`d.
pub const EDITORS: &[Editor] = &[Editor {
    binary: "hx",
    processes: &["hx", "helix"],
    open_command: "open",
    reload_command: "reload-all",
    reload_verdict: super::reload::helix_verdict,
}];

/// The editor `selx open` assumes: the one selx ships an integration for.
pub const DEFAULT_EDITOR: &str = "hx";

impl Editor {
    /// Is this pane's foreground process this editor?
    pub fn runs(&self, process: &str) -> bool {
        self.processes.contains(&process)
    }
}

/// The drivable editor `binary` names, if it is one.
pub fn editor(binary: &str) -> Option<&'static Editor> {
    let name = Path::new(binary).file_name()?.to_str()?;
    EDITORS.iter().find(|e| e.binary == name || e.runs(name))
}

pub struct OpenArgs {
    /// The editor binary: what to look for in a pane, and what to run when
    /// there is not one.
    pub editor: String,
    /// Files as the caller wrote them, `hx`-style: `FILE`, `FILE:LINE`,
    /// `FILE:LINE:COL`, `+LINE FILE`, and `--`.
    pub files: Vec<OsString>,
}

/// One file to open, as an editor command argument.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub path: PathBuf,
    /// `LINE` or `LINE:COL`, already in the `path:line:col` form both the
    /// `hx` command line and `:open` accept.
    pub position: Option<String>,
}

impl Target {
    /// The argument text: an absolute path, so it does not depend on the
    /// editor's working directory being the caller's.
    pub fn spec(&self) -> String {
        match &self.position {
            Some(p) => format!("{}:{p}", self.path.display()),
            None => self.path.display().to_string(),
        }
    }
}

/// The files named by `args`, resolved against `cwd`.
///
/// `None` means "this is not a request that can be retyped into a running
/// editor" — an option belongs to a real editor process, which is what the
/// caller gets instead. `+LINE` attaches to the first file, the way the
/// editor itself would apply it.
pub fn parse_targets(args: &[OsString], cwd: &Path) -> Option<Vec<Target>> {
    let mut targets: Vec<Target> = Vec::new();
    let mut plus: Option<String> = None;
    for arg in args {
        let arg = arg.to_str()?; // a non-UTF-8 path is the editor's problem
        if arg == "--" {
            continue;
        }
        if let Some(line) = arg.strip_prefix('+') {
            if line.chars().all(|c| c.is_ascii_digit()) && !line.is_empty() {
                plus = Some(line.to_string());
                continue;
            }
            return None;
        }
        if arg.starts_with('-') {
            return None;
        }
        // `path:line[:col]`, but only where the tail really is a position:
        // a colon is a legal character in a file name.
        let (path, position) = match arg.split_once(':') {
            Some((p, pos)) if is_position(pos) => (p, Some(pos.to_string())),
            _ => (arg, None),
        };
        targets.push(Target {
            path: cwd.join(path),
            position,
        });
    }
    if let (Some(line), Some(first)) = (plus, targets.first_mut()) {
        first.position.get_or_insert(line);
    }
    (!targets.is_empty()).then_some(targets)
}

/// `12` or `12:34` — the tail of a `path:line:col` argument.
fn is_position(s: &str) -> bool {
    let mut parts = s.split(':');
    let ok =
        |p: Option<&str>| p.is_some_and(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
    ok(parts.next()) && parts.next().is_none_or(|p| ok(Some(p))) && parts.next().is_none()
}

/// A path as one shellword for Helix's command line.
///
/// Helix splits command arguments as shellwords, so single quotes carry a
/// path with spaces in it and nothing inside them needs escaping. Two
/// characters cannot be written that way at all: a `'` has no escape inside
/// single quotes, and a `%` is Helix's command expansion (`%{…}`, `%sh{…}`)
/// wherever it appears. `None` for both, which sends the caller to a real
/// editor process rather than to a mangled path.
pub fn quote(spec: &str) -> Option<String> {
    (!spec.contains('\'') && !spec.contains('%')).then(|| format!("'{spec}'"))
}

/// Pick the pane to open in: one running `editor`, in the caller's tab if
/// there is one there.
///
/// The caller's own pane is never a candidate — an editor that shells out
/// to `selx open` would otherwise be told to open a file in itself, from
/// inside the process that is waiting for the answer.
///
/// Sharing a tab with the caller is the whole of the ranking, matching how
/// selx targets agent panes: the editor you can see from lazygit is the one
/// you meant, and a second editor in some other tab of the workspace is a
/// fallback, not a competitor. Ties keep herdr's own pane order.
pub fn choose_pane<'a>(
    panes: &'a [(HerdrPane, Vec<String>)],
    editor: &Editor,
    my_tab: Option<&str>,
    my_pane: Option<&str>,
) -> Option<&'a HerdrPane> {
    let mut candidates: Vec<&(HerdrPane, Vec<String>)> = panes
        .iter()
        .filter(|(pane, procs)| {
            Some(pane.pane_id.as_str()) != my_pane && procs.iter().any(|p| editor.runs(p))
        })
        .collect();
    // Stable, so the first pane herdr listed wins among equals.
    candidates.sort_by_key(|(pane, _)| {
        std::cmp::Reverse(my_tab.is_some() && pane.tab_id.as_deref() == my_tab)
    });
    candidates.first().map(|(pane, _)| pane)
}

/// Why nothing could be typed into a running editor. For `selx open` every
/// one of these ends in `exec`, and is worth an `-v` line saying which.
#[derive(Debug)]
pub(crate) enum Undeliverable {
    /// Options, or a path `:open` cannot be given.
    NotExpressible,
    /// `selx open` was not run from a herdr pane.
    NoHerdr,
    /// An editor selx cannot type into.
    NotDrivable(String),
    /// No pane in this workspace is running it.
    NoPane,
    /// herdr was there and the call failed anyway.
    Failed(anyhow::Error),
}

impl std::fmt::Display for Undeliverable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Undeliverable::NotExpressible => {
                write!(f, "these arguments need a real editor process")
            }
            Undeliverable::NoHerdr => write!(f, "not running in a herdr pane"),
            Undeliverable::NotDrivable(bin) => write!(f, "selx cannot type into {bin}"),
            Undeliverable::NoPane => write!(f, "no pane in this herdr workspace is running it"),
            Undeliverable::Failed(e) => write!(f, "{e:#}"),
        }
    }
}

pub fn run(args: OpenArgs, verbose: u8) -> Result<u8> {
    super::init_cli_logging(verbose);
    match deliver(&args) {
        Ok(pane) => {
            tracing::info!(pane, "opened in the running editor");
            Ok(crate::exit::OK)
        }
        Err(why) => {
            tracing::info!("running {} instead: {why}", args.editor);
            exec_editor(&args)
        }
    }
}

/// Type the files into the editor's pane and focus it; the pane on success.
fn deliver(args: &OpenArgs) -> std::result::Result<String, Undeliverable> {
    let editor =
        editor(&args.editor).ok_or_else(|| Undeliverable::NotDrivable(args.editor.clone()))?;
    let cwd = super::cwd().map_err(Undeliverable::Failed)?;
    let targets = parse_targets(&args.files, &cwd).ok_or(Undeliverable::NotExpressible)?;
    let quoted: Option<Vec<String>> = targets.iter().map(|t| quote(&t.spec())).collect();
    let quoted = quoted.ok_or(Undeliverable::NotExpressible)?;

    let (client, pane) = find_editor_pane(editor)?;
    let commands: Vec<String> = quoted
        .iter()
        .map(|spec| format!("{} {spec}", editor.open_command))
        .collect();
    let send = || -> Result<()> {
        type_commands(&client, &pane.pane_id, &commands)?;
        client.focus_pane(&pane.pane_id)
    };
    send().map_err(Undeliverable::Failed)?;
    Ok(pane.pane_id)
}

/// The pane in this herdr workspace running `editor`, and a client to
/// reach it — or why there is not one.
pub(crate) fn find_editor_pane(
    editor: &Editor,
) -> std::result::Result<(HerdrClient, HerdrPane), Undeliverable> {
    // Coordinates, not the socket, are what makes this a herdr request: a
    // pane that cannot say which workspace it is in has nothing to be local
    // to, and picking an editor out of some other workspace would drag the
    // user somewhere they were not looking.
    let var = |k: &str| std::env::var(k).ok().filter(|s| !s.is_empty());
    let workspace = var("HERDR_WORKSPACE_ID").ok_or(Undeliverable::NoHerdr)?;
    let client = HerdrClient::discover(None).ok_or(Undeliverable::NoHerdr)?;

    let panes = client
        .list_panes(Some(&workspace))
        .map_err(Undeliverable::Failed)?;
    let with_procs: Vec<(HerdrPane, Vec<String>)> = panes
        .into_iter()
        .map(|p| {
            // A pane that will not describe itself is not a candidate, and
            // not a reason to give up on the others.
            let procs = client.pane_processes(&p.pane_id).unwrap_or_default();
            (p, procs)
        })
        .collect();
    let pane = choose_pane(
        &with_procs,
        editor,
        var("HERDR_TAB_ID").as_deref(),
        var("HERDR_PANE_ID").as_deref(),
    )
    .ok_or(Undeliverable::NoPane)?
    .clone();
    Ok((client, pane))
}

/// Type each of `commands` into the editor's command line in `pane`.
///
/// Esc first: the editor may be in insert mode, a picker or a prompt, and
/// in normal mode it costs nothing. Then `:` as a *keypress* — sent as text
/// it would be pasted into the buffer instead of opening the command line —
/// and the command itself as text, which is what makes a path with spaces
/// in it survive.
pub(crate) fn type_commands(client: &HerdrClient, pane: &str, commands: &[String]) -> Result<()> {
    client.send_keys(pane, &["esc"])?;
    for command in commands {
        client.send_keys(pane, &[":"])?;
        client.send_text(pane, command)?;
        client.send_keys(pane, &["enter"])?;
    }
    Ok(())
}

/// Become the editor. Only returns if it could not be run at all.
fn exec_editor(args: &OpenArgs) -> Result<u8> {
    let err = Command::new(&args.editor).args(&args.files).exec();
    Err(err).with_context(|| format!("could not run {}", args.editor))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(id: &str, tab: &str) -> HerdrPane {
        HerdrPane {
            pane_id: id.to_string(),
            tab_id: Some(tab.to_string()),
            workspace_id: Some("w1".to_string()),
            focused: false,
        }
    }

    fn running(id: &str, tab: &str, procs: &[&str]) -> (HerdrPane, Vec<String>) {
        (pane(id, tab), procs.iter().map(|s| s.to_string()).collect())
    }

    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    fn helix() -> &'static Editor {
        editor("hx").unwrap()
    }

    #[test]
    fn a_relative_file_is_resolved_against_the_callers_directory() {
        // lazygit runs at the repo root and hands over a repo-relative
        // path; the editor pane may sit anywhere, so what is typed at it
        // has to be absolute.
        let got = parse_targets(&args(&["src/main.rs"]), Path::new("/repo")).unwrap();
        assert_eq!(got[0].spec(), "/repo/src/main.rs");
    }

    #[test]
    fn line_and_column_survive_as_the_editor_wrote_them() {
        let got = parse_targets(&args(&["a.rs:42", "b.rs:7:3"]), Path::new("/w")).unwrap();
        assert_eq!(got[0].spec(), "/w/a.rs:42");
        assert_eq!(got[1].spec(), "/w/b.rs:7:3");
    }

    #[test]
    fn a_colon_that_is_not_a_position_stays_part_of_the_name() {
        for name in ["od:d.txt", "a:b:c.txt", "x:12a"] {
            let got = parse_targets(&args(&[name]), Path::new("/w")).unwrap();
            assert_eq!(got[0].position, None, "{name}");
            assert_eq!(got[0].path, Path::new("/w").join(name), "{name}");
        }
    }

    #[test]
    fn plus_line_lands_on_the_first_file_only() {
        let got = parse_targets(&args(&["+7", "a.rs", "b.rs"]), Path::new("/w")).unwrap();
        assert_eq!(got[0].spec(), "/w/a.rs:7");
        assert_eq!(got[1].spec(), "/w/b.rs");
        // An explicit position on the file wins: it is the more specific of
        // the two, and the editor's own precedence.
        let got = parse_targets(&args(&["+7", "a.rs:9"]), Path::new("/w")).unwrap();
        assert_eq!(got[0].spec(), "/w/a.rs:9");
    }

    #[test]
    fn the_double_dash_separator_is_not_a_file() {
        let got = parse_targets(&args(&["--", "a.rs"]), Path::new("/w")).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].spec(), "/w/a.rs");
    }

    #[test]
    fn anything_optionlike_belongs_to_a_real_editor() {
        // These are not failures — they are the cases that must `exec`.
        for flags in [
            vec!["--vsplit", "a.rs"],
            vec!["-w", "/tmp", "a.rs"],
            vec!["--health"],
            vec!["+notanumber", "a.rs"],
        ] {
            assert!(
                parse_targets(&args(&flags), Path::new("/w")).is_none(),
                "{flags:?}"
            );
        }
        // And so is naming no file at all: `hx` with no arguments opens a
        // scratch buffer, which is not something to deliver anywhere.
        assert!(parse_targets(&[], Path::new("/w")).is_none());
    }

    #[test]
    fn quoting_carries_spaces_and_refuses_what_it_cannot_carry() {
        assert_eq!(quote("/w/a.rs").unwrap(), "'/w/a.rs'");
        assert_eq!(quote("/w/has space.rs").unwrap(), "'/w/has space.rs'");
        // No escape for a single quote inside single quotes, and `%` is a
        // command expansion wherever it appears — both go to `exec`.
        assert_eq!(quote("/w/it's.rs"), None);
        assert_eq!(quote("/w/100%.rs"), None);
    }

    #[test]
    fn the_editor_in_the_callers_tab_wins() {
        let panes = [
            running("w1:p1", "w1:t1", &["hx"]),
            running("w1:p9", "w1:t2", &["hx"]),
        ];
        let got = choose_pane(&panes, helix(), Some("w1:t2"), Some("w1:p5"));
        assert_eq!(got.unwrap().pane_id, "w1:p9");
        // With no tab of our own, herdr's order decides.
        let got = choose_pane(&panes, helix(), None, None);
        assert_eq!(got.unwrap().pane_id, "w1:p1");
    }

    #[test]
    fn only_panes_actually_running_the_editor_are_candidates() {
        let panes = [
            running("w1:p1", "w1:t1", &["lazygit"]),
            running("w1:p2", "w1:t1", &[]),
            running("w1:p3", "w1:t1", &["zsh", "helix"]),
        ];
        let got = choose_pane(&panes, helix(), Some("w1:t1"), None);
        assert_eq!(got.unwrap().pane_id, "w1:p3");
        // Nothing running it at all is the `exec` case, not a wrong pane.
        let none = [running("w1:p1", "w1:t1", &["vim"])];
        assert!(choose_pane(&none, helix(), Some("w1:t1"), None).is_none());
    }

    #[test]
    fn the_calling_pane_is_never_told_to_open_a_file_in_itself() {
        // An editor whose own `:sh selx open …` reached here would other-
        // wise be sent keystrokes while it waits for this process to exit.
        let panes = [running("w1:p1", "w1:t1", &["hx"])];
        assert!(choose_pane(&panes, helix(), Some("w1:t1"), Some("w1:p1")).is_none());
    }

    #[test]
    fn editors_are_recognized_by_binary_name_and_by_path() {
        assert_eq!(editor("hx").unwrap().binary, "hx");
        assert_eq!(editor("helix").unwrap().binary, "hx");
        assert_eq!(editor("/snap/helix/120/bin/hx").unwrap().binary, "hx");
        // Not something selx knows how to type into: `--editor vim` is a
        // legal request, it just always ends in `exec`.
        assert!(editor("vim").is_none());
    }
}
