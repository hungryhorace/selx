//! Finding Claude's pane, against a real herdr server: a registered pane
//! beats one that merely looks like Claude, and focus follows an ambient
//! send into it — the gesture the whole feature exists for.

mod common;

use common::{connect, event_from, herdr_bin, run_local, Session};
use selx::adapters::claude_ide::{ClaudeIdeAdapter, ClaudeIdeOptions};
use selx::adapters::herdr::{HerdrClient, PaneFocuser, MUX};
use selx::core::{Adapter, Delivery, Focuser, Origin};

#[test]
fn focus_follows_an_ambient_send_into_the_registered_claude() {
    let Some(bin) = herdr_bin() else {
        eprintln!("herdr not installed; skipping");
        return;
    };
    run_local(async {
        let session = Session::start(bin, "claude");
        let ws_dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(ws_dir.path()).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();

        // One tab: the editor, a Claude started with `selx claude`, and a
        // bare `claude` that herdr recognizes but that never connected here.
        let ws = session.create_workspace(&root);
        let ours = session.split(&ws.editor, "down", &root);
        let bare = session.split(&ours, "down", &root);
        // `bare` is reported last, so it is the most recently active: only
        // registration can make `ours` win.
        session.report_agent(&ours, "claude", "idle", None);
        session.report_agent(&bare, "claude", "idle", None);

        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let lock_dir = tempfile::tempdir().unwrap();
        let adapter = ClaudeIdeAdapter::start(ClaudeIdeOptions {
            root: root.clone(),
            port,
            listener,
            lock_dir: lock_dir.path().to_path_buf(),
            ide_name: "selx-test".into(),
            verbose: false,
            herdr_socket: Some(session.socket.clone()),
        })
        .unwrap();
        let _client = connect(port, Some(adapter.auth_token()), None)
            .await
            .expect("handshake");
        tokio::task::yield_now().await;
        assert_eq!(adapter.client_count(), 1);

        // `<space>t` in Helix: ambient, which only this adapter carries.
        let mut send = event_from(&root, &ws.id, &ws.tab, &ws.editor);
        send.kind = selx::core::EventKind::Ambient;

        // Before anyone registers, any pane that looks like Claude will do,
        // and the busiest wins — which is the bare one.
        assert_eq!(
            adapter.deliver(&send).unwrap(),
            Delivery::Delivered(Some(bare.clone()))
        );

        // `selx claude` registers its pane; now the connected Claude wins.
        adapter.register_origin(&Origin {
            mux: MUX.into(),
            session: Some(session.name.clone()),
            workspace: Some(ws.id.clone()),
            tab: Some(ws.tab.clone()),
            pane: Some(ours.clone()),
        });
        assert_eq!(
            adapter.deliver(&send).unwrap(),
            Delivery::Delivered(Some(ours.clone()))
        );

        // And that is where the user ends up, ready to type "explain this".
        let herdr = HerdrClient::new(session.socket.clone(), HerdrClient::DEFAULT_TIMEOUT);
        herdr.focus_pane(&ws.editor).unwrap();
        let focuser = PaneFocuser {
            socket: Some(session.socket.clone()),
        };
        assert_eq!(
            focuser.focus(std::slice::from_ref(&ours), &send).unwrap(),
            Some(ours.clone())
        );
        assert!(session.pane_focused(&ours));
        assert!(!session.pane_focused(&bare));

        // A Claude parked on a permission prompt is still where to go — the
        // inverse of the injection rule.
        session.report_agent(&ours, "claude", "blocked", None);
        herdr.focus_pane(&ws.editor).unwrap();
        assert_eq!(
            adapter.deliver(&send).unwrap(),
            Delivery::Delivered(Some(ours.clone()))
        );
        assert_eq!(
            focuser.focus(std::slice::from_ref(&ours), &send).unwrap(),
            Some(ours.clone())
        );
        assert!(
            session.pane_focused(&ours),
            "a blocked Claude is still focused"
        );
    });
}
