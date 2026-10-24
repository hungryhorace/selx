#!/usr/bin/env python3
"""Drive a real interactive `claude` under a pty against a selx daemon.

`claude -p` never connects to an IDE, so the only way to test the Claude
half of the bridge for real is to run the interactive TUI: wait for the
daemon to log the connection, send an explicit selection with `selx send`,
ask Claude which lines it was pointed at, and read the answer back off the
screen. Needs Claude credentials, and costs a (small, haiku) API call.

Usage: scripts/claude-interactive-smoke.py [path/to/selx] [--ambient]
"""

import glob
import os
import re
import shutil
import subprocess
import sys
import tempfile

from _tui import Tui, build_selx, require

# What we point Claude at, and what it must say back.
SAMPLE = "fn alpha() {}\nfn beta() {}\nfn gamma() {\n    let x = 42;\n}\nfn delta() {}\n"
LINES = (3, 5)
QUESTION = ("Which file and line range did I just point you at (or select)? "
            "Reply with only path:start-end and nothing else.")
# The tail of the question, used to find where Claude's reply begins.
ANSWER_STARTS = "nothing else."
ANSWER = re.compile(r"lib\.rs:?\s*%d\s*[-–]\s*%d" % LINES)


def squashed(text):
    """Whitespace-free lowercase, for matching text a TUI may have wrapped
    or drawn a box around."""
    return re.sub(r"\s+", "", text).lower()


def main():
    require("claude")
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    selx = os.path.abspath(args[0]) if args else build_selx()
    ambient = "--ambient" in sys.argv

    ws = tempfile.mkdtemp(prefix="selx-ismoke-")
    state = tempfile.mkdtemp(prefix="sx", dir="/tmp")
    env = dict(os.environ, SELX_STATE_DIR=state, SELX_CONFIG="")
    # Don't look like a nested Claude session: the CLI behaves differently
    # when it thinks it was launched by another Claude.
    for k in [k for k in env if k.startswith("CLAUDE")]:
        env.pop(k)
    try:
        sys.exit(run(selx, ws, state, env, ambient))
    finally:
        subprocess.run([selx, "stop", "--all"], env=env, capture_output=True)
        shutil.rmtree(ws, ignore_errors=True)
        shutil.rmtree(state, ignore_errors=True)


def run(selx, ws, state, env, ambient):
    with open(os.path.join(ws, "lib.rs"), "w") as f:
        f.write(SAMPLE)

    port = subprocess.check_output([selx, "ensure"], cwd=ws, env=env).decode().strip()
    log = glob.glob(os.path.join(state, "*", "daemon.log"))[0]
    env["CLAUDE_CODE_SSE_PORT"] = port
    env["ENABLE_IDE_INTEGRATION"] = "true"
    lock = os.path.expanduser("~/.claude/ide/%s.lock" % port)
    print("daemon port %s; lock: %s" % (port, os.path.exists(lock)))

    claude = Tui(["claude", "--model", "haiku"], cwd=ws, env=env)
    try:
        if not wait_for_connection(claude, log):
            print("FAIL: Claude never connected to the daemon")
            return report(claude, log, 1)

        claude.drain(3)
        kind = [] if ambient else ["--explicit"]
        sent = subprocess.run(
            [selx, "send", "--file", "lib.rs", "--start", str(LINES[0]),
             "--end", str(LINES[1]), *kind, "--source", "smoke", "--json"],
            cwd=ws, env=env, capture_output=True, text=True)
        print("send:", sent.stdout.strip(), sent.stderr.strip())
        claude.drain(2)

        claude.send(QUESTION, wait=1)
        claude.send("\r", wait=0)
        answered = claude.wait_for(ANSWER, timeout=90, after=ANSWER_STARTS)
        print("answer: %s" % ("yes" if answered else "NOT FOUND"))
        claude.send("/exit\r", wait=3)
        return report(claude, log, 0 if answered else 1)
    finally:
        claude.close()


def wait_for_connection(claude, log, timeout=40):
    """Wait for the daemon to log Claude's IDE connection, accepting the
    one-time trust prompt a fresh scratch folder provokes."""
    trusted = False
    for _ in range(timeout * 5):
        claude.drain(0.2)
        text = squashed(claude.text())
        if not trusted and "trustthisfolder" in text:
            # The dialog is a two-option list, "No, exit" and "Yes, I trust
            # this folder", and since Claude Code 2.1.3xx the cursor starts
            # on No: a bare Enter quits Claude. Move it to Yes first, unless
            # the cursor (❯) is already there.
            if "❯yes" not in text:
                claude.send("\x1b[B", wait=0.3)
            claude.send("\r", wait=0)
            trusted = True
            print("accepted trust prompt")
        if "Claude Code connected" in open(log).read():
            return True
    return False


def report(claude, log, code):
    """Everything a human needs to see why it passed or failed."""
    text = claude.text()
    cut = text.rfind(ANSWER_STARTS)
    tail = text[cut + len(ANSWER_STARTS):] if cut >= 0 else text[-1500:]
    print("--- transcript (from the question on) ---")
    # Claude's spinner glyphs are noise once the escape codes are gone.
    print(re.sub(r"[✶✻✽✢✳·⏵⏸❯]+", " ", tail)[:1500])
    print("--- daemon log ---")
    print("\n".join(l[:700] for l in open(log).read().splitlines()
                    if "ready" not in l))
    print("OK" if code == 0 else "FAIL")
    return code


if __name__ == "__main__":
    main()
