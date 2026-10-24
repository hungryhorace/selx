//! Text injection against a real herdr server: the pane selection rules of
//! the plan's §4.2, exercised over the real socket API.

mod common;

use common::{event, event_from, herdr_bin, Session};
use selx::adapters::text_inject::{TextInjectAdapter, TextInjectOptions};
use selx::core::{Adapter, Delivery, EventKind, SelectionEvent};
use std::time::Duration;

/// What the adapter types for `common::event`'s selection.
const REFERENCE: &str = "Regarding src/capture.rs lines 40-62 (currently selected):";

#[test]
fn injects_into_workspace_agent_without_submitting() {
    let Some(bin) = herdr_bin() else {
        eprintln!("herdr not installed; skipping");
        return;
    };
    let session = Session::start(bin, "cwd");
    let ws = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(ws.path()).unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();

    let pane = session.create_workspace(&root).editor;
    session.settle_cwd(&pane, &root);

    let mut opts = TextInjectOptions::new(root.clone());
    opts.socket = Some(session.socket.clone());
    let adapter = TextInjectAdapter::new(opts);

    // No agent yet: dropped, with a reason.
    match adapter.deliver(&event(&root, EventKind::Explicit)).unwrap() {
        Delivery::Dropped(r) => assert!(r.contains("no eligible"), "{r}"),
        other => panic!("{other:?}"),
    }

    // A codex agent appears in the pane (reported, so no codex install is needed).
    session.report_agent(&pane, "codex", "idle", None);
    assert_eq!(
        adapter.deliver(&event(&root, EventKind::Explicit)).unwrap(),
        Delivery::Delivered(Some(pane.clone()))
    );
    let text = session.wait_for_text(&pane, REFERENCE, Duration::from_secs(5));
    // Still at the prompt: the shell did not execute anything.
    assert!(
        !text.contains("command not found"),
        "text was submitted:\n{text}"
    );

    // Blocked agents are left alone.
    session.report_agent(&pane, "codex", "blocked", Some(2));
    match adapter.deliver(&event(&root, EventKind::Explicit)).unwrap() {
        Delivery::Dropped(r) => assert!(r.contains("blocked"), "{r}"),
        other => panic!("{other:?}"),
    }

    // Claude panes are excluded (they have their own adapter).
    session.report_agent(&pane, "claude", "idle", Some(3));
    match adapter.deliver(&event(&root, EventKind::Explicit)).unwrap() {
        Delivery::Dropped(r) => assert!(r.contains("no eligible"), "{r}"),
        other => panic!("{other:?}"),
    }

    // A sender's own sentence is typed in place of the reference.
    session.report_agent(&pane, "codex", "idle", Some(4));
    let said = SelectionEvent::message(
        &root,
        "Regarding m --rect 1,2,3,4 (selected in the editor): ",
    )
    .with_source("test");
    assert_eq!(
        adapter.deliver(&said).unwrap(),
        Delivery::Delivered(Some(pane.clone()))
    );
    session.wait_for_text(&pane, "Regarding m --rect 1,2,3,4", Duration::from_secs(5));

    // Ambient never reaches the pane even when handed over directly.
    session.report_agent(&pane, "codex", "idle", Some(5));
    assert!(matches!(
        adapter.deliver(&event(&root, EventKind::Ambient)).unwrap(),
        Delivery::Dropped(_)
    ));
    let text = session.pane_text(&pane);
    assert_eq!(text.matches("Regarding").count(), 2, "{text}");
}

/// The M5 acceptance case: a send from the editor's pane reaches the agent
/// sharing its tab, even though a busier agent one tab over — and another in
/// a second workspace on the same directory — would win on recency alone.
#[test]
fn injects_into_the_agent_sharing_the_editors_tab() {
    let Some(bin) = herdr_bin() else {
        eprintln!("herdr not installed; skipping");
        return;
    };
    let session = Session::start(bin, "tabs");
    let ws = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(ws.path()).unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();

    // Tab 1: the editor, splitting the screen with an agent.
    let tab1 = session.create_workspace(&root);
    let near = session.split(&tab1.editor, "right", &root);

    // Tab 2 of the same workspace, and a whole second workspace on the same
    // directory: both plausible under the old filesystem-plus-recency rule.
    let far = session.create_tab(&tab1.id, &root);
    let other = session.create_workspace(&root).editor;

    // Reported in this order, so the two decoys are the most recently
    // active: herdr assigns state_change_seq itself, increasing.
    for pane in [&near, &far, &other] {
        session.report_agent(pane, "codex", "idle", None);
    }

    let mut opts = TextInjectOptions::new(root.clone());
    opts.socket = Some(session.socket.clone());
    let adapter = TextInjectAdapter::new(opts);
    assert_eq!(
        adapter
            .deliver(&event_from(&root, &tab1.id, &tab1.tab, &tab1.editor))
            .unwrap(),
        Delivery::Delivered(Some(near.clone()))
    );
    session.wait_for_text(&near, REFERENCE, Duration::from_secs(5));
    for pane in [&far, &other] {
        let text = session.pane_text(pane);
        assert!(
            !text.contains("Regarding"),
            "pane {pane} was typed into:\n{text}"
        );
    }
}
