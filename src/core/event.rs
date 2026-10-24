//! The stable centre of the design: a selection made in some editor.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// How strongly the user means the selection.
///
/// Adapters that cannot express a weak signal must *drop* `Ambient` events,
/// never upgrade them to `Explicit`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EventKind {
    /// "This is what I happen to have selected" — may be coincidental.
    Ambient,
    /// "I am deliberately pointing the agent at these lines."
    Explicit,
}

impl EventKind {
    /// The one spelling table for this enum. `Display`, `FromStr` and
    /// serde's `rename_all = "lowercase"` must all agree with it — which
    /// `spellings_match_the_wire_format` is what checks.
    pub fn as_str(self) -> &'static str {
        match self {
            EventKind::Ambient => "ambient",
            EventKind::Explicit => "explicit",
        }
    }

    /// Every value, for error messages and exhaustive tests.
    pub const VALUES: [EventKind; 2] = [EventKind::Ambient, EventKind::Explicit];
}

impl std::fmt::Display for EventKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for EventKind {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<EventKind, ParseError> {
        match s {
            "ambient" => Ok(EventKind::Ambient),
            "explicit" => Ok(EventKind::Explicit),
            _ => Err(ParseError::new("event kind", s, &["ambient", "explicit"])),
        }
    }
}

/// A string that is not one of an enum's values.
///
/// Carries the accepted values, so a caller does not have to restate them
/// and cannot restate them wrongly — the reason this exists rather than a
/// bare `()` error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    what: &'static str,
    got: String,
    valid: &'static [&'static str],
}

impl ParseError {
    pub fn new(what: &'static str, got: &str, valid: &'static [&'static str]) -> ParseError {
        ParseError {
            what,
            got: got.to_string(),
            valid,
        }
    }
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let list = match self.valid {
            [] => String::new(),
            [only] => (*only).to_string(),
            [rest @ .., last] => format!("{} or {last}", rest.join(", ")),
        };
        write!(
            f,
            "{} must be {list} (got {:?})",
            self.what,
            self.got.trim()
        )
    }
}

impl std::error::Error for ParseError {}

/// Where an event was produced, in terminal-multiplexer coordinates.
///
/// Every field is an opaque string as far as core is concerned: it carries
/// them, compares them for equality with nothing, and never interprets
/// them. Only an adapter whose own multiplexer matches `mux` may read them
/// (see `adapters::herdr`). The vocabulary is deliberately the common
/// shape of herdr, tmux and zellij, so a second multiplexer is a new value
/// of `mux` rather than a new design.
///
/// Only `selx send` can fill this in: it runs as a child of the editor and
/// so knows where the keypress happened, while the daemon is a long-lived
/// per-root process whose own environment points at an arbitrary pane or
/// none at all.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Origin {
    /// Multiplexer that owns the coordinates below, e.g. "herdr".
    pub mux: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tab: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane: Option<String>,
}

/// A 1-based, inclusive line range within a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineRange {
    /// 1-based.
    pub start: u32,
    /// 1-based, inclusive.
    pub end: u32,
}

/// A selection event as accepted by the ingestion API and fanned out to adapters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectionEvent {
    /// Absolute, canonicalized workspace root (worktree root or plain folder).
    pub workspace_root: PathBuf,
    /// Absolute path of the selected file. `None` for a message, which
    /// is about whatever its sentence says and not about a file: a map
    /// in an editor, say, wherever the map is kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<PathBuf>,
    /// Selected lines. `None` means the file as a whole — the frontend is
    /// pointing at the file without making any line-level claim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lines: Option<LineRange>,
    /// Selected text, if the frontend could supply it cheaply.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    pub kind: EventKind,
    /// Informational: "helix", "lazygit", ...
    pub source: String,
    /// Where the send came from, when the sender could say. `None` — a
    /// plain terminal, an editor outside any multiplexer, a test — is a
    /// supported case, not a degraded one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<Origin>,
    /// A sentence to put in the agent's prompt instead of a reference to
    /// the file and lines, for a sender whose selection is not lines — a
    /// rectangle of a map, say. Always explicit: it goes by typing into a
    /// pane, which cannot express a weak signal. The daemon keeps it to
    /// one line of at most [`MAX_MESSAGE_CHARS`] with no control
    /// characters ([`sanitize_message`]), so a message can never press
    /// Enter or drive the terminal; that is the whole hazard of free text,
    /// and it lives here rather than in each sender.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// The most a message may be, in characters. Room for a sentence and a
/// reference, not a paragraph: the agent reads the file for the rest.
pub const MAX_MESSAGE_CHARS: usize = 400;

/// A message as it may be typed: every control character (a newline, a
/// tab, an escape) becomes a space, and the result is cut to
/// [`MAX_MESSAGE_CHARS`]. A trailing space is kept: a sender ends its
/// sentence with one so the user's own words start off it.
pub fn sanitize_message(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(MAX_MESSAGE_CHARS)
        .collect()
}

impl SelectionEvent {
    /// The whole-file event for `file` under `root`: no lines, no text, no
    /// origin, source `"cli"`. Every optional field has a `with_*` to add
    /// it back.
    ///
    /// Not `#[cfg(test)]`, though tests are what mostly build events by
    /// hand: `send.rs` assembles one this way too, and the adapters'
    /// integration tests are separate crates that could not see a
    /// test-only constructor. Written out longhand, a new optional field
    /// means editing every construction site — which is how `origin`
    /// landed.
    pub fn new(
        root: impl Into<PathBuf>,
        file: impl Into<PathBuf>,
        kind: EventKind,
    ) -> SelectionEvent {
        SelectionEvent {
            workspace_root: root.into(),
            file: Some(file.into()),
            lines: None,
            text: None,
            kind,
            source: "cli".into(),
            origin: None,
            message: None,
        }
    }

    /// A message and nothing else: no file, no lines, explicit, sanitized
    /// as the daemon would (see [`sanitize_message`]). What `selx say`
    /// sends.
    pub fn message(root: impl Into<PathBuf>, message: &str) -> SelectionEvent {
        SelectionEvent {
            workspace_root: root.into(),
            file: None,
            lines: None,
            text: None,
            kind: EventKind::Explicit,
            source: "cli".into(),
            origin: None,
            message: Some(sanitize_message(message)),
        }
    }

    /// A 1-based inclusive line range. `start == end` is a single line.
    pub fn with_lines(self, start: u32, end: u32) -> SelectionEvent {
        SelectionEvent {
            lines: Some(LineRange { start, end }),
            ..self
        }
    }

    /// The same, for a range already in hand (what `send` parses into).
    pub fn with_line_range(self, lines: Option<LineRange>) -> SelectionEvent {
        SelectionEvent { lines, ..self }
    }

    pub fn with_text(self, text: impl Into<String>) -> SelectionEvent {
        SelectionEvent {
            text: Some(text.into()),
            ..self
        }
    }

    /// Where the send came from. See [`Origin`]: only `selx send` knows.
    pub fn with_origin(self, origin: Origin) -> SelectionEvent {
        SelectionEvent {
            origin: Some(origin),
            ..self
        }
    }

    /// The same, for an origin that may be absent (outside a multiplexer).
    pub fn with_origin_opt(self, origin: Option<Origin>) -> SelectionEvent {
        SelectionEvent { origin, ..self }
    }

    /// A sentence for the prompt in place of a file reference, sanitized
    /// as the daemon would (see [`sanitize_message`]).
    pub fn with_message(self, message: &str) -> SelectionEvent {
        SelectionEvent {
            message: Some(sanitize_message(message)),
            ..self
        }
    }

    /// The event as the daemon accepts it: the message, if any, made safe
    /// to type. Applied on receipt, so no sender is trusted to have done it.
    pub fn sanitized(self) -> SelectionEvent {
        SelectionEvent {
            message: self.message.as_deref().map(sanitize_message),
            ..self
        }
    }

    /// The sending frontend, e.g. `"helix"`. Informational.
    pub fn with_source(self, source: impl Into<String>) -> SelectionEvent {
        SelectionEvent {
            source: source.into(),
            ..self
        }
    }

    /// Structural validation performed by the daemon before routing.
    pub fn validate(&self) -> Result<(), String> {
        if !self.workspace_root.is_absolute() {
            return Err("workspace_root must be absolute".into());
        }
        match &self.file {
            Some(f) if !f.is_absolute() => return Err("file must be absolute".into()),
            None if self.message.is_none() => {
                return Err("a send with no file must say something".into())
            }
            None if self.lines.is_some() || self.text.is_some() => {
                return Err("lines and text need a file".into())
            }
            _ => {}
        }
        if let Some(l) = self.lines {
            if l.start == 0 || l.end == 0 {
                return Err("line numbers are 1-based; 0 is not valid".into());
            }
            if l.end < l.start {
                return Err("end line must be >= start line".into());
            }
        }
        if self.source.is_empty() {
            return Err("source must not be empty".into());
        }
        if let Some(o) = &self.origin {
            if o.mux.is_empty() {
                return Err("origin.mux must name a multiplexer".into());
            }
        }
        if let Some(m) = &self.message {
            if m.trim().is_empty() {
                return Err("message must say something".into());
            }
            if self.kind != EventKind::Explicit {
                return Err("a message is always explicit".into());
            }
        }
        Ok(())
    }

    /// A copy with the selected text removed. Used everywhere text must not
    /// leak at default verbosity (logs, status output, follow streams).
    pub fn redacted(&self) -> SelectionEvent {
        SelectionEvent {
            text: None,
            ..self.clone()
        }
    }

    /// The file path relative to the workspace root when it lies inside it,
    /// otherwise the absolute path; for a message with no file, the
    /// message itself. For human-readable output only.
    pub fn display_path(&self) -> String {
        match (&self.file, &self.message) {
            (Some(f), _) => relative_display(f, &self.workspace_root),
            (None, Some(m)) => format!("{m:?}"),
            (None, None) => "(nothing)".into(),
        }
    }

    /// `path:start-end` (`path:line` for single-line selections, bare
    /// `path` for whole-file events).
    pub fn display_range(&self) -> String {
        match self.lines {
            None => self.display_path(),
            Some(l) if l.start == l.end => format!("{}:{}", self.display_path(), l.start),
            Some(l) => format!("{}:{}-{}", self.display_path(), l.start, l.end),
        }
    }
}

pub fn relative_display(file: &Path, root: &Path) -> String {
    match file.strip_prefix(root) {
        Ok(rel) if !rel.as_os_str().is_empty() => rel.display().to_string(),
        _ => file.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev() -> SelectionEvent {
        SelectionEvent::new("/ws", "/ws/src/main.rs", EventKind::Explicit)
            .with_lines(40, 62)
            .with_text("fn main() {}")
            .with_source("helix")
    }

    #[test]
    fn the_builder_starts_from_a_valid_whole_file_event() {
        // The bare event is the minimum a sender can mean — "this file" —
        // and must pass the daemon's own validation as-is.
        let bare = SelectionEvent::new("/ws", "/ws/src/main.rs", EventKind::Ambient);
        assert_eq!(bare.lines, None);
        assert_eq!(bare.text, None);
        assert_eq!(bare.origin, None);
        assert_eq!(bare.source, "cli");
        assert!(bare.validate().is_ok());

        // Each `with_*` sets its own field and disturbs nothing else.
        let o = Origin {
            mux: "herdr".into(),
            session: None,
            workspace: Some("w1".into()),
            tab: None,
            pane: Some("w1:p1".into()),
        };
        let full = bare
            .clone()
            .with_lines(40, 62)
            .with_text("fn main() {}")
            .with_source("helix")
            .with_origin(o.clone());
        assert_eq!(full.lines, Some(LineRange { start: 40, end: 62 }));
        assert_eq!(full.text.as_deref(), Some("fn main() {}"));
        assert_eq!(full.source, "helix");
        assert_eq!(full.origin, Some(o.clone()));
        assert_eq!(full.workspace_root, bare.workspace_root);
        assert_eq!(full.file, bare.file);
        assert_eq!(full.kind, bare.kind);
        assert!(full.validate().is_ok());

        // The `Option` variants are what `send` has in hand; passing
        // `None` leaves the field as the constructor left it.
        assert_eq!(bare.clone().with_line_range(None).lines, None);
        assert_eq!(
            bare.clone()
                .with_line_range(Some(LineRange { start: 1, end: 1 })),
            bare.clone().with_lines(1, 1)
        );
        assert_eq!(bare.clone().with_origin_opt(None).origin, None);
        assert_eq!(
            bare.clone().with_origin_opt(Some(o.clone())),
            bare.with_origin(o)
        );
    }

    #[test]
    fn serde_roundtrip() {
        let e = ev();
        let s = serde_json::to_string(&e).unwrap();
        assert!(s.contains("\"kind\":\"explicit\""));
        let back: SelectionEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(e, back);
    }

    #[test]
    fn text_is_optional_and_omitted_when_absent() {
        let e = ev().redacted();
        let s = serde_json::to_string(&e).unwrap();
        assert!(!s.contains("text"));
        let back: SelectionEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(back.text, None);
    }

    #[test]
    fn validation() {
        assert!(ev().validate().is_ok());
        let mut bad = ev();
        bad.lines = Some(LineRange { start: 0, end: 62 });
        assert!(bad.validate().is_err());
        let mut bad = ev();
        bad.lines = Some(LineRange { start: 40, end: 3 });
        assert!(bad.validate().is_err());
        let mut ok = ev();
        ok.lines = None;
        assert!(ok.validate().is_ok());
        let mut bad = ev();
        bad.file = Some("relative.rs".into());
        assert!(bad.validate().is_err());
    }

    #[test]
    fn display() {
        assert_eq!(ev().display_range(), "src/main.rs:40-62");
        let mut e = ev();
        e.lines = Some(LineRange { start: 40, end: 40 });
        assert_eq!(e.display_range(), "src/main.rs:40");
        e.lines = None;
        assert_eq!(e.display_range(), "src/main.rs");
        e.file = Some("/elsewhere/x.rs".into());
        assert_eq!(e.display_path(), "/elsewhere/x.rs");
    }

    #[test]
    fn origin_is_optional_and_survives_a_roundtrip() {
        let e = ev();
        assert!(!serde_json::to_string(&e).unwrap().contains("origin"));

        let mut e = ev();
        e.origin = Some(Origin {
            mux: "herdr".into(),
            session: None,
            workspace: Some("w1".into()),
            tab: Some("w1:t2".into()),
            pane: Some("w1:p3".into()),
        });
        let s = serde_json::to_string(&e).unwrap();
        assert!(!s.contains("session"), "unset coordinates are omitted: {s}");
        assert_eq!(serde_json::from_str::<SelectionEvent>(&s).unwrap(), e);
        assert!(e.validate().is_ok());

        // An event from an older sender has no origin field at all.
        let old = r#"{"workspace_root":"/ws","file":"/ws/a.rs","kind":"ambient","source":"t"}"#;
        assert_eq!(
            serde_json::from_str::<SelectionEvent>(old).unwrap().origin,
            None
        );
    }

    #[test]
    fn origin_must_name_a_multiplexer() {
        let mut bad = ev();
        bad.origin = Some(Origin {
            mux: String::new(),
            session: None,
            workspace: Some("w1".into()),
            tab: None,
            pane: None,
        });
        assert!(bad.validate().is_err());
    }

    #[test]
    fn a_message_is_one_safe_line_and_always_explicit() {
        let e = SelectionEvent::new("/ws", "/ws/m.map", EventKind::Explicit)
            .with_message("Regarding m --rect 1,2,3,4: \n\x1b[2Jrm -rf /\t");
        assert_eq!(
            e.message.as_deref(),
            Some("Regarding m --rect 1,2,3,4:   [2Jrm -rf / ")
        );
        assert!(e.validate().is_ok());
        // What a sender put on the wire is not trusted: the daemon
        // sanitizes again on receipt.
        let mut raw = e.clone();
        raw.message = Some("one\ntwo".into());
        assert_eq!(raw.sanitized().message.as_deref(), Some("one two"));
        assert!(
            !serde_json::to_string(&SelectionEvent::new("/ws", "/ws/a", EventKind::Explicit))
                .unwrap()
                .contains("message")
        );

        let long = "x".repeat(MAX_MESSAGE_CHARS + 50);
        assert_eq!(sanitize_message(&long).chars().count(), MAX_MESSAGE_CHARS);

        let mut bad = e.clone();
        bad.message = Some("  ".into());
        assert!(bad.validate().is_err());
        let mut bad = e;
        bad.kind = EventKind::Ambient;
        assert!(bad.validate().is_err());

        // A message needs no file, and is the only thing that does not.
        let said = SelectionEvent::message("/ws", "Regarding m --rect 1,2,3,4: ");
        assert_eq!(said.file, None);
        assert_eq!(said.kind, EventKind::Explicit);
        assert!(said.validate().is_ok());
        assert_eq!(said.display_path(), "\"Regarding m --rect 1,2,3,4: \"");
        assert!(!serde_json::to_string(&said).unwrap().contains("file"));
        let mut bad = said.clone();
        bad.message = None;
        assert!(bad.validate().is_err());
        let mut bad = said;
        bad.lines = Some(LineRange { start: 1, end: 1 });
        assert!(bad.validate().is_err());
    }

    #[test]
    fn lines_are_optional_and_omitted_when_absent() {
        let mut e = ev();
        e.lines = None;
        let s = serde_json::to_string(&e).unwrap();
        assert!(!s.contains("lines"));
        let back: SelectionEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(back.lines, None);
    }
}
