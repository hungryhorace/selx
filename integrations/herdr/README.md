# selx + herdr

selx delivers to non-Claude agents (Codex, Gemini CLI, opencode, …) running
as TUIs in herdr panes, via the `TextInjectAdapter`, and moves herdr's focus
to the pane that received an event. There is nothing to install here: the
daemon talks to herdr's socket API directly. This file is the reference for
both — which pane a send targets, and which pane focus follows to.

## How it works

Every `selx daemon` loads a `TextInjectAdapter` alongside the log and Claude
adapters. On an **explicit** event it:

1. lists agents via herdr's `agent.list` over the session socket,
2. picks the pane to type into (see below),
3. sends the reference text with `pane.send_text` — **no Enter**; the user
   finishes and submits the prompt.

Ambient events are dropped: typed text cannot express a weak "may or may not
be relevant" signal, so `supports_ambient()` is `false` and the router never
hands ambient events to this adapter.

The text is:

```
Regarding src/capture.rs lines 40-62 (currently selected):␠
```

## Target selection

- Candidates are recognized agents (herdr knows their kind) whose kind is not
  excluded. `claude` is excluded by default — it has its own adapter.
- `SELX_INJECT_TARGET` pins a specific agent by live name, pane id (`w1:p2`),
  or kind label. It is an explicit override and lifts every locality rule
  below for that agent.
- Otherwise locality decides. `selx send` records the herdr coordinates of
  the pane it was invoked from (`HERDR_WORKSPACE_ID`, `HERDR_TAB_ID`,
  `HERDR_PANE_ID`, `HERDR_SESSION`) on the event, and only agents in the
  **same herdr workspace** are candidates. The pane's working directory is
  not consulted: an agent that has `cd`'d into `/tmp` or a submodule is still
  yours, and one in another workspace is not, whatever directory it sits in.
- Sent from outside herdr (a plain terminal, an editor in another terminal),
  there are no coordinates, and the older rule applies instead: panes whose
  working directory is inside the daemon's workspace root.
- Candidates rank on `(shares the sender's tab, state_change_seq, revision,
  focused)`, descending. Sharing a tab with the editor beats everything else,
  so a second agent of the same kind grinding through a long task one tab
  over cannot steal the send from the one you are talking to; recency only
  breaks ties within a tab. The choice is logged.
- Agents `blocked` on an approval/question dialog are never typed into.

## Focus

After an event is routed, the daemon makes **one** `pane.focus` call for the
pane that received it — never from inside an adapter, so two adapters cannot
race to focus different panes. The rules:

- Focus follows **delivery**, not the keypress. An ambient send that no
  adapter could express reaches no pane, so focus does not move and
  `selx send` warns that no agent received it. "Focus didn't move" is the
  symptom of a drop, and it needs an explanation.
- Both kinds move focus. Ambient is the primary gesture — it is how you say
  "explain this" without composing a reference — and it is only usable from
  the agent's pane.
- The sending pane is never focused, and neither is anything else unless the
  send came *from* a herdr pane. A `selx send` from a plain terminal still
  reaches the agent; it just does not pull a herdr session the sender cannot
  see into view.
- `blocked` agents *are* focused, the inverse of the injection rule: a pane
  parked on a permission prompt is exactly where you want to be sent.
- If several panes received the event, they rank by the same key targeting
  uses (same tab as the sender first).
- Claude has no pane of its own to report, so selx infers one: panes that
  ran `selx claude` against this daemon come first, then any pane herdr
  recognizes as `claude`. That is what makes `<space>t` — an ambient send,
  which only Claude receives — land you in Claude's pane ready to type
  "explain this". A bare `claude` started without the wrapper is never
  focused while a registered one is live: it holds no connection to the
  daemon and was told nothing.

`SELX_FOCUS`, or `[focus] mode` in `~/.config/selx/config.toml`:
`delivered` (default), `explicit` (ambient sends deliver without moving you)
or `off`.

## Socket discovery

`HERDR_SOCKET_PATH` (set inside herdr panes) wins; otherwise the daemon uses
`$HERDR_SESSION`'s socket, else the default session's
`~/.config/herdr/herdr.sock` (honoring `HERDR_CONFIG_PATH`). If herdr is not
running the event is dropped with a reason, not an error.

## Environment

| Var | Meaning |
|---|---|
| `SELX_INJECT_TARGET` | agent name / pane id / kind to always inject into |
| `SELX_INJECT_EXCLUDE` | comma-separated kinds never targeted (default `claude`) |
| `SELX_FOCUS` | `delivered` (default), `explicit` or `off` |
| `HERDR_SOCKET_PATH` / `HERDR_SESSION` / `HERDR_CONFIG_PATH` | herdr socket discovery |
| `HERDR_WORKSPACE_ID` / `HERDR_TAB_ID` / `HERDR_PANE_ID` | read by `selx send` (not the daemon) to record where the keypress happened |

Verified against herdr 0.8.2 (socket API protocol 20).
