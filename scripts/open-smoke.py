#!/usr/bin/env python3
"""Dogfood smoke test: lazygit's 'e' opens the file in the Helix that is
already running, instead of stacking a second Helix on top of lazygit.

Drives real lazygit and real Helix in real herdr panes - herdr supplies the
terminals, so unlike the other smoke scripts this one needs no pty - using
the shipped integrations/lazygit snippet, whose `e` runs `selx open`. Helix
and lazygit sit in *separate tabs* of one workspace, the arrangement
`selx open`'s targeting rule is written for. The unit tests cover which
pane it picks; what needs a real terminal is whether the keystrokes it
sends actually land in Helix as a command rather than as text in a buffer.

Both legs matter: with a Helix pane the file must arrive as a buffer there
and focus must follow, and with no Helix pane at all `selx open` must fall
back to a plain `hx`. The fallback is asserted on the pane's foreground
process, not on what the pane says - lazygit's own diff panel shows the
file's contents, so matching text there passes whether or not an editor
ever ran.

Everything happens in a throwaway herdr session (HERDR_SESSION), which is
stopped and deleted on the way out however the script exits; it never
touches the session you are working in.

Usage: python3 scripts/open-smoke.py [--keep]
Needs: herdr, hx, lazygit and git on PATH; builds selx with cargo if
SELX_BIN is unset. No daemon is started: `selx open` never talks to one.
"""

import json
import os
import shutil
import subprocess
import sys
import tempfile
import time

from _tui import REPO, build_selx, require

SESSION = "selx-open-smoke-%d" % os.getpid()
ANCHOR = "AAA anchor buffer"
TARGET = "ZZZ target file contents"


def herdr(*args, check=True):
    """A herdr CLI call against the throwaway session; parsed if it is JSON."""
    out = subprocess.run(["herdr"] + list(args),
                         env=dict(os.environ, HERDR_SESSION=SESSION),
                         capture_output=True, text=True)
    if check and out.returncode != 0:
        sys.exit("FAIL: herdr %s: %s" % (" ".join(args), out.stderr.strip()))
    try:
        return json.loads(out.stdout)
    except json.JSONDecodeError:
        return out.stdout


def pane_text(pane):
    return herdr("pane", "read", pane, "--source", "visible", check=False)


def pane_procs(pane):
    """The names of the pane's foreground processes ('hx', 'lazygit', ...)."""
    info = herdr("pane", "process-info", "--pane", pane, check=False)
    if not isinstance(info, dict):
        return []
    procs = info["result"]["process_info"].get("foreground_processes") or []
    return [p.get("name") for p in procs]


def wait_until(predicate, timeout, what):
    """Poll `predicate` until it is true; return False on timeout."""
    end = time.time() + timeout
    while time.time() < end:
        if predicate():
            return True
        time.sleep(0.25)
    print("  (timed out waiting for %s)" % what)
    return False


def fail(message, *panes):
    """Bail out showing what each named pane had on screen."""
    for label, pane in panes:
        print("--- %s (%s) ---\n%s" % (label, pane, pane_text(pane)))
    sys.exit("FAIL: " + message)


def start_server(log):
    """A detached herdr server for SESSION, up and answering."""
    with open(log, "w") as f:
        subprocess.Popen(["herdr", "server"], stdout=f, stderr=f,
                         start_new_session=True,
                         env=dict(os.environ, HERDR_SESSION=SESSION))
    if not wait_until(
            lambda: subprocess.run(
                ["herdr", "pane", "list"],
                env=dict(os.environ, HERDR_SESSION=SESSION),
                capture_output=True).returncode == 0,
            30, "the herdr server"):
        sys.exit("FAIL: herdr server never came up; log:\n%s"
                 % open(log).read())


def stop_server():
    """Leave nothing running: this session, and only this session."""
    herdr("server", "stop", check=False)
    time.sleep(1)
    herdr("session", "delete", SESSION, check=False)


def make_repo(ws):
    """A repo with one committed file and one modified file, so lazygit's
    files panel opens with the modified file as its selected row."""
    def git(*args):
        subprocess.run(["git", "-c", "user.email=smoke@selx", "-c",
                        "user.name=smoke"] + list(args), cwd=ws, check=True,
                       capture_output=True)
    git("init", "-q")
    for name, body in (("anchor.txt", ANCHOR), ("target.txt", TARGET)):
        with open(os.path.join(ws, name), "w") as f:
            f.write(body + "\n")
    git("add", "-A")
    git("commit", "-qm", "init")
    with open(os.path.join(ws, "target.txt"), "a") as f:
        f.write("a modification, so the file is listed\n")


def quit_helix(pane):
    """`:quit!` typed into Helix - `selx open`'s own mechanism, in reverse."""
    herdr("pane", "send-keys", pane, "esc")
    herdr("pane", "send-keys", pane, ":")
    herdr("pane", "send-text", pane, "quit!")
    herdr("pane", "send-keys", pane, "enter")


def main():
    keep = "--keep" in sys.argv
    require("herdr", "hx", "lazygit", "git")
    selx = build_selx()

    work = tempfile.mkdtemp(prefix="selx-open-smoke-")
    try:
        run(work, selx)
    finally:
        stop_server()
        if keep:
            print("kept:", work)
        else:
            shutil.rmtree(work, ignore_errors=True)


def run(work, selx):
    ws = os.path.join(work, "ws")
    os.makedirs(ws)
    make_repo(ws)

    # The `selx` under test, ahead of any installed one on PATH: what the
    # snippet's `e` runs has to be this build, not the developer's
    # ~/.cargo/bin copy of an older one.
    bindir = os.path.dirname(selx)
    lg_cfg = os.path.join(work, "lazygit.yml")
    shutil.copy(os.path.join(REPO, "integrations", "lazygit",
                             "config.yml.snippet"), lg_cfg)

    start_server(os.path.join(work, "herdr.log"))

    # Helix and lazygit in two tabs of one workspace: `selx open` prefers a
    # Helix in the caller's tab, and has to find this one without that.
    created = herdr("workspace", "create", "--cwd", ws)["result"]
    hx_pane = created["root_pane"]["pane_id"]
    workspace = created["workspace"]["workspace_id"]
    lg_pane = herdr("tab", "create", "--workspace", workspace,
                    "--cwd", ws)["result"]["root_pane"]["pane_id"]

    herdr("pane", "run", hx_pane, "cd %s && hx anchor.txt" % ws)
    if not wait_until(lambda: ANCHOR in pane_text(hx_pane), 30, "Helix"):
        fail("Helix never started", ("helix", hx_pane))

    # CONFIG_DIR keeps lazygit's own state out of ~/.config; a fresh one
    # means the first-run popup is there to dismiss.
    herdr("pane", "run", lg_pane,
          "cd %s && PATH=%s:$PATH CONFIG_DIR=%s lazygit --use-config-file %s"
          % (ws, bindir, work, lg_cfg))
    if not wait_until(lambda: "target.txt" in pane_text(lg_pane), 30,
                      "lazygit"):
        fail("lazygit never started (does it still accept the snippet?)",
             ("lazygit", lg_pane))
    herdr("pane", "send-keys", lg_pane, "enter")
    time.sleep(1.0)

    # --- with a Helix running: the file lands there ------------------------
    herdr("pane", "send-keys", lg_pane, "e")
    if not wait_until(lambda: TARGET in pane_text(hx_pane), 15,
                      "target.txt in Helix"):
        fail("'e' did not open target.txt in the running Helix",
             ("helix", hx_pane), ("lazygit", lg_pane))
    print("PASS lazygit: 'e' opened target.txt in the Helix one tab over")

    if not wait_until(
            lambda: herdr("pane", "get", hx_pane)["result"]["pane"]["focused"],
            5, "focus"):
        fail("focus did not follow the file to the Helix pane",
             ("helix", hx_pane))
    print("PASS focus: the Helix pane took focus")

    # The point of the exercise: lazygit is still lazygit, with no second
    # Helix stacked on top of it.
    if "hx" in pane_procs(lg_pane) or "lazygit" not in pane_procs(lg_pane):
        fail("lazygit's pane is running %r, not just lazygit"
             % (pane_procs(lg_pane),), ("lazygit", lg_pane))
    print("PASS lazygit: no second Helix over lazygit (pane runs %s)"
          % ", ".join(pane_procs(lg_pane)))

    # --- with no Helix running: a plain hx, as before ----------------------
    quit_helix(hx_pane)
    if not wait_until(lambda: "hx" not in pane_procs(hx_pane), 15,
                      "Helix to exit"):
        fail("Helix would not quit", ("helix", hx_pane))
    herdr("pane", "send-keys", lg_pane, "e")
    if not wait_until(lambda: "hx" in pane_procs(lg_pane), 20,
                      "the fallback editor"):
        fail("no Helix pane and no fallback `hx` either: 'e' did nothing",
             ("lazygit", lg_pane))
    print("PASS fallback: with no Helix pane, 'e' ran a plain hx in place")

    print("OK")


if __name__ == "__main__":
    main()
