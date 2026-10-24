//! JSON-RPC 2.0 / MCP-flavoured request handling for Claude Code.

use super::Inner;
use serde_json::{json, Value};
use tracing::{debug, info, warn};

/// The MCP revision claudecode.nvim advertises; Claude accepts it.
pub const MCP_PROTOCOL_VERSION: &str = "2024-11-05";
pub const SERVER_NAME: &str = "selx";
pub const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn notification(method: &str, params: Value) -> String {
    json!({ "jsonrpc": "2.0", "method": method, "params": params }).to_string()
}

fn response(id: &Value, result: Value) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string()
}

fn error(id: &Value, code: i64, message: &str) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }).to_string()
}

fn text_result(text: String) -> Value {
    json!({ "content": [{ "type": "text", "text": text }] })
}

fn empty_schema() -> Value {
    json!({ "type": "object", "additionalProperties": false, "$schema": "http://json-schema.org/draft-07/schema#" })
}

/// The tools we advertise. Deliberately minimal: selx has no editor to open
/// files or diffs in, and advertising `openDiff`/`openFile` would make Claude
/// route edits through us. Read-only tools only.
pub fn tool_list() -> Value {
    json!([
        { "name": "getCurrentSelection", "description": "Get the current text selection in the editor", "inputSchema": empty_schema() },
        { "name": "getLatestSelection", "description": "Get the most recent text selection", "inputSchema": empty_schema() },
        { "name": "getWorkspaceFolders", "description": "Get all workspace folders currently open in the IDE", "inputSchema": empty_schema() },
        { "name": "getOpenEditors", "description": "Get information about currently open editors", "inputSchema": empty_schema() },
    ])
}

/// Handle one inbound text frame. Returns the frame to send back, if any.
pub(crate) fn handle(inner: &Inner, client: u64, text: &str) -> Option<String> {
    let msg: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(e) => {
            warn!(client, "unparseable frame from Claude: {e}");
            return Some(error(&Value::Null, -32700, "Parse error"));
        }
    };
    let id = msg.get("id").cloned();
    let method = msg.get("method").and_then(|m| m.as_str());
    let params = msg.get("params").cloned().unwrap_or(Value::Null);

    let Some(method) = method else {
        // A response to a request we never sent (we send none); ignore.
        debug!(client, "ignoring frame without method: {text}");
        return None;
    };
    let Some(id) = id else {
        // Notification.
        match method {
            "notifications/initialized" => info!(client, "Claude Code handshake complete"),
            // Observed (2.1.x): sent right after the handshake with Claude's pid.
            "ide_connected" => info!(
                client,
                claude_pid = params["pid"].as_u64().unwrap_or(0),
                "Claude Code reports connected"
            ),
            other => debug!(client, "notification {other}: {params}"),
        }
        return None;
    };

    debug!(client, method, "request");
    let result = match method {
        "initialize" => {
            let ci = &params["clientInfo"];
            info!(
                client,
                name = ci["name"].as_str().unwrap_or("?"),
                version = ci["version"].as_str().unwrap_or("?"),
                protocol = params["protocolVersion"].as_str().unwrap_or("?"),
                "Claude Code initialize"
            );
            json!({
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": {
                    "logging": {},
                    "prompts": { "listChanged": true },
                    "tools": { "listChanged": true },
                },
                "serverInfo": { "name": SERVER_NAME, "version": SERVER_VERSION },
            })
        }
        "ping" => json!({}),
        "prompts/list" => json!({ "prompts": [] }),
        "tools/list" => json!({ "tools": tool_list() }),
        "tools/call" => {
            let name = params["name"].as_str().unwrap_or("");
            info!(client, tool = name, "Claude Code tool call");
            match call_tool(inner, name) {
                Some(v) => v,
                None => return Some(error(&id, -32602, "Unknown tool")),
            }
        }
        _ => return Some(error(&id, -32601, "Method not found")),
    };
    Some(response(&id, result))
}

fn call_tool(inner: &Inner, name: &str) -> Option<Value> {
    let body = match name {
        "getCurrentSelection" | "getLatestSelection" => match inner.latest.borrow().as_ref() {
            Some(sel) => {
                let mut v = sel.to_value();
                v["success"] = json!(true);
                v
            }
            None => json!({ "success": false, "message": "No selection available" }),
        },
        "getWorkspaceFolders" => {
            let root = inner.root.to_string_lossy();
            let name = inner
                .root
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            json!({
                "success": true,
                "folders": [{ "name": name, "uri": format!("file://{root}"), "path": root }],
                "rootPath": root,
            })
        }
        "getOpenEditors" => json!({ "tabs": [] }),
        // Not advertised, but Claude calls it after every turn anyway; a
        // benign answer keeps its logs clean. We never open diff tabs.
        "closeAllDiffTabs" => return Some(text_result("CLOSED_0_DIFF_TABS".into())),
        _ => return None,
    };
    Some(text_result(body.to_string()))
}
