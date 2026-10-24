//! WS-layer tests with a fake Claude Code client: handshake, auth failures,
//! Origin rejection, notification framing, tools.

mod common;

use common::{connect, run_local, Ws};
use futures_util::{SinkExt, StreamExt};
use selx::adapters::claude_ide::{lock, ClaudeIdeAdapter, ClaudeIdeOptions};
use selx::core::{Adapter, Delivery, EventKind, SelectionEvent};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

struct Harness {
    adapter: ClaudeIdeAdapter,
    port: u16,
    root: PathBuf,
    _lock_dir: tempfile::TempDir,
    _root_dir: tempfile::TempDir,
}

async fn harness() -> Harness {
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    let lock_dir = tempfile::tempdir().unwrap();
    let root_dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(root_dir.path()).unwrap();
    let adapter = ClaudeIdeAdapter::start(ClaudeIdeOptions {
        root: root.clone(),
        port,
        listener,
        lock_dir: lock_dir.path().to_path_buf(),
        ide_name: "selx-test".into(),
        verbose: false,
        // No herdr in these tests: deliveries name no pane.
        herdr_socket: Some("/nonexistent/herdr.sock".into()),
    })
    .unwrap();
    Harness {
        adapter,
        port,
        root,
        _lock_dir: lock_dir,
        _root_dir: root_dir,
    }
}

async fn call(ws: &mut Ws, id: u64, method: &str, params: Value) -> Value {
    let req = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
    ws.send(Message::text(req.to_string())).await.unwrap();
    let reply = recv(ws).await;
    assert_eq!(reply["id"], id, "{reply}");
    reply
}

async fn recv(ws: &mut Ws) -> Value {
    let msg = tokio::time::timeout(Duration::from_secs(5), ws.next())
        .await
        .expect("timed out")
        .expect("closed")
        .unwrap();
    serde_json::from_str(msg.to_text().unwrap()).unwrap()
}

fn event(h: &Harness, kind: EventKind, start: u32, end: u32, text: Option<&str>) -> SelectionEvent {
    let e = SelectionEvent::new(&h.root, h.root.join("main.rs"), kind)
        .with_lines(start, end)
        .with_source("test");
    match text {
        Some(t) => e.with_text(t),
        None => e,
    }
}

#[test]
fn full_handshake_and_notifications() {
    run_local(async {
        let h = harness().await;

        // Lock file matches what Claude will read.
        let lock = lock::read(h._lock_dir.path(), h.port).unwrap();
        assert_eq!(lock.auth_token, h.adapter.auth_token());
        assert_eq!(lock.workspace_folders, vec![h.root.clone()]);
        assert_eq!(lock.ide_name, "selx-test");
        assert_eq!(lock.transport, "ws");
        assert_eq!(lock.pid, std::process::id());

        // Nobody connected yet: explicit event is dropped, not failed.
        assert_eq!(
            h.adapter
                .deliver(&event(&h, EventKind::Explicit, 1, 2, None))
                .unwrap(),
            Delivery::Dropped("no Claude Code client connected".into())
        );
        assert_eq!(h.adapter.client_count(), 0);

        let mut ws = connect(h.port, Some(h.adapter.auth_token()), None)
            .await
            .expect("handshake");
        tokio::task::yield_now().await;
        assert_eq!(h.adapter.client_count(), 1);

        // MCP handshake as Claude performs it.
        let init = call(
            &mut ws,
            0,
            "initialize",
            json!({ "protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": { "name": "claude-code", "version": "2.1.238" } }),
        )
        .await;
        assert_eq!(init["result"]["protocolVersion"], "2024-11-05");
        assert_eq!(init["result"]["serverInfo"]["name"], "selx");
        assert!(init["result"]["capabilities"]["tools"].is_object());
        ws.send(Message::text(
            json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }).to_string(),
        ))
        .await
        .unwrap();

        let tools = call(&mut ws, 1, "tools/list", json!({})).await;
        let names: Vec<&str> = tools["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"getCurrentSelection"), "{names:?}");
        assert!(
            !names.contains(&"openDiff"),
            "must not advertise editing tools"
        );
        assert_eq!(
            call(&mut ws, 2, "ping", json!({})).await["result"],
            json!({})
        );
        assert_eq!(
            call(&mut ws, 3, "prompts/list", json!({})).await["result"]["prompts"],
            json!([])
        );
        assert_eq!(
            call(&mut ws, 4, "nope", json!({})).await["error"]["code"],
            -32601
        );

        // The event dropped earlier (nobody connected) is still retained as the
        // latest selection, so a Claude that connects later can ask for it.
        let sel = call(
            &mut ws,
            5,
            "tools/call",
            json!({ "name": "getLatestSelection", "arguments": {} }),
        )
        .await;
        let body: Value =
            serde_json::from_str(sel["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(body["success"], true);
        assert_eq!(body["selection"]["start"]["line"], 0);
        assert_eq!(body["selection"]["end"]["line"], 1);

        // Explicit event → at_mentioned with 0-based lines.
        std::fs::write(h.root.join("main.rs"), "one\ntwo\nthree\nfour\n").unwrap();
        assert_eq!(
            h.adapter
                .deliver(&event(&h, EventKind::Explicit, 2, 3, None))
                .unwrap(),
            Delivery::delivered()
        );
        let n = recv(&mut ws).await;
        assert_eq!(n["method"], "at_mentioned");
        assert_eq!(
            n["params"],
            json!({ "filePath": h.root.join("main.rs"), "lineStart": 1, "lineEnd": 2 })
        );
        assert!(n.get("id").is_none());

        // Ambient event → selection_changed; text read from disk.
        assert_eq!(
            h.adapter
                .deliver(&event(&h, EventKind::Ambient, 2, 3, None))
                .unwrap(),
            Delivery::delivered()
        );
        let n = recv(&mut ws).await;
        assert_eq!(n["method"], "selection_changed");
        assert_eq!(n["params"]["text"], "two\nthree");
        assert_eq!(
            n["params"]["selection"]["start"],
            json!({ "line": 1, "character": 0 })
        );
        assert_eq!(
            n["params"]["selection"]["end"],
            json!({ "line": 2, "character": 5 })
        );
        assert_eq!(n["params"]["selection"]["isEmpty"], false);
        assert_eq!(
            n["params"]["fileUrl"],
            format!("file://{}", h.root.join("main.rs").display())
        );

        // Frontend-supplied text wins over disk.
        h.adapter
            .deliver(&event(&h, EventKind::Ambient, 1, 1, Some("custom")))
            .unwrap();
        assert_eq!(recv(&mut ws).await["params"]["text"], "custom");

        // getLatestSelection reflects the last event.
        let sel = call(
            &mut ws,
            6,
            "tools/call",
            json!({ "name": "getCurrentSelection", "arguments": {} }),
        )
        .await;
        let body: Value =
            serde_json::from_str(sel["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(body["success"], true);
        assert_eq!(body["text"], "custom");

        let ws_folders = call(
            &mut ws,
            7,
            "tools/call",
            json!({ "name": "getWorkspaceFolders", "arguments": {} }),
        )
        .await;
        let body: Value =
            serde_json::from_str(ws_folders["result"]["content"][0]["text"].as_str().unwrap())
                .unwrap();
        assert_eq!(body["rootPath"], h.root.to_str().unwrap());
        assert_eq!(
            call(&mut ws, 8, "tools/call", json!({ "name": "openDiff" })).await["error"]["code"],
            -32602
        );
        // Claude calls this after every turn without it being advertised.
        let closed = call(
            &mut ws,
            9,
            "tools/call",
            json!({ "name": "closeAllDiffTabs" }),
        )
        .await;
        assert_eq!(closed["result"]["content"][0]["text"], "CLOSED_0_DIFF_TABS");

        // Second client: broadcast reaches both.
        let mut ws2 = connect(h.port, Some(h.adapter.auth_token()), None)
            .await
            .unwrap();
        tokio::task::yield_now().await;
        assert_eq!(h.adapter.client_count(), 2);
        h.adapter
            .deliver(&event(&h, EventKind::Explicit, 4, 4, None))
            .unwrap();
        assert_eq!(recv(&mut ws).await["params"]["lineStart"], 3);
        assert_eq!(recv(&mut ws2).await["params"]["lineStart"], 3);

        // Disconnect is noticed.
        ws2.close(None).await.unwrap();
        for _ in 0..50 {
            if h.adapter.client_count() == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(h.adapter.client_count(), 1);

        // Shutdown removes the lock file.
        h.adapter.shutdown();
        assert!(!h.adapter.lock_path().exists());
    });
}

/// A message goes by typing into Claude's pane, never over the socket:
/// the protocol's mention has no room for a sentence. With no herdr to
/// find the pane through, it is dropped with the reason, and a connected
/// Claude hears nothing.
#[test]
fn a_message_is_typed_not_sent_over_the_socket() {
    run_local(async {
        let h = harness().await;
        let mut ws = connect(h.port, Some(h.adapter.auth_token()), None)
            .await
            .unwrap();
        let said = SelectionEvent::message(
            &h.root,
            "Regarding m --rect 1,2,3,4 (selected in the editor): ",
        )
        .with_source("risced");
        match h.adapter.deliver(&said).unwrap() {
            Delivery::Dropped(r) => assert!(r.contains("herdr is not running"), "{r}"),
            other => panic!("{other:?}"),
        }
        // A plain mention still goes over the socket afterwards, so the
        // connection was untouched by the message.
        assert_eq!(
            h.adapter
                .deliver(&event(&h, EventKind::Explicit, 1, 1, None))
                .unwrap(),
            Delivery::delivered()
        );
        let note = recv(&mut ws).await;
        assert_eq!(note["method"], "at_mentioned", "{note}");
    });
}

#[test]
fn bad_auth_and_origin_are_rejected() {
    run_local(async {
        let h = harness().await;
        let good = h.adapter.auth_token().to_string();

        let err = connect(h.port, None, None)
            .await
            .expect_err("missing token must be rejected");
        assert!(err.contains("401"), "{err}");

        let err = connect(h.port, Some("00000000-0000-0000-0000-000000000000"), None)
            .await
            .expect_err("wrong token");
        assert!(err.contains("401"), "{err}");

        // Right length, one char off.
        let mut near = good.clone();
        near.replace_range(0..1, if &good[0..1] == "a" { "b" } else { "a" });
        assert!(connect(h.port, Some(&near), None).await.is_err());

        // Correct token but a browser Origin: rejected (CVE-style attack).
        let err = connect(h.port, Some(&good), Some("http://evil.example"))
            .await
            .expect_err("origin must be rejected");
        assert!(err.contains("403"), "{err}");

        assert_eq!(h.adapter.client_count(), 0);
        // And the server is still fine afterwards.
        assert!(connect(h.port, Some(&good), None).await.is_ok());
    });
}

#[test]
fn malformed_frames_do_not_kill_the_connection() {
    run_local(async {
        let h = harness().await;
        let mut ws = connect(h.port, Some(h.adapter.auth_token()), None)
            .await
            .unwrap();
        ws.send(Message::text("not json")).await.unwrap();
        assert_eq!(recv(&mut ws).await["error"]["code"], -32700);
        // A response-shaped frame (no method) is ignored, connection stays up.
        ws.send(Message::text(
            json!({ "jsonrpc": "2.0", "id": 9, "result": {} }).to_string(),
        ))
        .await
        .unwrap();
        assert_eq!(
            call(&mut ws, 10, "ping", json!({})).await["result"],
            json!({})
        );
    });
}
