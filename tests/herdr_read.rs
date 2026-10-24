//! What `selx reload` reads from herdr, against a real server: a pane's
//! screen as text, and which pane has the keyboard.

mod common;

use common::{herdr_bin, Session};
use selx::adapters::herdr::HerdrClient;
use std::time::Duration;

#[test]
fn a_panes_screen_and_focus_can_be_read() {
    let Some(bin) = herdr_bin() else {
        eprintln!("herdr not installed; skipping");
        return;
    };
    let session = Session::start(bin, "read");
    let ws = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(ws.path()).unwrap();
    let w = session.create_workspace(&root);
    let other = session.split(&w.editor, "right", &root);

    session.cli(&["pane", "run", &w.editor, "echo selx-read-$((6*7))"]);
    session.wait_for_text(&w.editor, "selx-read-42", Duration::from_secs(10));
    let client = HerdrClient::new(session.socket.clone(), HerdrClient::DEFAULT_TIMEOUT);
    let text = client.read_visible(&w.editor).unwrap();
    assert!(text.contains("selx-read-42"), "{text}");
    // Rows come back as lines, which is what the screen check counts on.
    assert!(text.lines().count() > 1, "{text:?}");

    for pane in [&other, &w.editor] {
        client.focus_pane(pane).unwrap();
        let panes = client.list_panes(Some(&w.id)).unwrap();
        let focused: Vec<&str> = panes
            .iter()
            .filter(|p| p.focused)
            .map(|p| p.pane_id.as_str())
            .collect();
        assert_eq!(focused, [pane.as_str()]);
    }
}
