//! Every environment variable selx reads, in one list.
//!
//! Twenty variables are read from seven files across three layers. The
//! list has to exist somewhere; the point of putting it here is that the
//! two things that must agree with it — the README's table and the
//! end-to-end harness's scrub list — both derive from it instead of
//! restating it. Both had already drifted: `SELX_FOCUS` and `SELX_LOG`
//! were read by the daemon and scrubbed by neither.
//!
//! This is a registry, not the reader. Each variable is still read where it
//! is used; `Var::what` is just enough to write a table from.

/// One environment variable selx reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Var {
    pub name: &'static str,
    /// What it controls, phrased to sit in the README's list.
    pub what: &'static str,
    /// Where it is read: the layer (`core`, `adapters`, or `cmd` for the
    /// command layer, `config.rs` included).
    pub layer: &'static str,
}

/// Every variable selx reads, wherever it reads it.
pub const VARS: &[Var] = &[
    Var {
        name: "SELX_CONFIG",
        what: "config file path; empty disables the config file",
        layer: "cmd",
    },
    Var {
        name: "XDG_CONFIG_HOME",
        what: "base for the default config file location",
        layer: "cmd",
    },
    Var {
        name: "SELX_IDLE_TIMEOUT",
        what: "daemon idle shutdown, in whole seconds",
        layer: "cmd",
    },
    Var {
        name: "SELX_FOCUS",
        what: "when a delivery moves focus: delivered, explicit or off",
        layer: "cmd",
    },
    Var {
        name: "SELX_RELOAD",
        what: "whether Claude's edits reload the editor: on or off",
        layer: "cmd",
    },
    Var {
        name: "SELX_INJECT_TARGET",
        what: "pin text injection to an agent name, pane id or kind",
        layer: "cmd",
    },
    Var {
        name: "SELX_INJECT_EXCLUDE",
        what: "agent kinds never injected into, comma-separated",
        layer: "cmd",
    },
    Var {
        name: "SELX_IDE_NAME",
        what: "ideName written to the Claude lock file",
        layer: "cmd",
    },
    Var {
        name: "SELX_CLAUDE_BIN",
        what: "path to the claude binary",
        layer: "cmd",
    },
    Var {
        name: "SELX_CLAUDE_UPDATE",
        what: "0 skips the claude update selx claude runs before starting Claude",
        layer: "cmd",
    },
    Var {
        name: "SELX_LOG",
        what: "tracing filter for -v diagnostics",
        layer: "cmd",
    },
    Var {
        name: "SELX_STATE_DIR",
        what: "base directory for per-workspace runtime state",
        layer: "core",
    },
    Var {
        name: "CLAUDE_CONFIG_DIR",
        what: "where the Claude IDE lock file lives",
        layer: "adapters",
    },
    Var {
        name: "HERDR_SOCKET_PATH",
        what: "herdr's API socket",
        layer: "adapters",
    },
    Var {
        name: "HERDR_CONFIG_PATH",
        what: "herdr's config file, whose directory holds session sockets",
        layer: "adapters",
    },
    Var {
        name: "HERDR_SESSION",
        what: "herdr session, named on the event origin",
        layer: "adapters",
    },
    Var {
        name: "HERDR_WORKSPACE_ID",
        what: "the sending pane's herdr workspace",
        layer: "adapters",
    },
    Var {
        name: "HERDR_TAB_ID",
        what: "the sending pane's herdr tab",
        layer: "adapters",
    },
    Var {
        name: "HERDR_PANE_ID",
        what: "the sending pane",
        layer: "adapters",
    },
];

/// Just the names, for anyone who only needs to clear them.
pub fn names() -> impl Iterator<Item = &'static str> {
    VARS.iter().map(|v| v.name)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The README is where a user looks these up, so every variable selx
    /// reads must appear in it. Compile-time `include_str!`, so adding a
    /// variable and forgetting the docs fails here rather than silently.
    #[test]
    fn every_variable_is_documented_in_the_readme() {
        const README: &str = include_str!("../README.md");
        let missing: Vec<&str> = names().filter(|n| !README.contains(*n)).collect();
        assert!(missing.is_empty(), "undocumented in README.md: {missing:?}");
    }

    #[test]
    fn the_registry_is_a_set_of_real_names() {
        let mut seen = std::collections::HashSet::new();
        for v in VARS {
            assert!(seen.insert(v.name), "listed twice: {}", v.name);
            assert!(!v.what.is_empty(), "{} has no description", v.name);
            assert!(
                v.name
                    .chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'),
                "not an environment variable name: {}",
                v.name
            );
        }
    }
}
