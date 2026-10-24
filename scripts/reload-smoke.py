#!/usr/bin/env python3
"""Dogfood smoke test: Claude's edits reload the Helix already running in
the workspace - and never at the cost of unsaved work, or of keystrokes
landing while you type.

Drives a real Helix in a real herdr pane and runs `selx reload
--claude-hook` the way Claude Code does, from a second pane's coordinates,
with the hook's JSON on stdin. The unit tests cover how a captured screen
is judged; what needs a real Helix is whether the screen really reads that
way, and whether the typed `:reload-all` lands as a command.

Legs, in order, each against a fresh edit on disk:

1. the user is in Claude's pane: the buffer follows the disk;
2. the user is in Helix, in normal mode: it still does;
3. a buffer has unsaved changes: nothing is typed, the change survives,
   and a reload is owed;
4. the change is undone: Claude's turn end pays the owed reload;
5. the user is in Helix in insert mode: nothing is typed until they leave
   it, and the turn end catches up;
6. SELX_RELOAD=off: the hook does nothing at all.

Everything happens in a throwaway herdr session (HERDR_SESSION), stopped
and deleted however the script exits; Helix runs with a config of its own
(`bufferline = "always"`) and selx with a state dir of its own and no
config file.

Usage: python3 scripts/reload-smoke.py [--keep]
Needs: herdr and hx on PATH; builds selx with cargo if SELX_BIN is unset.
"""

import glob
import json
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import time

from _tui import build_selx, require

SESSION = "selx-reload-smoke-%d" % os.getpid()
SOCKET = os.path.join(
    os.path.dirname(os.environ.get("HERDR_CONFIG_PATH", ""))
    or os.path.expanduser("~/.config/herdr"),
    "sessions", SESSION, "herdr.sock")


def herdr_env():
    """This process's environment, minus any herdr pane it is running in:
    every herdr call here must reach the throwaway session, never yours."""
    env = {k: v for k, v in os.environ.items() if not k.startswith("HERDR_")}
    env["HERDR_SESSION"] = SESSION
    return env


def herdr(*args, check=True):
    out = subprocess.run(["herdr"] + list(args), env=herdr_env(),
                         capture_output=True, text=True)
    if check and out.returncode != 0:
        sys.exit("FAIL: herdr %s: %s" % (" ".join(args), out.stderr.strip()))
    try:
        return json.loads(out.stdout)
    except json.JSONDecodeError:
        return out.stdout


def api(method, **params):
    """One call on the session's socket API - the same calls selx makes."""
    s = socket.socket(socket.AF_UNIX)
    s.settimeout(5)
    s.connect(SOCKET)
    s.sendall((json.dumps({"id": "smoke", "method": method,
                           "params": params}) + "\n").encode())
    buf = b""
    while not buf.endswith(b"\n"):
        chunk = s.recv(65536)
        if not chunk:
            break
        buf += chunk
    reply = json.loads(buf)
    if "error" in reply:
        sys.exit("FAIL: herdr %s: %s" % (method, reply["error"]))
    return reply["result"]


def screen(pane):
    return api("pane.read", pane_id=pane, source="visible")["read"]["text"]


def wait_until(predicate, timeout, what):
    end = time.time() + timeout
    while time.time() < end:
        if predicate():
            return True
        time.sleep(0.25)
    print("  (timed out waiting for %s)" % what)
    return False


def fail(message, pane):
    sys.exit("FAIL: %s\n--- helix ---\n%s" % (message, screen(pane)))


def start_server(log):
    with open(log, "w") as f:
        subprocess.Popen(["herdr", "server"], stdout=f, stderr=f,
                         start_new_session=True, env=herdr_env())
    if not wait_until(lambda: os.path.exists(SOCKET) and
                      herdr("pane", "list", check=False) != "", 30,
                      "the herdr server"):
        sys.exit("FAIL: herdr server never came up; log:\n%s"
                 % open(log).read())


def stop_server():
    herdr("server", "stop", check=False)
    time.sleep(1)
    herdr("session", "delete", SESSION, check=False)


def main():
    keep = "--keep" in sys.argv
    require("herdr", "hx")
    selx = build_selx()
    work = tempfile.mkdtemp(prefix="selx-reload-smoke-")
    # Short, like the e2e suite's: the state dir holds the daemon's unix
    # socket, whose path length is capped, and selx refuses a dir that
    # could not hold one - macOS's per-user temp dir is already too deep.
    state = tempfile.mkdtemp(prefix="sx", dir="/tmp")
    try:
        run(work, state, selx)
    finally:
        stop_server()
        if keep:
            print("kept:", work, state)
        else:
            shutil.rmtree(work, ignore_errors=True)
            shutil.rmtree(state, ignore_errors=True)


def run(work, state, selx):
    ws = os.path.realpath(os.path.join(work, "ws"))
    os.makedirs(ws)
    a, b = os.path.join(ws, "a.txt"), os.path.join(ws, "b.txt")
    for path, text in ((a, "alpha START"), (b, "beta")):
        with open(path, "w") as f:
            f.write(text + "\n")
    hx_config = os.path.join(work, "helix.toml")
    with open(hx_config, "w") as f:
        f.write('[editor]\nbufferline = "always"\n')

    start_server(os.path.join(work, "herdr.log"))
    created = herdr("workspace", "create", "--cwd", ws)["result"]
    hx = created["root_pane"]["pane_id"]
    tab = created["root_pane"]["tab_id"]
    workspace = created["workspace"]["workspace_id"]
    claude = herdr("pane", "split", hx, "--direction", "right",
                   "--cwd", ws)["result"]["pane"]["pane_id"]

    herdr("pane", "run", hx, "cd %s && hx -c %s a.txt b.txt"
          % (ws, hx_config))
    if not wait_until(lambda: "alpha START" in screen(hx), 30, "Helix"):
        fail("Helix never started", hx)

    def hook(payload, **env):
        """The hook, as Claude Code runs it from Claude's pane."""
        full = dict(herdr_env(), SELX_STATE_DIR=state, SELX_CONFIG="",
                    HERDR_SOCKET_PATH=SOCKET, HERDR_WORKSPACE_ID=workspace,
                    HERDR_TAB_ID=tab, HERDR_PANE_ID=claude, **env)
        out = subprocess.run([selx, "reload", "--claude-hook", "--root", ws],
                             input=json.dumps(payload), env=full,
                             capture_output=True, text=True, cwd=ws)
        if out.returncode != 0 or out.stdout or out.stderr:
            sys.exit("FAIL: the hook must exit 0 and say nothing; got %d\n"
                     "stdout: %s\nstderr: %s"
                     % (out.returncode, out.stdout, out.stderr))

    def edited(text, **env):
        """Claude writes a.txt, and its PostToolUse hook runs."""
        with open(a, "w") as f:
            f.write(text + "\n")
        hook({"hook_event_name": "PostToolUse", "tool_name": "Write",
              "tool_input": {"file_path": a, "content": text}}, **env)

    def turn_ended():
        hook({"hook_event_name": "Stop", "stop_hook_active": False})

    def owed():
        return bool(glob.glob(os.path.join(state, "*", "reload-pending")))

    def shows(text):
        return wait_until(lambda: text in screen(hx), 10, repr(text))

    def stays_without(text):
        time.sleep(2)
        return text not in screen(hx)

    def focus(pane):
        api("pane.focus", pane_id=pane)

    def keys(*names):
        for k in names:
            api("pane.send_keys", pane_id=hx, keys=[k])
        time.sleep(0.5)

    # 1 ----------------------------------------------------------------------
    focus(claude)
    edited("alpha ONE")
    if not shows("alpha ONE"):
        fail("Claude's edit did not reach Helix", hx)
    print("PASS 1: with focus in Claude's pane, the buffer followed the disk")

    # 2 ----------------------------------------------------------------------
    focus(hx)
    keys("j", "k")  # clear any message the last reload left
    edited("alpha TWO")
    if not shows("alpha TWO"):
        fail("a focused Helix in normal mode was not reloaded", hx)
    if not api("pane.get", pane_id=hx)["pane"]["focused"]:
        fail("the reload moved focus", hx)
    # And again with no key in between: a reload must not leave anything on
    # screen (a message) that holds off the next one while you read.
    edited("alpha TWO AGAIN")
    if not shows("alpha TWO AGAIN"):
        fail("a second reload in a row was held off", hx)
    print("PASS 2: with focus in Helix in normal mode, it still followed, "
          "edit after edit")

    # 3 ----------------------------------------------------------------------
    keys("g", "g", "i", "X", "esc")  # an unsaved change: "Xalpha TWO"
    if not shows("Xalpha TWO"):
        fail("could not make an unsaved change", hx)
    focus(claude)
    edited("alpha THREE")
    if not stays_without("alpha THREE") or "Xalpha TWO" not in screen(hx):
        fail("a buffer with unsaved changes was reloaded", hx)
    if not owed():
        fail("the held-off reload is not owed", hx)
    print("PASS 3: an unsaved change held the reload off, and survived")

    # 4 ----------------------------------------------------------------------
    keys("u")  # back to the saved text: nothing unsaved any more
    if not shows("alpha TWO") or "[+]" in screen(hx).splitlines()[0]:
        fail("undo did not leave the buffer clean", hx)
    turn_ended()
    if not shows("alpha THREE"):
        fail("the turn end did not pay the owed reload", hx)
    if owed():
        fail("still owed after reloading", hx)
    print("PASS 4: once clean, Claude's turn end caught up")

    # 5 ----------------------------------------------------------------------
    focus(hx)
    keys("j", "k", "i")  # typing: insert mode, nothing typed yet
    edited("alpha FIVE")
    if not stays_without("alpha FIVE"):
        fail("a reload was typed at a user in insert mode", hx)
    if " INS " not in screen(hx) or "[+]" in screen(hx):
        fail("the user's insert mode was disturbed", hx)
    keys("esc")
    turn_ended()
    if not shows("alpha FIVE"):
        fail("leaving insert mode and a turn end did not catch up", hx)
    print("PASS 5: insert mode held the reload off; the turn end caught up")

    # 6 ----------------------------------------------------------------------
    focus(claude)
    edited("alpha SIX", SELX_RELOAD="off")
    if not stays_without("alpha SIX") or owed():
        fail("SELX_RELOAD=off did not switch the reload off", hx)
    print("PASS 6: SELX_RELOAD=off did nothing")

    print("OK")


if __name__ == "__main__":
    main()
