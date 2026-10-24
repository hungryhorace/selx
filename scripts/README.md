Manual test scripts for the parts that need real external tools.

- `claude-interactive-smoke.py` — drives a real interactive Claude Code
  session under a pty (`claude -p` never connects to an IDE), accepts the
  trust prompt, waits for the daemon to report the connection, then checks
  explicit (`--ambient` for ambient) selections reach Claude.
- `editor-smoke.py` — drives real Helix and lazygit sessions under ptys
  using the shipped `integrations/` snippets verbatim, and asserts the
  daemon received the right events (source, file, line span). Skips if
  `hx`/`lazygit` are not on PATH.
- `open-smoke.py` — drives real lazygit and real Helix in real herdr panes
  (herdr supplies the terminals, so this one needs no pty) and checks that
  lazygit's `e` hands the file to the Helix already running one tab over,
  that focus follows it, that no second Helix ends up stacked on lazygit,
  and that with no Helix pane at all `selx open` still falls back to a
  plain `hx`. `cmd::open`'s unit tests cover which pane it picks; this
  covers whether the keystrokes land in Helix as a command. Runs in a
  throwaway `HERDR_SESSION` that is stopped and deleted however the script
  exits; it never touches the session you are working in. Needs herdr;
  starts no daemon.
- `reload-smoke.py` — drives a real Helix in a real herdr pane and runs
  `selx reload --claude-hook` the way Claude Code does, with each leg
  against a fresh edit on disk: the buffer follows the disk with focus in
  Claude's pane and with focus in Helix; an unsaved change holds the
  reload off and survives, and Claude's turn end pays the owed reload once
  it is undone; insert mode holds it off until left; `SELX_RELOAD=off`
  does nothing. `cmd::reload`'s unit tests cover how a captured screen is
  judged; this covers whether real screens read that way and whether the
  `:reload-all` lands as a command. Same throwaway-session isolation as
  `open-smoke.py`, plus a Helix config and selx state dir of its own.
- `_tui.py` — not a script. The pty scaffolding the two pty-driven
  scripts need: fork a
  pty at a window size a curses program will render into, pump the master
  end, strip escape codes before matching on the text, and locate the selx
  binary. There were two copies of it, in two styles.

All four build/use the debug `selx` (override with `SELX_BIN`) and isolate
what they touch: the two daemon-driving scripts with `SELX_STATE_DIR` and
`SELX_CONFIG=""`, `open-smoke.py` with a herdr session of its own (it starts
no daemon and reads no config), `reload-smoke.py` with all three, all of
them with throwaway repos or directories. All four exit non-zero on a failure and print `SKIP:` when the tools they drive are
not installed, so they are safe to run unconditionally.
