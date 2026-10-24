//! Focus follows delivery, against a real herdr server: after a send lands
//! in an agent's pane, that pane holds the focus.

mod common;

use common::{event, event_from, herdr_bin, Session};
use selx::adapters::herdr::{HerdrClient, PaneFocuser};
use selx::adapters::text_inject::{TextInjectAdapter, TextInjectOptions};
use selx::core::{Adapter, Delivery, EventKind, Focuser};

#[test]
fn a_delivered_send_moves_focus_to_the_agents_pane() {
    let Some(bin) = herdr_bin() else {
        eprintln!("herdr not installed; skipping");
        return;
    };
    let session = Session::start(bin, "focus");
    let ws = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(ws.path()).unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();

    // The editor and an agent splitting one tab — the layout the feature is
    // for: select in Helix, and end up in the agent's pane ready to type.
    let ws1 = session.create_workspace(&root);
    let editor = ws1.editor.clone();
    let agent = session.split(&editor, "right", &root);
    session.report_agent(&agent, "codex", "idle", None);

    let client = HerdrClient::new(session.socket.clone(), HerdrClient::DEFAULT_TIMEOUT);
    let focuser = PaneFocuser {
        socket: Some(session.socket.clone()),
    };
    let mut opts = TextInjectOptions::new(root.clone());
    opts.socket = Some(session.socket.clone());
    let adapter = TextInjectAdapter::new(opts);
    let send = event_from(&root, &ws1.id, &ws1.tab, &editor);

    // The user is in the editor.
    client.focus_pane(&editor).unwrap();
    assert!(session.pane_focused(&editor));

    // An explicit send: the reference is typed, and focus follows it.
    let Delivery::Delivered(Some(pane)) = adapter.deliver(&send).unwrap() else {
        panic!("explicit send was not delivered to a pane");
    };
    assert_eq!(pane, agent);
    assert_eq!(focuser.focus(&[pane], &send).unwrap(), Some(agent.clone()));
    assert!(session.pane_focused(&agent), "the agent's pane holds focus");
    assert!(!session.pane_focused(&editor));

    // An ambient send this adapter cannot express is dropped, so no pane is
    // reported and the daemon has nothing to focus: the user stays put.
    client.focus_pane(&editor).unwrap();
    let ambient = event(&root, EventKind::Ambient);
    assert!(matches!(
        adapter.deliver(&ambient).unwrap(),
        Delivery::Dropped(_)
    ));
    assert!(session.pane_focused(&editor), "a drop must not move focus");

    // And the sending pane is never focused, whatever was delivered there.
    assert_eq!(
        focuser.focus(std::slice::from_ref(&editor), &send).unwrap(),
        None
    );
    assert!(session.pane_focused(&editor));
}
