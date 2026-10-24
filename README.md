# selx

[![CI](https://github.com/hungryhorace/selx/actions/workflows/ci.yml/badge.svg)](https://github.com/hungryhorace/selx/actions/workflows/ci.yml)

Editor selection bridge for terminal AI agents. Select lines in Helix or
lazygit, press a key, and the agent running in another pane knows which
lines in which file you mean.

```
$ selx send --file src/main.rs --start 40 --end 62 --explicit --source helix
```

In the pane running `selx claude`, Claude's prompt is now waiting with the
reference typed in:

```
> @src/main.rs#L40-62
```

Selections in Helix and lazygit reach Claude Code as IDE context through
the Claude Code IDE protocol: quietly, as the `⧉ Selected 23 lines from
main.rs` context Claude may or may not use (ambient), or as an
`@file#L40-62` mention in the next prompt (explicit). Running Claude as
`selx claude` instead of `claude` is the whole setup, and needs nothing
else. With [herdr](https://herdr.dev), a terminal workspace manager for AI
coding agents, the same keys also type a reference into any other agent's
pane (Codex, Gemini CLI, opencode, …), focus follows the send to the pane
that received it, lazygit's `e` opens the file in the Helix already
running in your workspace, and Claude's edits reload Helix. Only that
second group needs herdr. selx runs on macOS (Apple Silicon) and Linux
(amd64 and aarch64), from a release binary or a Rust toolchain.

selx is not affiliated with Anthropic, Helix, lazygit or herdr. The Claude
Code IDE protocol it speaks is observed, not documented
([docs/PROTOCOL-NOTES.md](docs/PROTOCOL-NOTES.md)), and can change in any
Claude Code release; `selx status` is where breakage shows.
[docs/DESIGN.md](docs/DESIGN.md) is what the code does, and
[integrations/herdr/README.md](integrations/herdr/README.md) is the
reference for which pane a send reaches and where focus goes.

## Installing

Either:

- With a Rust toolchain: `cargo install selx`, into `~/.cargo/bin`.
- Or download the tarball for your platform from the
  [Releases](https://github.com/hungryhorace/selx/releases) page
  (`aarch64-apple-darwin` for an Apple Silicon Mac,
  `x86_64-unknown-linux-gnu` or `aarch64-unknown-linux-gnu` for Linux),
  check it against the `.sha256` beside it, and put `selx` on your `PATH`:

  ```sh
  curl -LO https://github.com/hungryhorace/selx/releases/download/vX.Y.Z/selx-vX.Y.Z-aarch64-apple-darwin.tar.gz
  tar xzf selx-vX.Y.Z-aarch64-apple-darwin.tar.gz
  install selx-vX.Y.Z-aarch64-apple-darwin/selx ~/.local/bin/
  ```

  On a Mac, download with `curl` as above rather than a browser: Gatekeeper
  does not quarantine a command-line binary fetched that way. For one a
  browser saved, `xattr -d com.apple.quarantine selx` clears the flag.
- Or, for an unreleased `main`: `cargo install --git
  https://github.com/hungryhorace/selx selx`, or `just install` from a
  checkout.

## Build

For working on selx. The [justfile](justfile) is the single definition of
the build and the checks, and CI only calls it; get `just` with `cargo
install just`.

```sh
just build      # release build → target/release/selx
just test       # exactly what CI runs: fmt --check, clippy -D warnings, tests, cargo-deny licences
just audit      # security advisories in the dependency tree (needs network)
just install    # cargo install into ~/.cargo/bin
```

(Or the underlying `cargo build --release` / `cargo test` directly.)

## Quickstart

1. Hook up your editors — both snippets are shipped verbatim and verified
   against real versions (Helix 25.07.1, lazygit 0.64.1):
   - append [integrations/helix/config.toml.snippet](integrations/helix/config.toml.snippet)
     to `~/.config/helix/config.toml` → `<space>t` shares the selected
     lines quietly (ambient), `<space>T` sends them as an explicit mention;
   - merge [integrations/lazygit/config.yml.snippet](integrations/lazygit/config.yml.snippet)
     into `~/.config/lazygit/config.yml` → `t` shares the selected file and
     `e` in the staging view shares the diff line under the cursor, both as
     ambient context (VS Code-style); `T` sends the selected file
     explicitly. One convention everywhere: `t` tells the agent quietly,
     shift it to `T` when you mean it loudly — and both keys are unbound
     in these panels by default. `e` in the *files* panel still opens the
     editor, but through `selx open`: the file becomes a buffer in the
     Helix already running in your herdr workspace, instead of a second
     Helix stacked on top of lazygit.
2. Start your agent:
   - Claude Code: run it as `selx claude` (instead of `claude`) anywhere in
     the workspace — the status line shows the IDE connection, and in herdr
     the wrapper also tells selx which pane it is in, so a send moves you
     there;
   - anything else in a [herdr](https://herdr.dev) pane: nothing to do;
     explicit sends are typed into an agent pane in the same herdr workspace,
     preferring the one sharing a tab with your editor (never submitted —
     you press enter).

Select lines in Helix, hit `<space>t`, and ask the agent about them.

No editor config needed to test the pipeline:

```sh
selx claude                 # terminal 1: Claude Code, connected to this workspace's daemon
selx send --file src/main.rs --start 40 --end 62 --explicit --source helix   # terminal 2
```

`selx status --follow` shows every event and which adapters received it;
`selx stop` shuts the daemon down (it idles out after 4 h anyway).

IDE integration is interactive-only in Claude Code: `claude -p` never
connects.

The workspace is the git worktree root of the current directory (or the
directory itself outside a repo); `--root PATH` overrides. One daemon per
workspace; `ensure` is idempotent and race-safe, and recovers from a
`kill -9`'d daemon. State lives in `$XDG_RUNTIME_DIR/selx/<hash>/`
(fallback `~/.cache/selx/run/`), mode 0700.

Selected text is never logged unless the daemon runs with `-vv`.

## Commands

| Command | Behaviour |
|---|---|
| `selx ensure [--root P] [--json]` | Start the daemon if needed; print port. |
| `selx send --file F --start N [--end N] [--explicit] [--source S] [--text-stdin] [--json]` | Forward a selection (starts the daemon if needed). Without `--explicit` the event is *ambient*. |
| `selx say TEXT [--root P] [--source S] [--json]` | Put a sentence of the sender's own in the agent's prompt, for a selection that is not lines of a file (a rectangle of a map). Always explicit; typed into the agent's pane through herdr, Claude's included. The daemon keeps it to one line of at most 400 characters with no control characters, so it can never press Enter. |
| `selx claude [--root P] [-- CLAUDE_ARGS…]` | `ensure`, `claude update`, export `CLAUDE_CODE_SSE_PORT`, `exec claude`. Claude is brought up to date before the session starts and its own background updater is disabled (`DISABLE_AUTOUPDATER=1`), so it is never updated underneath a running session. In a herdr pane it also passes `--settings` with hooks that reload your editor after Claude edits a file (see [Editor follows the agent](#editor-follows-the-agent)). |
| `selx open [--editor BIN] FILE[:LINE[:COL]]…` | Open files in the editor already running in this herdr workspace (Helix today), moving focus to it. Runs the editor itself when it cannot: no editor pane, no herdr, or arguments only a real editor process can honour. No daemon involved. |
| `selx reload [--editor BIN] [--root P] [--claude-hook]` | Make the editor running in this herdr workspace re-read its files from disk, unless its screen shows a buffer with unsaved changes or you typing in it. Exit 1 with the reason when it will not. `--claude-hook` is the form `selx claude` installs: reads the hook's JSON on stdin, always exits 0. No daemon involved. |
| `selx status [--root P] [--json] [--follow]` | Health, port, adapters, recent events; `--follow` streams. Exit 2 if not running. |
| `selx stop [--root P] [--all]` | Graceful shutdown + cleanup. |
| `selx daemon --root P [--port N] [--foreground] [--idle-timeout S]` | Internal; run by hand with `--foreground` for debugging. |

## Environment

Every variable selx reads. A value it cannot parse fails at startup and
says what it accepts, rather than falling through to the config file. The
list is kept in step with the code by a test (`selx::env::VARS`).

| Variable | Effect |
|---|---|
| `SELX_CONFIG` | Config file path; empty disables the config file. |
| `XDG_CONFIG_HOME` | Base for the default config file location (`$XDG_CONFIG_HOME/selx/config.toml`, else `~/.config/selx/config.toml`). |
| `SELX_IDLE_TIMEOUT` | Daemon idle shutdown, in whole seconds; 0 disables. |
| `SELX_FOCUS` | When a delivery moves focus: `delivered`, `explicit` or `off`. |
| `SELX_RELOAD` | Whether Claude's edits reload the editor: `on` (default) or `off`. |
| `SELX_INJECT_TARGET` | Pin text injection to an agent name, pane id (`w1:p2`) or kind. |
| `SELX_INJECT_EXCLUDE` | Agent kinds never injected into, comma-separated. Empty means exclude nothing. |
| `SELX_IDE_NAME` | `ideName` written to the Claude lock file (default `selx`). |
| `SELX_CLAUDE_BIN` | Path to the `claude` binary. |
| `SELX_CLAUDE_UPDATE` | `0` skips the `claude update` that `selx claude` runs before starting Claude. |
| `SELX_LOG` | `tracing` filter for `-v` diagnostics. |
| `SELX_STATE_DIR` | Base directory for per-workspace runtime state. |
| `CLAUDE_CONFIG_DIR` | Where the Claude IDE lock file lives (default `~/.claude`). |
| `HERDR_SOCKET_PATH` | herdr's API socket. |
| `HERDR_CONFIG_PATH` | herdr's config file, whose directory holds session sockets. |
| `HERDR_SESSION` | herdr session; also names the session on the event origin. |
| `HERDR_WORKSPACE_ID` | The sending pane's herdr workspace. |
| `HERDR_TAB_ID` | The sending pane's herdr tab. |
| `HERDR_PANE_ID` | The sending pane. |

The four `HERDR_*` coordinate variables are set by herdr inside its own
panes; `selx send` reads them to record where a keypress happened. Nothing
else needs setting by hand.

## Config file (optional)

`~/.config/selx/config.toml` — selx runs fine without it. Environment
variables beat the file; a matching `[[workspace]]` section beats the top
level.

```toml
idle_timeout_secs = 14400      # daemon idle shutdown; 0 disables

[inject]                       # herdr text-injection defaults
target = "codex"               # agent name, pane id ("w1:p2") or kind
exclude = ["claude"]           # agent kinds never targeted (default)

[focus]                        # follow the send into the pane that got it
mode = "delivered"             # "delivered" (default) | "explicit" | "off"

[reload]                       # Claude's edits reload the editor
mode = "on"                    # "on" (default) | "off"

[[workspace]]                  # per-workspace overrides; first match wins
root = "~/Projects/foo"
idle_timeout_secs = 0
inject = { target = "gemini" }
focus = { mode = "off" }
reload = { mode = "off" }
```

For herdr, explicit selections are typed into the chosen agent pane's input
(never submitted); ambient selections are dropped. Configure with
`SELX_INJECT_TARGET` and `SELX_INJECT_EXCLUDE` (see
[integrations/herdr/README.md](integrations/herdr/README.md)).

When a send reaches an agent's pane, herdr's focus follows it there, so you
can finish the thought where the context landed. A send nothing received
never moves you — selx warns instead. `SELX_FOCUS=off` (or `[focus] mode`)
turns it off.

## Editor follows the agent

Helix has no file watcher: when Claude edits a file you have open, the
buffer keeps the old text until you `:reload`. Run Claude as `selx claude`
in a herdr pane and selx does it for you — after each edit, it types
`:reload-all` into the Helix in your workspace, without moving focus.

`:reload-all` throws away unsaved changes, so selx first reads Helix's
screen and only reloads when it can see that **every** buffer is clean.
It needs `bufferline = "always"` under `[editor]` in your Helix config:
the bufferline is where Helix marks each modified buffer `[+]`. It holds
off when:

- any buffer is marked `[+]`, or the bufferline is too long to see whole;
- there is no bufferline (or no line numbers) to check;
- your keyboard is in Helix and you are in insert or select mode, or have
  a picker, prompt or message up.

A reload it held off is retried after Claude's next edit and at the end of
Claude's turn. `selx reload` does the same by hand (from a keybinding, or
after a `git checkout` in lazygit) and says why when it will not. Should
anything ever slip through, the reload is in the buffer's undo history:
`u` brings your text back.

Only Claude's own file edits trigger it, not its shell commands. Switch it
off with `SELX_RELOAD=off` or `[reload] mode = "off"` — the config file
also quiets a Claude that is already running. Passing your own
`--settings` to `selx claude` also leaves the hooks out.

## Contributing

Pull requests are welcome. `just test` is what CI runs, so run it before
opening one. The unit and integration tests cover the daemon, the routing
and the screen checks against captured screens; the parts that need real
Helix, lazygit, Claude Code and herdr are checked by hand, two ways.

For fully isolated end-to-end dogfooding (helix + lazygit + Claude Code +
herdr in one container, nothing touching the host environment), see
[docker/README.md](docker/README.md).

Manual checks against real tools live in `scripts/`:
`claude-interactive-smoke.py` drives an interactive Claude session under a
pty; `editor-smoke.py` drives real Helix and lazygit sessions with the
shipped snippets and asserts the daemon received the right events;
`reload-smoke.py` drives a real Helix in herdr through every rule of the
editor reload.

## Licence

Apache-2.0; see [LICENSE](LICENSE).

