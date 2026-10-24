//! selx — editor selection bridge for terminal AI agents.
//!
//! One crate, three layers, in dependency order: [`core`] (editor- and
//! agent-agnostic: events, routing, the daemon), [`adapters`] (everything
//! Claude- or herdr-specific, behind the [`core::Adapter`] trait), and the
//! command layer at this level ([`cmd`], [`config`], [`mod@env`], [`out`]).
//! Lower layers never import higher ones; `just test` greps for it.
//!
//! The logic lives here rather than in `main.rs` so the test suites can
//! see it — [`env::VARS`] in particular, which the end-to-end harness
//! iterates to scrub the environment it runs commands in.

// The one `unsafe` in selx is `pre_exec(setsid)` in `core::process`,
// allowed there alone; nothing else needs any.
#![deny(unsafe_code)]

pub mod adapters;
pub mod cmd;
pub mod config;
pub mod core;
pub mod env;
pub mod out;

/// Exit codes shared by all commands.
pub mod exit {
    pub const OK: u8 = 0;
    pub const ERROR: u8 = 1;
    /// `status`/`stop`: no daemon is running for the workspace.
    pub const NOT_RUNNING: u8 = 2;
}
