# selx design

This file is what the code does.

## Shape

```
Helix ──┐                                   ┌─ LogAdapter
lazygit ┤→ selx send → ingest.sock → core ──┼─ ClaudeIdeAdapter
other ──┘                          routing  └─ TextInjectAdapter
```

- One daemon **per workspace root**. Editors are dumb: `selx send --file F
  [--start N --end N] [--explicit]` (no `--start` means the whole file);
  they never know which agents exist.
- **Layering is the rule:** one crate, three modules in dependency order.
  `core` knows no editor and no agent; everything Claude- or herdr-specific
  is in `adapters`; the command layer (`cmd`, `config`, `env`, `out`) sits
  on both. Core exposes the `Adapter` trait, the `Router`, the daemon, and
  `ensure`. The three were once separate crates, with the compiler
  enforcing the rule; now `just layering` (part of `just test`) greps for
  a lower module importing a higher one.

## Event model

`SelectionEvent { workspace_root, file, lines?, text?, kind, source, origin? }`
with `kind ∈ { ambient, explicit }` and `lines` a 1-based inclusive range —
absent when the file as a whole is meant, with no line-level claim — see
`src/core/event.rs`.

`origin` is where the send came from, in multiplexer coordinates:
`{ mux, session?, workspace?, tab?, pane? }`, all opaque strings that core
carries and never interprets — only an adapter whose own multiplexer matches
`mux` may read them. `selx send` fills it from the herdr environment
(`HERDR_SESSION`/`WORKSPACE_ID`/`TAB_ID`/`PANE_ID`); the daemon must not,
being a long-lived per-root process that belongs to no pane. `None` — a
plain terminal, a test — is a supported case, not a degraded one.

An adapter declares `supports_ambient()`. The `Router` never hands an
`Ambient` event to an adapter that returns `false`; it records the drop in
the `RouteOutcome` instead. Adapters cannot upgrade a weak signal by
accident because they never see it.

## Root resolution (one rule, used everywhere)

1. `--root PATH` if given.
2. Else `git -C <start> rev-parse --show-toplevel`.
3. Else `<start>` itself.

Always canonicalized. A missing `git` binary or a non-repo folder is the
fallback path, not an error. For `selx send`, `<start>` is the file's
directory; if that is outside any repo, the cwd's resolution is used (a
file's own directory is not a stable pairing key).

## Ports and state

- Port = `10000 + (FNV-1a-64(root) mod 55536)`; the daemon probes upward
  (`+1, +2, …`, wrapping inside the range, `workspace::port_sequence_from`
  — one definition of the probe order) if the port is busy, and records the
  actual port in the state dir. FNV is inlined: std's `DefaultHasher`
  is not stable across Rust versions and the port must be.
- State dir: `$SELX_STATE_DIR/<hash>` (override, used by tests and the
  Docker harness) → `$XDG_RUNTIME_DIR/selx/<hash>` → the per-user cache
  dir (`~/Library/Caches/selx/run/<hash>` on macOS, `~/.cache/selx/run/<hash>`
  on Linux; the no-`$XDG_RUNTIME_DIR` case covers macOS and some WSL
  setups). The chain is a pure `state::base_dir_from(state_override,
  runtime_dir)`, so it is unit-tested on every platform. Mode 0700; every
  file in it 0600. Contents: `root`, `pid`, `port`, `ingest.sock`,
  `ensure.lock`, `events.log`, `daemon.log`.
- The daemon binds the TCP port on 127.0.0.1 before anything else and hands
  the listener to the adapter factory (`daemon::BindInfo`); the
  `ClaudeIdeAdapter` takes it for its WebSocket server. If no adapter takes
  it, the daemon holds it so the recorded port stays reserved.

## Daemon lifecycle

- `selx ensure` takes `flock(ensure.lock)`, pings the socket, and returns
  if a daemon for **this root** answers. Otherwise it cleans stale state
  (pid/port/socket; a live-but-unresponsive pid is given 2 s then
  terminated), spawns `selx daemon` detached (`setsid`, stdio → `daemon.log`),
  and waits for a healthy ping. Losers of the lock race block, then see the
  winner's daemon. The spawned `Child` is polled with `try_wait` so a daemon
  that dies at startup is detected immediately (an unreaped zombie still
  answers `kill(pid, 0)`).
- Higher layers can hook stale cleanup (`on_stale`) and daemon exit
  (`daemon::at_exit`) — this is how the Claude IDE adapter removes
  `~/.claude/ide/<port>.lock` without core knowing it exists.
- Exit paths: idle timeout (default 4 h, `SELX_IDLE_TIMEOUT` / `--idle-timeout`,
  0 disables; only fires with zero connected agent clients), SIGTERM/SIGINT/
  SIGHUP, `selx stop` (`--all` stops every daemon and removes each state
  dir), and panics (hook runs the at-exit cleanups then exits
  101 — a daemon that panicked is not trusted to keep running). `ensure`
  restarts it on the next send.
- Activity that resets the idle clock: `send` requests. `ping`/`status`
  polling does not keep a daemon alive.
- **Installing over a running daemon.** A daemon keeps whatever binary
  started it, so after `cargo install` a fresh `selx` would otherwise drive
  hours-old code with nothing to show for it. Every daemon reports a build
  id — the crate version plus the size and mtime of its executable, taken
  at startup, because the version alone never changes between local
  installs (`build_id::current`). `ensure` compares it with the caller's
  own. On a mismatch it replaces the daemon if it has no connected agents,
  and otherwise leaves it running and warns (replacing it would drop a
  live Claude session); `selx status` reports the mismatch without acting
  on it.
  A build neither side can identify is never called stale: silently
  restarting a healthy daemon on every command is worse than missing an
  upgrade. The whole decision is `ensure::judge`, a pure function of
  "does the build differ" and "how many clients did it report", both from
  the one `ping`.

## Ingestion protocol (private)

NDJSON over `ingest.sock`. Requests: `ping`, `status`, `send {event}`,
`registerorigin {origin}`, `follow` (streams frames after an `{"ok":true}`
line), `stop`. Responses carry `ok`; errors are `{"ok":false,"error":…}`.
The daemon rejects events whose `workspace_root` is not its own. `selx` is
the only client; this may change freely.

- `ping` carries `version`, `build` and `clients` — everything `ensure`
  needs to judge an installed-over daemon, in one round trip. `clients` is
  `Option`: absent from a daemon predating the field, which counts as busy
  rather than as zero.
- `status` is `ping` plus uptime, idle, adapters and the recent list.
  `client::try_ping` / `try_status` share one probe that rejects an answer
  from a daemon serving a different root.
- A `recent` entry and a `follow` frame are the same `EventRecord`
  (`ts`, `event`, `outcome`) — the event redacted unless the daemon runs
  at `-vv`.
- Timestamps are Unix seconds in JSON and `HH:MM:SSZ` in human output (no
  timezone dependency).

## Configuration

`CLI flag > environment > [[workspace]] > top level > built-in default`,
for every setting. Only the first leg lives in `cmd/daemon.rs`, which has
the flags; the rest is `config::Resolved::apply_env`, which takes an
environment *lookup* rather than reading the process's own, so the whole
table is unit-tested without spawning a daemon or mutating global state.

A value selx cannot parse is an error naming what it accepts, never a
silent fall-through to the next layer (an unparsed `SELX_IDLE_TIMEOUT=5m`
would otherwise quietly become the default). Empty means "unset"
everywhere except `SELX_INJECT_EXCLUDE`, where it means an empty exclude
list, the only way to say "inject into Claude too".

`selx::env::VARS` is the registry of every variable selx reads, from
all three layers. The README's table and the end-to-end harness's
environment scrub both derive from it — a test asserts each name appears
in `README.md`, and `Env::cmd_bin` iterates it — because a list restated
by hand drifts from the code.

The config file, `~/.config/selx/config.toml` (path override
`SELX_CONFIG`; the empty string disables it, which the tests set so a
developer's real config never leaks in), is parsed with `basic-toml`
(serde-only, no TOML editing machinery) and `deny_unknown_fields`, so a
typo fails loudly at daemon startup with the offending key and "fix or
remove it" advice; `ensure` surfaces that via its daemon-log tail. Only
the daemon reads it (idle timeout, inject target/exclude, focus and
reload modes); `send`/`ensure`/`status` don't, so an edit takes effect on
the next daemon start (`selx stop && selx ensure`), except the reload
hook, which reads the switch on every run. `[[workspace]] root` supports
a leading `~`; roots are compared after canonicalization, matching what
root resolution produces. Config lives in the command layer — core stays
config-free.

## ClaudeIdeAdapter

Lives entirely in `adapters::claude_ide`; core knows nothing of it.
Protocol details and everything observed on the wire are in
[PROTOCOL-NOTES.md](PROTOCOL-NOTES.md). Design points:

- Lock file `<config>/ide/<port>.lock` written at adapter start (before the
  daemon answers pings, so `ensure` returning healthy implies the lock file
  exists), removed via `daemon::at_exit` on every exit path, and by
  `ensure`'s `on_stale` hook after a `kill -9` (only when the lock's pid is
  the dead daemon's, or its `ideName` is ours and the pid is dead — another
  IDE's live lock on the same port is never touched).
- WebSocket server: 127.0.0.1 only; constant-time token check; any `Origin`
  header → 403; echoes the requested subprotocol (`mcp`) — Claude drops the
  connection otherwise. The lock file's `ideName` is `"selx"`, which Claude
  displays verbatim; `SELX_IDE_NAME` overrides it for experiments.
- Ambient → `selection_changed`, explicit → `at_mentioned`; both 0-based on
  the wire, converted from selx's 1-based inclusive lines. Text missing from
  the event is read from disk so ambient context is useful.
- Events arriving with no Claude connected are `Dropped` (surfaced in the
  routing outcome and as a `selx send` warning), but still retained as the
  "latest selection" so a Claude that connects later can fetch it with
  `getLatestSelection`. There is no queue: a drop is a drop, not a failure.
- `client_count()` = connected Claude sockets; idle shutdown never fires
  while one is connected.
- `selx claude [-- args]` = `ensure` + register this pane (below) +
  `CLAUDE_CODE_SSE_PORT`/`ENABLE_IDE_INTEGRATION` + `exec claude`.
  `SELX_CLAUDE_BIN` overrides the binary (tests use `/usr/bin/env`).

### Which pane is Claude in

Claude speaks no multiplexer and the WebSocket carries no pane, so the
delivered pane is inferred from herdr (`herdr::resolve_claude_pane`, pure
and unit-tested):

- **Registered panes first.** Before `exec`, `selx claude` sends
  `Request::RegisterOrigin` over the ingestion socket with its `HERDR_*`
  coordinates; the daemon fans it to adapters through
  `Adapter::register_origin` (default no-op) and this one keeps the pane
  ids. Kind-matching alone finds panes that *look like* Claude, which is not
  the set that received anything: a bare `claude` appears in `agent.list`
  but holds no connection here, and focusing it would send the user to a
  pane that was told nothing. Registrations live in the adapter, so a daemon
  restart falls back to kind until the next `selx claude`.
- **Then by kind**, for a Claude connected some other way.
- A herdr origin confines both to its own workspace, and the ranking the
  TextInjectAdapter uses for targeting (below) picks between several.
  `blocked` is not excluded — see the focus section.
- Every step is best-effort: no herdr, an unreachable socket or no matching
  pane costs focus, never the delivery, which has already happened.
- `ClaudeIdeAdapter::deliver` fans out to *every* connected client, so with
  two Claudes in one workspace "the agent that received it" is genuinely
  plural; ranking picks one. Registration narrows the candidates without
  making the question answerable.

## TextInjectAdapter

For agents with no IDE protocol, running as TUIs in [herdr](https://herdr.dev)
panes. The adapter is `adapters::text_inject`; herdr itself — socket
client, agent model, target selection — is `adapters::herdr`, since
finding a pane is not the same job as typing into one. Core knows nothing of
either.
Details and env vars in [../integrations/herdr/README.md](../integrations/herdr/README.md).

- `supports_ambient() = false`: typed prompt text cannot carry a weak signal,
  so the router drops ambient events before they reach this adapter. This is
  a correctness rule, not an optimization.
- On an explicit event: `agent.list` over herdr's socket, choose a pane,
  `pane.send_text` with `Regarding <path> lines A-B (currently selected): `
  and **no newline**. The user submits.
- Target (`choose_target`): a configured `SELX_INJECT_TARGET` (name /
  pane id / kind) wins anywhere, being an explicit override. Otherwise
  locality decides, and locality is spatial: with a herdr `origin`, only
  agents in the **same herdr workspace** qualify — a hard filter — and the
  pane's cwd is not consulted at all, so an agent that has `cd`'d out of the
  repo stays reachable. Without a usable origin the older filesystem rule
  applies instead (cwd under the workspace root). Candidates rank on
  `(same tab as the sender, state_change_seq, revision, focused)`,
  descending: the agent sharing the editor's tab beats a busier one
  elsewhere, and recency only breaks ties within a tab. `claude` excluded by
  default (own adapter); `blocked` agents skipped — typing into an approval
  dialog corrupts it. `choose_target` is pure and unit-tested; the herdr wire
  path is covered by integration tests against a real herdr server in an
  isolated named session (skipped when `herdr` is absent, e.g. CI).
- herdr's API is newline-delimited JSON, one response per connection
  (`{"method","params"}` to `{"result"}` or `{"error"}`); the `HerdrClient`
  opens a fresh connection per call.
- One discovery path: `HerdrClient::discover(override)` resolves the socket
  (explicit, else `default_socket()`) and answers `None` when herdr is not
  running. Every caller — text-inject, claude-ide's pane lookup, the
  focuser — goes through it, and each treats `None` as "nothing to deliver
  to" or "no focus to move", never as a failure. It resolves per call
  rather than once at startup so a herdr started *after* the daemon is
  still found.
- **Not cached.** An explicit event costs two `agent.list` round trips
  (text-inject choosing a target, claude-ide naming Claude's pane) plus a
  third from the focuser when more than one pane was reached. Caching them
  per event would mean the adapters sharing state that core deliberately
  cannot see, to save three local socket calls on a human keypress. The
  decision is to pay it.

## Focus

`Delivery::Delivered` carries an optional pane id, opaque to core;
`RouteOutcome` reports the panes reached, which of the deliveries told an
actual agent (`Adapter::is_agent()` — the event log says `false`), and the
pane focus ended on. After `Router::route` the daemon makes at most one
call through a `Focuser` it was handed by the adapter factory
(`BindInfo::focuser`); the herdr implementation is
`adapters::herdr::PaneFocuser`. Focusing from inside each adapter was
rejected: two adapters could then race to focus different panes on one
event.

Policy lives in `adapter::should_follow` — a pane was reached, the mode
allows it (`FocusMode`: `delivered` default / `explicit` / `off`, via
`SELX_FOCUS` or `[focus] mode`), and **the event carries an origin**. A
sender with no seat in a multiplexer (a plain terminal, a script) has none
to be moved from, so moving someone else's would be a surprise; the herdr
focuser narrows that further to origins that are herdr's own. Targeting
keeps its origin-less fallback — the agent is still told, the screen is just
not taken.

Which pane is `herdr::choose_focus`, pure and unit-tested: never the sending
pane, otherwise the best-ranked pane by the same key targeting uses.
`blocked` agents are excluded from injection but kept for focus — a
permission prompt is where the user wants to be.

`selx send` warns "no agent received …" for **both** kinds now: with focus
following delivery, a dropped ambient send is otherwise silent, and the
warning is what explains focus not moving.

## Editor reload

The other direction: Claude edits a file, and the Helix showing it re-reads
it. Helix has no file watcher, so `selx reload` types `:reload-all` into
the editor's pane, found and typed into exactly as `selx open` does
(`open::find_editor_pane`, `open::type_commands`). A CLI command, not a
daemon feature: it needs no state beyond one marker file, and the daemon
knows no editor.

The trigger is Claude Code's hooks, which `selx claude` installs by passing
`--settings` (merged with the user's own settings, not replacing them):
`PostToolUse` on `Edit|Write|NotebookEdit`, and `Stop`. Both run
`selx reload --claude-hook --root <root>`, which reads the payload on stdin
and always exits 0 — a failing hook is reported on every edit. Only when
launched from a herdr pane, only without a `--settings` of the user's own,
and only when `SELX_RELOAD` / `[reload] mode` is not `off`; the hook reads
the switch again on every run, so the config file quiets a running Claude.

`:reload-all` discards unsaved changes, and Helix will not say which
buffers have any, so the pane's screen is read first (`pane.read`) and
`reload::helix_verdict` — pure, tested against captured screens — reloads
only on proof: a statusline, a bufferline (told from file text by the line
numbers under it) naming the statusline's file, too short to be
truncated, with no `[+]` anywhere; and, if the pane has the keyboard
(`focused` in `pane.list`), normal mode with no message or popup up. It
therefore needs `bufferline = "always"`; without one it never reloads.

A reload the screen held off is owed: `reload-pending` in the state dir,
paid by the next edit's hook or the turn's `Stop`. No editor pane at all
owes nothing. Reloads for one root are serialized by `reload.lock`, since
Claude's parallel edits fire parallel hooks, and interleaved keystrokes
would be typed into Helix as garbage.

The facts this rests on, verified live and worth re-checking when Claude
Code, Helix or herdr is upgraded: Claude's `PostToolUse` hooks receive the
edited file's absolute path as JSON on stdin and `Stop` fires when a
response ends; a `--settings` passed on the command line is merged with
the user's settings files, not substituted for them; Helix's `:reload` and
`:reload-all` both discard unsaved changes without asking, nothing in its
command line can name a buffer or report one modified, and its bufferline
truncates rather than scrolls, so a clipped label may have lost its `[+]`;
herdr's `pane.read` returns the pane's visible screen as text and
`pane.list` says which pane has the keyboard.

## A closed pipe is not a panic

`selx status | head` closes stdout under the CLI. Rust ignores `SIGPIPE`,
so the write returns `EPIPE` — which `println!` can only report by
panicking, and it printed a panic message where every other Unix tool
exits quietly.

Restoring the default disposition (dying on the signal, as `head` expects)
fixes the printing and breaks something else: the CLI also writes to the
daemon's socket, where a broken pipe is an error the code recovers from —
`try_ping` reads one as "this daemon is stale, restart it". A signal
disposition is process-wide and cannot tell the two writes apart, so
`SIGPIPE` stays ignored and the `EPIPE` is handled where the printing
happens: `crate::outln!` (`src/out.rs`) writes to a locked stdout
and, on a broken pipe, exits 141 — the status a program killed by signal
13 reports, so pipelines see the usual thing. `clippy.toml` bans
`println!` workspace-wide so a new call site cannot quietly reintroduce
the panic.

## Security decisions

- Selected text is never written to `events.log`, `daemon.log`, `status`
  output or `follow` frames unless the daemon runs at `-vv`. The `text`
  field is stripped before an event reaches any of those.
- Unix socket paths are length-limited (~104 bytes). `StateDir::create`
  fails early with advice (`set SELX_STATE_DIR`) rather than letting the
  daemon die at bind time.
- One private write, `core::paths::write_private`: a pid-suffixed
  temporary opened `create_new` at 0600, fsynced, renamed into place. The
  state files and the Claude lock file — which carries the auth token —
  both go through it, so a reader sees the whole file or the old one and
  never a partial, and two writers cannot share a temporary. `home_dir()`
  lives beside it: the one `BaseDirs`-then-`$HOME` fallback that every
  path under the home directory goes through.

## Future adapters

ACP is the emerging editor <-> agent protocol (Zed, JetBrains, ...). The
core event model deliberately uses ACP-compatible vocabulary so that an ACP
adapter is a protocol swap rather than a redesign: `Explicit` events map to
user-initiated context mentions, `Ambient` to editor-state updates. Out of
scope for v0; there is no stub module for it, because an empty file is not
a design.

## Verified editor facts

What the shipped snippets rest on, checked live against the versions
named and worth re-checking when either editor is upgraded:

- Helix 25.07.1, via a pty harness: `%{buffer_name}` is the cwd-relative
  path; `%{selection_line_start}` / `%{selection_line_end}` are **1-based
  inclusive** — selx's own convention, no conversion. They work in normal
  mode too (Helix always has a selection), so the shipped snippet uses the
  selection pair everywhere instead of `%{cursor_line}`, and a multi-line
  `x` selection sends its full span even from normal mode.
- lazygit 0.64.1: `editAtLine` override + `t`/`T` custom commands,
  exercised end-to-end by `scripts/editor-smoke.py` (real hx + real
  lazygit under ptys firing the shipped snippets at a real daemon).
  Defaults are *ambient*, matching VS Code's selection sharing; `T` is the
  explicit file-send.
- `t`/`T` is one convention across both editors because both keys are
  free: in Helix 25.07.1's space mode (which-key popup under a pty) and in
  lazygit 0.64.1's files / commit-files panels (`lazygit -c` default
  keybinding dump; `t`/`T` mean revert/tag only in the commits panel,
  never bound here). `<space>c`/`C` in Helix shadow the comment togglers
  and `C` in lazygit shadows commit-with-editor, so neither is used.
  Multi-line diff selections are impossible upstream.

## Docker dogfood harness

`docker/` builds an amd64 container with Helix, lazygit, Claude Code and
herdr wired up with the shipped snippets, for end-to-end testing that
never touches the host environment. The Claude Code binary is staged
host-side by `build.sh` (a copy of the host install, else a resumable
download) rather than installed inside the Dockerfile: a one-shot
~240 MB in-container download cannot resume, and `RUN` layers cannot be
retried half way. `DISABLE_AUTOUPDATER=1` keeps the image deterministic.
