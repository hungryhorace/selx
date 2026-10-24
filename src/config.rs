//! Optional config file: `~/.config/selx/config.toml`.
//!
//! selx runs fine without one. `SELX_CONFIG` overrides the path; setting it
//! to an empty string disables the config file entirely (tests do this so a
//! developer's real config never leaks in).
//!
//! ```toml
//! idle_timeout_secs = 14400      # daemon idle shutdown; 0 disables
//!
//! [inject]                       # herdr text injection defaults
//! target = "codex"               # agent name, pane id ("w1:p2") or kind
//! exclude = ["claude"]           # agent kinds never targeted
//!
//! [focus]                        # follow a delivery to the pane that got it
//! mode = "delivered"             # "delivered" (default) | "explicit" | "off"
//!
//! [reload]                       # Claude's edits reload the editor
//! mode = "on"                    # "on" (default) | "off"
//!
//! [[workspace]]                  # per-workspace overrides; first match wins
//! root = "~/Projects/foo"
//! idle_timeout_secs = 0
//! inject = { target = "gemini" }
//! focus = { mode = "off" }
//! reload = { mode = "off" }
//! ```
//!
//! Precedence, most specific wins: CLI flag > environment variable
//! (`SELX_IDLE_TIMEOUT`, `SELX_INJECT_TARGET`, `SELX_INJECT_EXCLUDE`,
//! `SELX_FOCUS`, `SELX_RELOAD`) > `[[workspace]]` section > top level >
//! built-in default.

use crate::core::{FocusMode, ParseError};
use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Daemon idle shutdown in seconds; 0 disables it.
    pub idle_timeout_secs: Option<u64>,
    #[serde(default)]
    pub inject: Inject,
    #[serde(default)]
    pub focus: Focus,
    #[serde(default)]
    pub reload: Reload,
    /// Per-workspace overrides. The first section whose `root` matches the
    /// daemon's workspace root applies.
    #[serde(default)]
    pub workspace: Vec<Workspace>,
}

#[derive(Debug, Deserialize, Default, Clone)]
#[serde(deny_unknown_fields)]
pub struct Inject {
    /// Preferred injection target: live agent name, pane id or agent kind.
    pub target: Option<String>,
    /// Agent kinds never targeted (default `["claude"]` — Claude has its
    /// own adapter).
    pub exclude: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, Default, Clone)]
#[serde(deny_unknown_fields)]
pub struct Focus {
    /// When a delivery moves focus to the pane that received it.
    pub mode: Option<FocusMode>,
}

#[derive(Debug, Deserialize, Default, Clone)]
#[serde(deny_unknown_fields)]
pub struct Reload {
    /// Whether Claude's edits reload the editor (`selx reload`'s hook).
    pub mode: Option<ReloadMode>,
}

/// Whether `selx claude` installs the hooks that reload the editor after
/// Claude edits a file, and whether those hooks act. Both, so switching it
/// off in the config file also quiets a Claude that is already running.
/// A `selx reload` typed by hand is not affected: asking is the switch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReloadMode {
    #[default]
    On,
    Off,
}

impl std::str::FromStr for ReloadMode {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<ReloadMode, Self::Err> {
        match s {
            "on" => Ok(ReloadMode::On),
            "off" => Ok(ReloadMode::Off),
            _ => Err(ParseError::new("reload mode", s, &["on", "off"])),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Workspace {
    /// Workspace root this section applies to. A leading `~` expands to the
    /// home directory.
    pub root: PathBuf,
    pub idle_timeout_secs: Option<u64>,
    #[serde(default)]
    pub inject: Inject,
    #[serde(default)]
    pub focus: Focus,
    #[serde(default)]
    pub reload: Reload,
}

/// The settings that apply to one workspace root, after merging the
/// top-level and `[[workspace]]` sections. `None` means "not configured".
#[derive(Debug, Default, PartialEq)]
pub struct Resolved {
    pub idle_timeout_secs: Option<u64>,
    pub inject_target: Option<String>,
    pub inject_exclude: Option<Vec<String>>,
    pub focus_mode: Option<FocusMode>,
    pub reload_mode: Option<ReloadMode>,
}

impl Resolved {
    /// Layer the `SELX_*` overrides on top of what the config file said.
    ///
    /// This is the whole environment leg of the precedence chain, in one
    /// place, so every setting errors the same way on a value selx cannot
    /// parse — silently falling through to the config file is how
    /// `SELX_IDLE_TIMEOUT=5m` used to become the default without saying so.
    ///
    /// `env` is a lookup rather than [`std::env::var`] so the table below
    /// is unit-testable: process environment is global, and mutating it
    /// would be visible to every other test in this binary.
    ///
    /// Empty means "unset" everywhere except `SELX_INJECT_EXCLUDE`, where
    /// it means an empty exclude list — the only way to say "inject into
    /// Claude too", which is otherwise excluded by default.
    pub fn apply_env(self, env: &impl Fn(&str) -> Option<String>) -> Result<Resolved> {
        let set = |name: &str| env(name).filter(|s| !s.trim().is_empty());

        let idle_timeout_secs = match set("SELX_IDLE_TIMEOUT") {
            Some(s) => Some(s.trim().parse::<u64>().with_context(|| {
                format!("SELX_IDLE_TIMEOUT must be a whole number of seconds (got {s:?})")
            })?),
            None => self.idle_timeout_secs,
        };
        let focus_mode = match set("SELX_FOCUS") {
            // The accepted values come from `FocusMode` itself, so this
            // cannot list them wrongly.
            Some(s) => Some(s.trim().parse::<FocusMode>().context("SELX_FOCUS")?),
            None => self.focus_mode,
        };
        let reload_mode = match set("SELX_RELOAD") {
            Some(s) => Some(s.trim().parse::<ReloadMode>().context("SELX_RELOAD")?),
            None => self.reload_mode,
        };
        let inject_target = set("SELX_INJECT_TARGET")
            .map(|s| s.trim().to_string())
            .or(self.inject_target);
        let inject_exclude = match env("SELX_INJECT_EXCLUDE") {
            Some(list) => Some(
                list.split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect(),
            ),
            None => self.inject_exclude,
        };
        Ok(Resolved {
            idle_timeout_secs,
            inject_target,
            inject_exclude,
            focus_mode,
            reload_mode,
        })
    }

    /// [`apply_env`](Self::apply_env) against the real process environment.
    pub fn apply_process_env(self) -> Result<Resolved> {
        self.apply_env(&|name| std::env::var(name).ok())
    }
}

impl Config {
    /// Load the config file, or defaults when there is none.
    pub fn load() -> Result<Config> {
        match config_path() {
            Some(p) if p.exists() => Config::parse_file(&p),
            _ => Ok(Config::default()),
        }
    }

    fn parse_file(path: &Path) -> Result<Config> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config file {}", path.display()))?;
        basic_toml::from_str(&text).with_context(|| {
            format!(
                "config file {} is invalid; fix or remove it (selx runs fine without one)",
                path.display()
            )
        })
    }

    /// Merge the top-level settings with the first matching `[[workspace]]`
    /// section for `root` (which is already canonicalized by root resolution).
    pub fn for_root(&self, root: &Path) -> Resolved {
        let ws = self.workspace.iter().find(|w| {
            let expanded = expand_tilde(&w.root);
            std::fs::canonicalize(&expanded).unwrap_or(expanded) == root
        });
        let (ws_idle, ws_inject, ws_focus, ws_reload) = match ws {
            Some(w) => (
                w.idle_timeout_secs,
                w.inject.clone(),
                w.focus.clone(),
                w.reload.clone(),
            ),
            None => (None, Inject::default(), Focus::default(), Reload::default()),
        };
        Resolved {
            idle_timeout_secs: ws_idle.or(self.idle_timeout_secs),
            inject_target: ws_inject.target.or_else(|| self.inject.target.clone()),
            inject_exclude: ws_inject.exclude.or_else(|| self.inject.exclude.clone()),
            focus_mode: ws_focus.mode.or(self.focus.mode),
            reload_mode: ws_reload.mode.or(self.reload.mode),
        }
    }
}

/// `$SELX_CONFIG` (empty disables) else `$XDG_CONFIG_HOME|~/.config` +
/// `selx/config.toml`. The plain `~/.config` fallback is deliberate on macOS
/// too — terminal tools live there, not in `~/Library/Application Support`.
pub fn config_path() -> Option<PathBuf> {
    config_path_from(
        std::env::var_os("SELX_CONFIG"),
        std::env::var_os("XDG_CONFIG_HOME"),
    )
}

/// The precedence behind [`config_path`], separated from the environment so
/// it is testable without mutating this process's own (which every other
/// test in the binary would see).
fn config_path_from(
    selx_config: Option<std::ffi::OsString>,
    xdg_config_home: Option<std::ffi::OsString>,
) -> Option<PathBuf> {
    if let Some(p) = selx_config {
        // Set but empty means "no config file at all", not "use the
        // default": the tests rely on it to keep a developer's real config
        // out of a run.
        if p.is_empty() {
            return None;
        }
        return Some(PathBuf::from(p));
    }
    let base = xdg_config_home
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::core::paths::home_dir().join(".config"));
    Some(base.join("selx").join("config.toml"))
}

fn expand_tilde(p: &Path) -> PathBuf {
    match p.strip_prefix("~") {
        Ok(rest) => crate::core::paths::home_dir().join(rest),
        Err(_) => p.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn some(s: &str) -> Option<std::ffi::OsString> {
        Some(std::ffi::OsString::from(s))
    }

    /// A lookup standing in for the process environment.
    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let map: std::collections::HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |name: &str| map.get(name).cloned()
    }

    /// The environment leg of the precedence chain, for every setting. It
    /// had one end-to-end test per setting and none at all for the two
    /// injection ones; here it is a table.
    #[test]
    fn the_environment_beats_the_config_file_setting_by_setting() {
        let from_file = || Resolved {
            idle_timeout_secs: Some(3333),
            inject_target: Some("from-file".into()),
            inject_exclude: Some(vec!["claude".into(), "from-file".into()]),
            focus_mode: Some(FocusMode::Explicit),
            reload_mode: Some(ReloadMode::On),
        };

        // Nothing set: the file's answer, untouched.
        let r = from_file().apply_env(&env(&[])).unwrap();
        assert_eq!(r, from_file());

        // Each variable overrides its own setting and nothing else.
        let r = from_file()
            .apply_env(&env(&[
                ("SELX_IDLE_TIMEOUT", " 99 "),
                ("SELX_FOCUS", " off "),
                ("SELX_RELOAD", " off "),
                ("SELX_INJECT_TARGET", " from-env "),
                ("SELX_INJECT_EXCLUDE", "claude, gemini ,"),
            ]))
            .unwrap();
        assert_eq!(r.idle_timeout_secs, Some(99));
        assert_eq!(r.focus_mode, Some(FocusMode::Off));
        assert_eq!(r.reload_mode, Some(ReloadMode::Off));
        assert_eq!(r.inject_target.as_deref(), Some("from-env"));
        assert_eq!(
            r.inject_exclude,
            Some(vec!["claude".into(), "gemini".into()])
        );

        // An unrelated variable changes nothing.
        let r = from_file()
            .apply_env(&env(&[("SELX_STATE_DIR", "/tmp")]))
            .unwrap();
        assert_eq!(r, from_file());
    }

    #[test]
    fn an_empty_value_means_unset_except_for_the_exclude_list() {
        let from_file = || Resolved {
            idle_timeout_secs: Some(3333),
            inject_target: Some("from-file".into()),
            inject_exclude: Some(vec!["claude".into()]),
            focus_mode: Some(FocusMode::Explicit),
            reload_mode: Some(ReloadMode::Off),
        };
        let r = from_file()
            .apply_env(&env(&[
                ("SELX_IDLE_TIMEOUT", ""),
                ("SELX_FOCUS", "   "),
                ("SELX_RELOAD", ""),
                ("SELX_INJECT_TARGET", ""),
            ]))
            .unwrap();
        assert_eq!(r.idle_timeout_secs, Some(3333));
        assert_eq!(r.focus_mode, Some(FocusMode::Explicit));
        assert_eq!(r.reload_mode, Some(ReloadMode::Off));
        assert_eq!(r.inject_target.as_deref(), Some("from-file"));

        // `SELX_INJECT_EXCLUDE=` is the only way to say "exclude nothing",
        // which is the only way to have text typed into Claude as well.
        let r = from_file()
            .apply_env(&env(&[("SELX_INJECT_EXCLUDE", "")]))
            .unwrap();
        assert_eq!(r.inject_exclude, Some(vec![]));
    }

    #[test]
    fn a_value_selx_cannot_parse_is_an_error_naming_what_it_accepts() {
        // The failure this replaced: `5m` silently became the default.
        let e = Resolved::default()
            .apply_env(&env(&[("SELX_IDLE_TIMEOUT", "5m")]))
            .unwrap_err();
        let msg = format!("{e:#}");
        assert!(msg.contains("SELX_IDLE_TIMEOUT"), "{msg}");
        assert!(msg.contains("whole number of seconds"), "{msg}");

        let e = Resolved::default()
            .apply_env(&env(&[("SELX_FOCUS", "sometimes")]))
            .unwrap_err();
        let msg = format!("{e:#}");
        assert!(msg.contains("SELX_FOCUS"), "{msg}");
        assert!(msg.contains("delivered, explicit or off"), "{msg}");
        assert!(msg.contains("sometimes"), "{msg}");

        let e = Resolved::default()
            .apply_env(&env(&[("SELX_RELOAD", "0")]))
            .unwrap_err();
        let msg = format!("{e:#}");
        assert!(msg.contains("SELX_RELOAD"), "{msg}");
        assert!(msg.contains("on or off"), "{msg}");
    }

    #[test]
    fn the_config_path_prefers_selx_config_and_an_empty_one_disables_it() {
        // An explicit path wins over everything, including a set XDG dir.
        assert_eq!(
            config_path_from(some("/tmp/mine.toml"), some("/xdg")),
            Some(PathBuf::from("/tmp/mine.toml"))
        );
        // Set but empty: no config file at all. This is what the end-to-end
        // suite sets, so a developer's own config cannot change a test.
        assert_eq!(config_path_from(some(""), some("/xdg")), None);
        // Unset: XDG decides.
        assert_eq!(
            config_path_from(None, some("/xdg")),
            Some(PathBuf::from("/xdg/selx/config.toml"))
        );
        // An empty XDG variable is "unset", and falls through to ~/.config
        // — not to `/selx/config.toml`.
        for xdg in [None, some("")] {
            let p = config_path_from(None, xdg).expect("a home dir");
            assert!(p.ends_with(".config/selx/config.toml"), "{}", p.display());
            assert!(p.is_absolute(), "{}", p.display());
        }
    }

    fn parse(s: &str) -> Config {
        basic_toml::from_str(s).unwrap()
    }

    #[test]
    fn empty_and_full_configs_parse() {
        let c = parse("");
        assert_eq!(c.for_root(Path::new("/x")), Resolved::default());

        let c = parse(
            r#"
            idle_timeout_secs = 3333
            [inject]
            target = "codex"
            exclude = ["claude", "gemini"]
            [focus]
            mode = "explicit"
            [reload]
            mode = "on"
            [[workspace]]
            root = "/work/a"
            idle_timeout_secs = 0
            inject = { target = "gemini" }
            focus = { mode = "off" }
            reload = { mode = "off" }
            [[workspace]]
            root = "/work/b"
            "#,
        );
        // Unrelated root: top level only.
        let r = c.for_root(Path::new("/elsewhere"));
        assert_eq!(r.idle_timeout_secs, Some(3333));
        assert_eq!(r.inject_target.as_deref(), Some("codex"));
        assert_eq!(
            r.inject_exclude,
            Some(vec!["claude".into(), "gemini".into()])
        );
        assert_eq!(r.focus_mode, Some(FocusMode::Explicit));
        assert_eq!(r.reload_mode, Some(ReloadMode::On));
        // Matching workspace overrides per field, inherits the rest.
        let r = c.for_root(Path::new("/work/a"));
        assert_eq!(r.idle_timeout_secs, Some(0));
        assert_eq!(r.inject_target.as_deref(), Some("gemini"));
        assert_eq!(r.focus_mode, Some(FocusMode::Off));
        assert_eq!(r.reload_mode, Some(ReloadMode::Off));
        assert_eq!(
            r.inject_exclude,
            Some(vec!["claude".into(), "gemini".into()])
        );
        // Workspace section with nothing set inherits everything.
        let r = c.for_root(Path::new("/work/b"));
        assert_eq!(r.idle_timeout_secs, Some(3333));
        assert_eq!(r.inject_target.as_deref(), Some("codex"));
        assert_eq!(r.focus_mode, Some(FocusMode::Explicit));
    }

    #[test]
    fn tilde_expands_to_home() {
        let home = directories::BaseDirs::new().unwrap().home_dir().to_owned();
        assert_eq!(expand_tilde(Path::new("~/x/y")), home.join("x/y"));
        assert_eq!(expand_tilde(Path::new("/abs/x")), Path::new("/abs/x"));
    }

    #[test]
    fn typos_are_rejected_with_field_name() {
        let e = basic_toml::from_str::<Config>("idle_timeout = 5").unwrap_err();
        assert!(e.to_string().contains("idle_timeout"), "{e}");
        let e = basic_toml::from_str::<Config>("[inject]\ntargets = [\"x\"]").unwrap_err();
        assert!(e.to_string().contains("targets"), "{e}");
        let e = basic_toml::from_str::<Config>("[focus]\nmode = \"sometimes\"").unwrap_err();
        assert!(e.to_string().contains("sometimes"), "{e}");
        let e = basic_toml::from_str::<Config>("[reload]\nmode = \"auto\"").unwrap_err();
        assert!(e.to_string().contains("auto"), "{e}");
    }
}
