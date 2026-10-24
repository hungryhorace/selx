#!/usr/bin/env python3
"""Dogfood smoke test: real Helix and real lazygit fire the shipped
integration snippets at a real selx daemon.

Drives each TUI in a pty inside a throwaway git repo with an isolated
SELX_STATE_DIR, then asserts the daemon's status shows the events with the
expected source, file and line span. The daemon only runs the log adapter
path here (no Claude, no herdr) - this verifies the editor half of the
bridge, scripts/claude-interactive-smoke.py verifies the Claude half.

Usage: python3 scripts/editor-smoke.py [--keep]
Needs: hx and lazygit on PATH; builds selx with cargo if SELX_BIN is unset.
"""

import json
import os
import shutil
import subprocess
import sys
import tempfile
import time

from _tui import REPO, Tui, build_selx, require


def snippet(name):
    return os.path.join(REPO, "integrations", name)


def selx_events(selx, cwd, env):
    out = subprocess.run([selx, "status", "--json"], cwd=cwd, env=env,
                         capture_output=True, text=True)
    if out.returncode != 0:
        return []
    return json.loads(out.stdout).get("recent", [])


def expect_event(selx, cwd, env, source, file_suffix, start, end,
                 kind="explicit"):
    # start=end=None asserts a whole-file event (no "lines" key at all).
    want = None if start is None else {"start": start, "end": end}
    for _ in range(40):
        for rec in selx_events(selx, cwd, env):
            ev = rec["event"]
            if (ev["source"] == source and ev["file"].endswith(file_suffix)
                    and ev.get("lines") == want and ev["kind"] == kind):
                return ev
        time.sleep(0.25)
    sys.exit("FAIL: no %s %s event for %s:%r-%r in %r" % (
        kind, source, file_suffix, start, end, selx_events(selx, cwd, env)))


def main():
    keep = "--keep" in sys.argv
    require("hx", "lazygit", "git")
    selx = build_selx()

    work = tempfile.mkdtemp(prefix="selx-editor-smoke-")
    state = tempfile.mkdtemp(prefix="sx", dir="/tmp")
    try:
        run(selx, work, state)
    finally:
        # Stop daemons even when an assertion bailed out (their state dir
        # is about to disappear; never leave them running).
        subprocess.run([selx, "stop", "--all"], env=dict(
            os.environ, SELX_STATE_DIR=state, SELX_CONFIG=""),
            capture_output=True)
        if keep:
            print("kept:", work, state)
        else:
            shutil.rmtree(work, ignore_errors=True)
            shutil.rmtree(state, ignore_errors=True)


def run(selx, work, state):
    ws = os.path.join(work, "ws")
    os.makedirs(ws)
    env = dict(os.environ)
    env.update({
        "SELX_STATE_DIR": state,
        "SELX_CONFIG": "",  # never read the developer's real config
        "PATH": os.path.dirname(selx) + os.pathsep + env["PATH"],
    })

    # A git repo with two committed files, both modified. The second lives
    # in a subdirectory so lazygit's files panel is a *tree*: its first row
    # is the `nested` folder, not a file. That row is what the snippet used
    # to crash on, so the smoke test starts there.
    def git(*args):
        subprocess.run(["git", "-c", "user.email=smoke@selx", "-c",
                        "user.name=smoke"] + list(args), cwd=ws, check=True,
                       capture_output=True)
    git("init", "-q")
    os.makedirs(os.path.join(ws, "nested"))
    sample = os.path.join(ws, "src.txt")
    deep = os.path.join(ws, "nested", "deep.txt")
    for path in (sample, deep):
        with open(path, "w") as f:
            f.write("".join("line %d content\n" % i for i in range(1, 11)))
    git("add", "-A")
    git("commit", "-qm", "init")
    for path in (sample, deep):
        with open(path, "a") as f:
            f.write("line 11 added\n")

    # --- Helix: shipped snippet verbatim as the whole config ---------------
    hx_cfg = os.path.join(work, "hx.toml")
    shutil.copy(snippet("helix/config.toml.snippet"), hx_cfg)
    hx = Tui(["hx", "--config", hx_cfg, "src.txt"], ws, env)
    hx.drain(2.0)
    hx.send("3gg", 0.5)      # line 3
    hx.send("x", 0.3)        # select line 3
    hx.send("x", 0.3)        # extend to line 4
    hx.send(" t", 2.0)       # <space>t -> ambient share
    hx.send(" T", 2.0)       # <space>T -> selx send --explicit
    hx.send("\x1b", 0.3)
    hx.send(":q!\r", 1.0)
    hx.close()
    expect_event(selx, ws, env, "helix", "src.txt", 3, 4, kind="ambient")
    expect_event(selx, ws, env, "helix", "src.txt", 3, 4)
    print("PASS helix: <space>t/<space>T sent lines 3-4 of src.txt")

    # --- lazygit: shipped snippet verbatim as the whole config -------------
    lg_cfg = os.path.join(work, "lazygit.yml")
    shutil.copy(snippet("lazygit/config.yml.snippet"), lg_cfg)
    lg_env = dict(env)
    lg_env["CONFIG_DIR"] = work  # keep lazygit's own state out of ~/.config
    lg = Tui(["lazygit", "--use-config-file", lg_cfg], ws, lg_env)
    lg.drain(3.0)
    lg.send("\r", 1.5)       # dismiss the first-run popup (fresh CONFIG_DIR)
    # Rows: `/`, `nested`, `nested/deep.txt`, `src.txt`. The folder rows are
    # what the snippet used to crash on (no SelectedFile behind them).
    lg.send("j", 0.4)        # onto the `nested` folder row
    lg.send("t", 2.0)
    expect_event(selx, ws, env, "lazygit", "/nested", None, None,
                 kind="ambient")
    print("PASS lazygit: 't' on a folder row shared nested/ (ambient)")
    lg.send("jj", 0.5)       # past nested/deep.txt, onto src.txt
    lg.send("t", 2.0)        # ambient share of the selected (modified) file
    expect_event(selx, ws, env, "lazygit", "src.txt", None, None,
                 kind="ambient")
    print("PASS lazygit: 't' shared src.txt (ambient, whole file)")
    lg.send("T", 2.0)        # explicit send of the same file
    expect_event(selx, ws, env, "lazygit", "src.txt", None, None,
                 kind="explicit")
    print("PASS lazygit: 'T' sent src.txt (explicit, whole file)")

    lg.send("\r", 1.5)       # enter staging view for src.txt
    lg.send("\r", 1.5)       # dismiss the first-run "hunk mode" popup
    lg.send("j", 0.3)        # down one diff line
    lg.send("e", 2.5)        # editAtLine -> ambient send; snaps back
    lg.close()               # (promptToReturnFromSubprocess: false)
    ev = [r["event"] for r in selx_events(selx, ws, env)
          if r["event"]["source"] == "lazygit"
          and r["event"]["kind"] == "ambient"
          and r["event"].get("lines", {}).get("start", 0) > 1]
    if not ev:
        sys.exit("FAIL: no ambient editAtLine event past line 1: %r"
                 % selx_events(selx, ws, env))
    print("PASS lazygit: 'e' in staging shared line %d (ambient)"
          % ev[0]["lines"]["start"])

    print("OK")


if __name__ == "__main__":
    main()
