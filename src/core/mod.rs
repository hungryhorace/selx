//! The editor-agnostic, agent-agnostic centre of selx.
//!
//! Hard rule: nothing in this module may know about any specific editor
//! (Helix, lazygit) or any specific agent (Claude Code, herdr, ...).
//! Editors talk to the daemon through the private ingestion protocol
//! ([`protocol`]); agents are reached through [`adapter::Adapter`]
//! implementations that live in the `adapters` module beside this one.
//! The compiler no longer enforces the boundary (this was its own crate
//! once); `just layering`, part of `just test`, greps `src/core/` for any
//! path into the adapters or the command layer instead.

pub mod adapter;
pub mod build_id;
pub mod client;
pub mod daemon;
pub mod ensure;
pub mod event;
pub mod paths;
pub mod process;
pub mod protocol;
pub mod state;
pub mod workspace;

pub use adapter::{
    should_follow, Adapter, AdapterInfo, Delivery, FocusMode, Focuser, RouteOutcome, Router,
};
pub use event::{EventKind, LineRange, Origin, ParseError, SelectionEvent};
pub use state::StateDir;

/// Seconds since the Unix epoch, for timestamps in logs and status output.
pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
