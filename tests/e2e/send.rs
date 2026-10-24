//! `selx send`: what it accepts, what it records, and what it tells the
//! user when nothing was listening.

use crate::{failed, succeeded, Env};
use serde_json::Value;

#[test]
fn invalid_sends_are_rejected() {
    let env = Env::new();
    let out = failed(env.run(&["send", "--file", "x.rs", "--start", "5", "--end", "3"]));
    assert!(String::from_utf8_lossy(&out.stderr).contains("end line"));
    failed(env.run(&["send", "--file", "x.rs", "--start", "0"]));
    // --end and --text-stdin make no sense without --start.
    failed(env.run(&["send", "--file", "x.rs", "--end", "3"]));
    failed(env.run(&["send", "--file", "x.rs", "--text-stdin"]));
    // No daemon was started for an invalid event.
    assert_eq!(env.run(&["status"]).status.code(), Some(2));
}

#[test]
fn say_sends_one_line_about_no_file() {
    let env = Env::new();
    failed(env.run(&["say", "  "]));
    // With nothing to type into, it is recorded and dropped, not lost.
    let out = succeeded(env.run(&[
        "say",
        "Regarding x --rect 1,2,3,4:\nrm -rf /",
        "--source",
        "risced",
        "--json",
    ]));
    let outcome: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(outcome.get("agents").is_none(), "{outcome}");
    let status = env.json(&["status", "--json"]);
    let event = &status["recent"][0]["event"];
    assert_eq!(event["message"], "Regarding x --rect 1,2,3,4: rm -rf /");
    assert_eq!(event["kind"], "explicit");
    assert!(event.get("file").is_none(), "{event}");
}

#[test]
fn whole_file_send_has_no_line_range() {
    let env = Env::new();
    succeeded(env.run(&["send", "--file", "x.rs", "--explicit", "--json"]));
    let status: Value = serde_json::from_slice(&env.run(&["status", "--json"]).stdout).unwrap();
    let event = &status["recent"][0]["event"];
    assert!(event.get("lines").is_none(), "{event}");
}

#[test]
fn send_carries_the_senders_pane_coordinates() {
    let env = Env::new();
    // No multiplexer: an origin-less event, which is a supported case.
    env.run(&["send", "--file", "x.rs", "--explicit"]);
    let status = env.json(&["status", "--json"]);
    assert!(status["recent"][0]["event"].get("origin").is_none());

    // From a herdr pane, `send` records where the keypress happened. The
    // daemon must not fill this in itself: it belongs to no pane.
    let mut c = env.cmd();
    c.args(["send", "--file", "x.rs", "--start", "3", "--explicit"])
        .env("HERDR_SESSION", "selx-e2e")
        .env("HERDR_WORKSPACE_ID", "w1")
        .env("HERDR_TAB_ID", "w1:t2")
        .env("HERDR_PANE_ID", "w1:p7");
    succeeded(c.output().expect("run selx"));
    // `recent` is oldest first, so the herdr send is the last entry.
    let status = env.json(&["status", "--json"]);
    let origin = &status["recent"].as_array().unwrap().last().unwrap()["event"]["origin"];
    assert_eq!(origin["mux"], "herdr", "{status}");
    assert_eq!(origin["session"], "selx-e2e");
    assert_eq!(origin["workspace"], "w1");
    assert_eq!(origin["tab"], "w1:t2");
    assert_eq!(origin["pane"], "w1:p7");
}

#[test]
fn a_send_no_agent_received_warns_for_both_kinds() {
    let env = Env::new();
    // Nothing agent-like is listening: no Claude client, no herdr. The
    // event log accepting the event must not pass for an agent being told,
    // or the warning would never fire.
    for kind in [
        vec!["send", "--file", "x.rs"],
        vec!["send", "--file", "x.rs", "--explicit"],
    ] {
        let out = succeeded(env.run(&kind));
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains("no agent received x.rs"), "{kind:?}: {err}");
    }
    let outcome: Value = serde_json::from_slice(
        &env.run(&["send", "--file", "x.rs", "--explicit", "--json"])
            .stdout,
    )
    .unwrap();
    assert_eq!(outcome["delivered"], serde_json::json!(["log"]));
    assert!(outcome.get("agents").is_none(), "{outcome}");
    // Nothing reached a pane, so nothing was focused.
    assert!(outcome.get("panes").is_none(), "{outcome}");
    assert!(outcome.get("focused").is_none(), "{outcome}");
}
