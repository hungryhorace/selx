//! Synchronous client for the ingestion socket. CLI commands are short-lived
//! and need no async runtime.

use crate::core::event::{Origin, SelectionEvent};
use crate::core::protocol::*;
use crate::core::state::StateDir;
use anyhow::{anyhow, Context, Result};
use serde::de::DeserializeOwned;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

pub struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl Client {
    pub fn connect(socket: &Path, timeout: Duration) -> Result<Client> {
        let stream = UnixStream::connect(socket)
            .with_context(|| format!("connecting to {}", socket.display()))?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        let writer = stream.try_clone()?;
        Ok(Client {
            reader: BufReader::new(stream),
            writer,
        })
    }

    pub fn connect_state(state: &StateDir, timeout: Duration) -> Result<Client> {
        Self::connect(&state.socket_path(), timeout)
    }

    fn write(&mut self, req: &Request) -> Result<()> {
        let mut line = serde_json::to_string(req)?;
        line.push('\n');
        self.writer.write_all(line.as_bytes())?;
        self.writer.flush()?;
        Ok(())
    }

    fn read_line(&mut self) -> Result<String> {
        let mut line = String::new();
        let n = self.reader.read_line(&mut line)?;
        if n == 0 {
            return Err(anyhow!("daemon closed the connection"));
        }
        Ok(line)
    }

    fn call<T: DeserializeOwned>(&mut self, req: &Request) -> Result<T> {
        self.write(req)?;
        let line = self.read_line()?;
        parse_response(&line)
    }

    pub fn ping(&mut self) -> Result<PingResponse> {
        self.call(&Request::Ping)
    }

    pub fn status(&mut self) -> Result<StatusResponse> {
        self.call(&Request::Status)
    }

    pub fn send(&mut self, event: SelectionEvent) -> Result<SendResponse> {
        self.call(&Request::Send {
            event: Box::new(event),
        })
    }

    /// Tell the daemon where this process is running. Best effort by
    /// nature: the caller is about to exec something else.
    pub fn register_origin(&mut self, origin: Origin) -> Result<()> {
        let _: OkResponse = self.call(&Request::RegisterOrigin { origin })?;
        Ok(())
    }

    pub fn stop(&mut self) -> Result<()> {
        let _: OkResponse = self.call(&Request::Stop)?;
        Ok(())
    }

    /// Stream frames to `f` until it returns `false`, the daemon goes away,
    /// or an error occurs.
    pub fn follow(mut self, mut f: impl FnMut(FollowFrame) -> bool) -> Result<()> {
        let _: OkResponse = self.call(&Request::Follow)?;
        // No read timeout while following.
        self.reader.get_ref().set_read_timeout(None)?;
        loop {
            let mut line = String::new();
            let n = self.reader.read_line(&mut line)?;
            if n == 0 {
                return Ok(());
            }
            let frame: FollowFrame = serde_json::from_str(&line).context("bad follow frame")?;
            if !f(frame) {
                return Ok(());
            }
        }
    }
}

fn parse_response<T: DeserializeOwned>(line: &str) -> Result<T> {
    let v: serde_json::Value = serde_json::from_str(line).context("unparseable response")?;
    if v.get("ok").and_then(|o| o.as_bool()) == Some(false) {
        let msg = v
            .get("error")
            .and_then(|e| e.as_str())
            .unwrap_or("unknown error");
        return Err(anyhow!("daemon error: {msg}"));
    }
    serde_json::from_value(v).context("unexpected response shape")
}

/// How long a health check waits. Short: the daemon is a local process on
/// a unix socket, and every command starts by asking it.
const PROBE_TIMEOUT: Duration = Duration::from_millis(1500);

/// Health check: the daemon answers a ping *and* serves this root.
pub fn try_ping(state: &StateDir) -> Option<PingResponse> {
    probe(state, PROBE_TIMEOUT, |c| c.ping(), |p| &p.root)
}

/// The same check, asking for the full status. `selx status` uses this: it
/// needs the answer itself, not just the fact that one came.
pub fn try_status(state: &StateDir, timeout: Duration) -> Option<StatusResponse> {
    probe(state, timeout, |c| c.status(), |s| &s.root)
}

/// Connect, ask, and reject an answer from a daemon serving another root.
///
/// The root check is the point: a state directory is named by a hash of
/// its root, so a daemon answering on this socket for a *different* root
/// is state left over from a path that has been reused, and treating it as
/// healthy would hand the user someone else's workspace.
fn probe<T>(
    state: &StateDir,
    timeout: Duration,
    ask: impl FnOnce(&mut Client) -> Result<T>,
    root_of: impl FnOnce(&T) -> &Path,
) -> Option<T> {
    let mut c = Client::connect_state(state, timeout).ok()?;
    let answer = ask(&mut c).ok()?;
    if root_of(&answer) != state.root {
        return None;
    }
    Some(answer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_daemon_error_becomes_the_message_it_sent() {
        let e = parse_response::<OkResponse>(r#"{"ok":false,"error":"no such root"}"#).unwrap_err();
        assert_eq!(e.to_string(), "daemon error: no such root");
    }

    #[test]
    fn a_refusal_with_no_reason_still_fails() {
        let e = parse_response::<OkResponse>(r#"{"ok":false}"#).unwrap_err();
        assert_eq!(e.to_string(), "daemon error: unknown error");
    }

    #[test]
    fn junk_on_the_wire_is_distinguished_from_the_wrong_shape() {
        // Not JSON at all: the daemon is not speaking this protocol.
        let e = parse_response::<OkResponse>("not json").unwrap_err();
        assert_eq!(e.to_string(), "unparseable response");

        // JSON, `ok` is not false, but not the response we asked for —
        // a daemon of the wrong vintage, not a daemon that said no.
        let e = parse_response::<PingResponse>(r#"{"ok":true}"#).unwrap_err();
        assert_eq!(e.to_string(), "unexpected response shape");
    }

    #[test]
    fn a_well_formed_response_parses() {
        let p: PingResponse = parse_response(
            r#"{"ok":true,"pid":7,"port":10001,"root":"/ws","version":"0.1.0","build":"x"}"#,
        )
        .unwrap();
        assert_eq!((p.pid, p.port), (7, 10001));
        // A daemon predating the `build` field omits it, and must still
        // be readable rather than looking like the wrong shape.
        let p: PingResponse =
            parse_response(r#"{"ok":true,"pid":7,"port":10001,"root":"/ws","version":"0.1.0"}"#)
                .unwrap();
        assert_eq!(p.build, "");
    }
}
