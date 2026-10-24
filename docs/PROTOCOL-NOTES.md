# Claude Code IDE protocol — selx notes

What selx depends on, where it came from, and what we saw on the wire.
The protocol is **reverse engineered and observed, not guaranteed**;
Anthropic can change it in any release. Everything below is pinned so that
breakage is diagnosable. Sources:

- `coder/claudecode.nvim` — `PROTOCOL.md` plus `lua/claudecode/{lockfile,server/*,selection,init,tools/*}.lua` (read at commit `2390c6e`).
- A real VS Code extension lock file on the dev machine.
- Live sessions against **Claude Code 2.1.238** driven by `scripts/claude-interactive-smoke.py`; the same script still connects and delivers (explicit and ambient) against 2.1.309. The wire details below were captured at 2.1.238.

## Discovery

| Item | Value | Source |
|---|---|---|
| Lock file | `$CLAUDE_CONFIG_DIR/ide/<port>.lock`, else `~/.claude/ide/<port>.lock` | nvim `lockfile.lua`; VS Code file at `~/.claude/ide/22685.lock` |
| Lock JSON | `{"pid","workspaceFolders":[…],"ideName","transport":"ws","runningInWindows":false,"authToken"}` | VS Code file verbatim; the 2.1.238 binary's schema names `workspaceFolders`, `ideName`, `runningInWindows`, `useWebSocket` |
| Token | VS Code uses a UUID v4; nvim uses 32 hex chars. selx uses UUID v4. | observed |
| Modes | dir 0700, file 0600, written atomically (tmp + rename) | nvim; matches selx |
| Env for `claude` | `CLAUDE_CODE_SSE_PORT=<port>` selects the lock file. `ENABLE_IDE_INTEGRATION=true` is **absent from the 2.1.238 binary** (0 string hits) — kept for older versions, harmless. | binary strings |
| Port range | 10000–65535 | PROTOCOL.md |
| Print mode | `claude -p …` (with or without `--ide`) **never connects** to the IDE; the `system/init` event lists `mcp_servers: []`. IDE integration is interactive-only. | observed |

## WebSocket upgrade (observed request from 2.1.238)

```
host: 127.0.0.1:<port>
connection: Upgrade
upgrade: websocket
sec-websocket-version: 13
sec-websocket-extensions: permessage-deflate; client_max_window_bits
sec-websocket-key: …
sec-websocket-protocol: mcp
user-agent: claude-code/2.1.238 (cli)
x-claude-code-ide-authorization: <token>
```

- **Auth:** `x-claude-code-ide-authorization` must equal `authToken`; selx compares in constant time and answers 401 otherwise. nvim additionally bounds the header length to 10–500 chars.
- **Subprotocol — required.** Claude requests `mcp`. If the server does not echo `Sec-WebSocket-Protocol: mcp`, Node's `ws` client closes the socket ~5 ms after the upgrade, before sending `initialize` (observed: "connected … disconnected" with nothing in between). nvim echoes whatever was requested; selx echoes the first requested value. **This is not mentioned in PROTOCOL.md.**
- **Origin:** Claude sends none. selx rejects any request carrying `Origin` with 403 (browser-origin WebSocket hijacking was a real CVE against the official extensions).
- Compression is requested but not negotiated by selx; Claude is fine with that.
- No `Origin`, no cookies, no path requirements (`/`).

## JSON-RPC session (observed order)

1. `initialize` request — `params.protocolVersion` was **`2025-11-25`**, `clientInfo {name:"claude-code", version:"2.1.238"}`. selx answers with nvim's response shape and `protocolVersion: "2024-11-05"`; Claude accepts the older revision.
   ```json
   {"protocolVersion":"2024-11-05",
    "capabilities":{"logging":{},"prompts":{"listChanged":true},"tools":{"listChanged":true}},
    "serverInfo":{"name":"selx","version":"…"}}
   ```
2. `notifications/initialized` notification.
3. `ide_connected` notification with `{"pid": <claude pid>}` — **not in PROTOCOL.md**; logged, not otherwise used.
4. `tools/list` and `prompts/list` requests. selx returns `prompts: []`.
5. After **every assistant turn**: `tools/call closeAllDiffTabs`, even though selx does not advertise it. selx answers `CLOSED_0_DIFF_TABS` (the VS Code result format) instead of an error.
6. `ping` requests are answered with `{}` (MCP standard); not yet observed from 2.1.238 in short sessions.

Unknown methods get `-32601`, unknown tools `-32602`, unparseable frames `-32700`.

## Tools selx advertises

Read-only, all with an empty object `inputSchema`:
`getCurrentSelection`, `getLatestSelection` (both return the last event as
`{"success":true,"text","filePath","fileUrl","selection"}` or
`{"success":false,"message":"No selection available"}`),
`getWorkspaceFolders` (`{"success":true,"folders":[{name,uri,path}],"rootPath"}`),
`getOpenEditors` (`{"tabs":[]}`). Results are MCP `content: [{type:"text", text:<JSON string>}]`.

Deliberately **not** advertised: `openFile`, `openDiff`, `getDiagnostics`,
`checkDocumentDirty`, `saveDocument`, `close_tab`, `executeCode`. selx has
no editor to act in, and advertising `openDiff` would make Claude route
edits through the IDE instead of the terminal.

## Notifications selx sends

### `selection_changed` (ambient)

```json
{"jsonrpc":"2.0","method":"selection_changed","params":{
  "text":"<selected text>",
  "filePath":"/abs/path","fileUrl":"file:///abs/path",
  "selection":{"start":{"line":2,"character":0},"end":{"line":4,"character":1},"isEmpty":false}}}
```

Lines are **0-based** (LSP style; nvim converts `lnum - 1`). If the frontend
did not supply `text`, selx reads the lines from disk (files ≤ 4 MiB). In
Claude's UI this appears as the hedged `⧉ Selected 3 lines from lib.rs in
selx` context line — Claude treats it as "may or may not be relevant".

### `at_mentioned` (explicit)

```json
{"jsonrpc":"2.0","method":"at_mentioned","params":{"filePath":"/abs/path","lineStart":2,"lineEnd":4}}
```

`lineStart`/`lineEnd` are **0-based** (nvim: `start_line - 1`, documented
"0-indexed for Claude"). Claude renders it as `@lib.rs#L3-5` prepended to
the next prompt and reads the file itself.

For a file-level mention, claudecode.nvim omits `lineStart`/`lineEnd`
entirely (nil params; it explicitly clears them for directory mentions
too). Claude then renders a bare `@lib.rs` with no `#L` suffix — selx does
the same for whole-file events (no `lines` on the `SelectionEvent`).

## `ideName`

selx sends `ideName: "selx"`. Claude 2.1.238 displays unknown names verbatim
("selx disconnected", "… in selx"); no special handling was observed, and
no known-name list is needed. JetBrains-style names trigger different
diff behaviour in Claude, so do not mimic those. Override with
`SELX_IDE_NAME` if an experiment needs it.

## Things to re-verify when Claude Code updates

- Subprotocol name (`mcp`) and that the echo is still required.
- `initialize` acceptance of `2024-11-05` (bump if Claude starts rejecting).
- The lock-file field set (`useWebSocket` may replace `transport`).
- Whether `closeAllDiffTabs` / other unadvertised tools are still called.
- Whether print mode gains IDE support (would make `-p` smoke tests possible).
