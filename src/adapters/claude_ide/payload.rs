//! Notification payloads in the shapes Claude Code expects.

use crate::core::{LineRange, SelectionEvent};
use serde::Serialize;
use serde_json::{json, Value};
use std::path::Path;

/// Don't slurp huge files just to echo a selection.
const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Position {
    /// 0-based.
    pub line: u32,
    pub character: u32,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Range {
    pub start: Position,
    pub end: Position,
    pub is_empty: bool,
}

/// `selection_changed` params; also the body of `getCurrentSelection`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Selection {
    pub text: String,
    pub file_path: String,
    pub file_url: String,
    pub selection: Range,
}

impl Selection {
    /// Lines are converted from selx's 1-based inclusive range to the
    /// 0-based LSP-style positions Claude expects. If the frontend did not
    /// supply the text, read it from disk (best effort). A whole-file event
    /// (no lines) becomes an empty selection at the top of the file — "this
    /// file is active", with no line-level claim.
    pub fn from_event(ev: &SelectionEvent) -> Selection {
        // A message has no file and is typed, not sent as a selection;
        // this is only reached for a file event.
        let file = ev.file.clone().unwrap_or_default();
        let (start_line, end_line, text) = match ev.lines {
            Some(l) => {
                let text = ev
                    .text
                    .clone()
                    .or_else(|| read_lines(&file, l))
                    .unwrap_or_default();
                (zero_based(l.start), zero_based(l.end), text)
            }
            None => (0, 0, String::new()),
        };
        let end_char = text
            .lines()
            .last()
            .map(|l| l.chars().count() as u32)
            .unwrap_or(0);
        let file_path = file.to_string_lossy().into_owned();
        Selection {
            file_url: format!("file://{file_path}"),
            file_path,
            selection: Range {
                start: Position {
                    line: start_line,
                    character: 0,
                },
                end: Position {
                    line: end_line,
                    character: end_char,
                },
                is_empty: text.is_empty(),
            },
            text,
        }
    }

    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).expect("Selection serializes")
    }
}

/// `at_mentioned` params. Line numbers are 0-based, as claudecode.nvim sends
/// them (`start_line - 1`, documented "0-indexed for Claude"). For a
/// whole-file event both keys are omitted, again matching claudecode.nvim;
/// Claude then renders a bare `@file` with no `#L` suffix.
pub fn at_mentioned(ev: &SelectionEvent) -> Value {
    let mut v = json!({ "filePath": ev.file.clone().unwrap_or_default().to_string_lossy() });
    if let Some(l) = ev.lines {
        v["lineStart"] = zero_based(l.start).into();
        v["lineEnd"] = zero_based(l.end).into();
    }
    v
}

/// A 1-based line number as the 0-based one Claude expects. Saturating: the
/// daemon rejects 0, but this module is `pub` two crates away from that
/// guard and must not panic on a range it never validated itself.
fn zero_based(line: u32) -> u32 {
    line.saturating_sub(1)
}

/// Lines `lines.start..=lines.end` (1-based, inclusive) of `path`, joined
/// with `\n`. `None` if the file is unreadable, too large, has no lines in
/// range, or the range is inverted.
pub fn read_lines(path: &Path, lines: LineRange) -> Option<String> {
    if lines.end < lines.start {
        return None;
    }
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_FILE_BYTES {
        return None;
    }
    let content = std::fs::read_to_string(path).ok()?;
    let lines: Vec<&str> = content
        .lines()
        .skip(zero_based(lines.start) as usize)
        .take((lines.end - lines.start) as usize + 1)
        .collect();
    if lines.is_empty() {
        return None;
    }
    Some(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::EventKind;

    fn ev(text: Option<&str>) -> SelectionEvent {
        let e = SelectionEvent::new("/ws", "/ws/src/a.rs", EventKind::Ambient)
            .with_lines(10, 12)
            .with_source("t");
        match text {
            Some(t) => e.with_text(t),
            None => e,
        }
    }

    #[test]
    fn selection_changed_uses_zero_based_lines() {
        let s = Selection::from_event(&ev(Some("ab\ncde\nf")));
        assert_eq!(
            s.selection.start,
            Position {
                line: 9,
                character: 0
            }
        );
        assert_eq!(
            s.selection.end,
            Position {
                line: 11,
                character: 1
            }
        );
        assert!(!s.selection.is_empty);
        assert_eq!(s.file_url, "file:///ws/src/a.rs");
        let v = s.to_value();
        assert_eq!(v["filePath"], "/ws/src/a.rs");
        assert_eq!(v["selection"]["isEmpty"], false);
    }

    #[test]
    fn at_mentioned_is_zero_based() {
        let v = at_mentioned(&ev(None));
        assert_eq!(
            v,
            json!({"filePath": "/ws/src/a.rs", "lineStart": 9, "lineEnd": 11})
        );
    }

    #[test]
    fn whole_file_at_mention_omits_lines() {
        let mut e = ev(None);
        e.lines = None;
        let v = at_mentioned(&e);
        assert_eq!(v, json!({"filePath": "/ws/src/a.rs"}));
    }

    #[test]
    fn whole_file_selection_is_empty_at_top() {
        let mut e = ev(Some("ignored"));
        e.lines = None;
        let s = Selection::from_event(&e);
        assert_eq!(s.text, "");
        assert!(s.selection.is_empty);
        assert_eq!(
            s.selection.start,
            Position {
                line: 0,
                character: 0
            }
        );
        assert_eq!(
            s.selection.end,
            Position {
                line: 0,
                character: 0
            }
        );
    }

    #[test]
    fn missing_text_is_read_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("x.txt");
        std::fs::write(&f, "l1\nl2\nl3\nl4\n").unwrap();
        let mut e = ev(None);
        e.file = Some(f.clone());
        e.lines = Some(LineRange { start: 2, end: 3 });
        let s = Selection::from_event(&e);
        assert_eq!(s.text, "l2\nl3");
        assert_eq!(s.selection.end.character, 2);
        // Out of range → empty selection, no panic.
        e.lines = Some(LineRange { start: 10, end: 12 });
        let s = Selection::from_event(&e);
        assert_eq!(s.text, "");
        assert!(s.selection.is_empty);
        // Unreadable file → empty.
        e.file = Some(dir.path().join("nope"));
        assert!(Selection::from_event(&e).selection.is_empty);
        // An inverted range is nothing, not a subtraction overflow: the
        // daemon rejects it, but nothing stops another caller passing one.
        assert_eq!(read_lines(&f, LineRange { start: 3, end: 2 }), None);
        assert_eq!(
            read_lines(&f, LineRange { start: 2, end: 2 }),
            Some("l2".into())
        );
    }
}
