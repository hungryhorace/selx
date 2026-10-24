//! LogAdapter: appends every event to a JSON-lines file so the pipeline is
//! observable and testable before any agent integration exists.

use crate::core::{Adapter, Delivery, SelectionEvent};
use anyhow::{Context, Result};
use serde::Serialize;
use std::cell::RefCell;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

pub struct LogAdapter {
    path: PathBuf,
    include_text: bool,
    file: RefCell<Option<File>>,
}

#[derive(Serialize)]
struct Line<'a> {
    ts: u64,
    event: &'a SelectionEvent,
}

impl LogAdapter {
    /// `include_text` must only be enabled at verbose log levels: selected
    /// text is never written at default verbosity.
    pub fn new(path: impl Into<PathBuf>, include_text: bool) -> Self {
        LogAdapter {
            path: path.into(),
            include_text,
            file: RefCell::new(None),
        }
    }

    fn open(&self) -> Result<File> {
        OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&self.path)
            .with_context(|| format!("opening {}", self.path.display()))
    }
}

impl Adapter for LogAdapter {
    fn name(&self) -> &str {
        "log"
    }

    fn supports_ambient(&self) -> bool {
        true
    }

    /// A recorder, not an agent: writing to the event log must never look
    /// like an agent was told.
    fn is_agent(&self) -> bool {
        false
    }

    fn deliver(&self, event: &SelectionEvent) -> Result<Delivery> {
        let redacted;
        let ev = if self.include_text {
            event
        } else {
            redacted = event.redacted();
            &redacted
        };
        let mut line = serde_json::to_string(&Line {
            ts: crate::core::unix_now(),
            event: ev,
        })?;
        line.push('\n');

        let mut slot = self.file.borrow_mut();
        if slot.is_none() {
            *slot = Some(self.open()?);
        }
        let f = slot.as_mut().unwrap();
        if let Err(e) = f.write_all(line.as_bytes()) {
            // File may have been rotated/removed underneath us: reopen once.
            *slot = None;
            let mut nf = self.open()?;
            nf.write_all(line.as_bytes()).map_err(|_| e)?;
            *slot = Some(nf);
        }
        Ok(Delivery::delivered())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::EventKind;

    #[test]
    fn writes_redacted_json_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.log");
        let a = LogAdapter::new(&path, false);
        let ev = SelectionEvent::new("/ws", "/ws/a.rs", EventKind::Ambient)
            .with_lines(3, 4)
            .with_text("secret")
            .with_source("t");
        a.deliver(&ev).unwrap();
        a.deliver(&ev).unwrap();
        let s = std::fs::read_to_string(&path).unwrap();
        assert_eq!(s.lines().count(), 2);
        assert!(!s.contains("secret"));
        assert!(s.contains("\"kind\":\"ambient\""));

        let v = LogAdapter::new(&path, true);
        v.deliver(&ev).unwrap();
        let s = std::fs::read_to_string(&path).unwrap();
        assert!(s.lines().last().unwrap().contains("secret"));
    }
}
