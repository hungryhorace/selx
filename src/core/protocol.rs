//! The private ingestion protocol between `selx` CLI commands and the daemon.
//!
//! Newline-delimited JSON over a Unix domain socket. One request per line,
//! one response line per request (except `follow`, which streams frames
//! until the client disconnects). `selx` is the only supported client, so
//! this may change freely between versions.

use crate::core::adapter::{AdapterInfo, RouteOutcome};
use crate::core::event::{Origin, SelectionEvent};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "cmd", rename_all = "lowercase")]
pub enum Request {
    /// Liveness + identity check.
    Ping,
    /// Full daemon status.
    Status,
    /// Route one selection event. Boxed: the event dwarfs every other
    /// variant, and all of them share this enum's size.
    Send { event: Box<SelectionEvent> },
    /// Tell the daemon where a client about to connect is running, so a
    /// delivery to it can name a pane. Sent by `selx claude` before exec.
    RegisterOrigin { origin: Origin },
    /// Stream events as they arrive (after an initial `{"ok":true}` line).
    Follow,
    /// Graceful shutdown.
    Stop,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PingResponse {
    pub ok: bool,
    pub pid: u32,
    pub port: u16,
    pub root: PathBuf,
    pub version: String,
    /// Identity of the binary this daemon was started from
    /// ([`crate::core::build_id`]). Empty from a daemon predating the field.
    #[serde(default)]
    pub build: String,
    /// Connected agent clients. `ensure` needs this in the same breath as
    /// `build` to decide whether an outdated daemon can be replaced, and a
    /// second connection to ask `status` for one number was the only other
    /// way to learn it. `None` from a daemon predating the field, which
    /// callers must read as "will not say", not as zero.
    #[serde(default)]
    pub clients: Option<usize>,
}

/// One routed event, as the daemon reports it: an entry in `status`'s
/// recent list and a line of a `follow` stream are the same thing, and were
/// two identical structs until one of them had to be built from the other
/// just to be formatted.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EventRecord {
    /// Unix seconds.
    pub ts: u64,
    /// Redacted unless the daemon runs with `--verbose`.
    pub event: SelectionEvent,
    pub outcome: RouteOutcome,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StatusResponse {
    pub ok: bool,
    pub pid: u32,
    pub port: u16,
    pub root: PathBuf,
    pub version: String,
    #[serde(default)]
    pub build: String,
    pub uptime_secs: u64,
    pub idle_secs: u64,
    pub idle_timeout_secs: u64,
    pub events_received: u64,
    pub clients: usize,
    pub adapters: Vec<AdapterInfo>,
    pub recent: Vec<EventRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SendResponse {
    pub ok: bool,
    pub outcome: RouteOutcome,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OkResponse {
    pub ok: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ErrorResponse {
    pub ok: bool,
    pub error: String,
}

impl ErrorResponse {
    pub fn new(msg: impl Into<String>) -> Self {
        ErrorResponse {
            ok: false,
            error: msg.into(),
        }
    }
}

/// One line of a `follow` stream. The same shape on the wire as a `recent`
/// entry, because it is the same thing.
pub type FollowFrame = EventRecord;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_wire_format() {
        assert_eq!(
            serde_json::to_string(&Request::Ping).unwrap(),
            r#"{"cmd":"ping"}"#
        );
        let r: Request = serde_json::from_str(r#"{"cmd":"status"}"#).unwrap();
        assert_eq!(r, Request::Status);
        let r: Request = serde_json::from_str(
            r#"{"cmd":"send","event":{"workspace_root":"/w","file":"/w/f","lines":{"start":1,"end":1},"kind":"ambient","source":"t"}}"#,
        )
        .unwrap();
        assert!(matches!(r, Request::Send { .. }));
        let r: Request = serde_json::from_str(
            r#"{"cmd":"registerorigin","origin":{"mux":"herdr","pane":"w1:p2"}}"#,
        )
        .unwrap();
        let Request::RegisterOrigin { origin } = r else {
            panic!("not a registration")
        };
        assert_eq!(origin.pane.as_deref(), Some("w1:p2"));
        assert_eq!(origin.workspace, None);
    }
}
