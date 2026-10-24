"""Shared scaffolding for the pty-driven smoke scripts.

Both smoke scripts drive a real terminal program and read back what it
painted, which needs the same four awkward things every time: fork a pty,
give it a window size a curses program will render into, pump the master
end so the child never blocks on a full buffer, and strip the escape codes
before matching on the text. They had two copies in two styles.

Not a test framework — `python3 scripts/<name>.py` is still the interface.
The leading underscore says this file is not one of them.
"""

import fcntl
import json
import os
import pty
import re
import select
import struct
import subprocess
import sys
import termios
import time

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

# CSI sequences, OSC strings, charset selections, and the bare carriage
# returns a full-screen redraw leaves behind. Enough that a `re.search` for
# what the program *said* is not defeated by how it drew it.
ANSI = re.compile(rb"\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b\][^\x07]*\x07|\x1b[=>()][A-Z0-9]?|\r")


def strip_ansi(raw):
    """Bytes from a terminal as readable text."""
    return ANSI.sub(b"", raw).decode("utf-8", "replace")


def build_selx():
    """The selx binary to drive: $SELX_BIN, else a debug build."""
    binpath = os.environ.get("SELX_BIN")
    if binpath:
        return os.path.abspath(binpath)
    subprocess.run(["cargo", "build"], cwd=REPO, check=True)
    meta = subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--no-deps"],
        cwd=REPO, check=True, capture_output=True, text=True)
    target = json.loads(meta.stdout)["target_directory"]
    return os.path.join(target, "debug", "selx")


class Tui:
    """A full-screen terminal program running under a pty.

    Everything it prints is kept in `self.out`, so a failure can show what
    was on screen instead of only that something was missing.
    """

    # gocui (lazygit) renders nothing at 0x0, and Claude's input box wraps
    # by column, so both need a believable window.
    ROWS = 40
    COLS = 120

    def __init__(self, argv, cwd=None, env=None):
        self.argv = argv
        self.out = bytearray()
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            # Child: everything after the fork must end in exec or _exit.
            if cwd:
                os.chdir(cwd)
            if env is not None:
                os.environ.clear()
                os.environ.update(env)
            os.environ.setdefault("TERM", "xterm-256color")
            try:
                os.execvp(argv[0], argv)
            finally:
                os._exit(127)
        fcntl.ioctl(self.fd, termios.TIOCSWINSZ,
                    struct.pack("HHHH", self.ROWS, self.COLS, 0, 0))

    def drain(self, seconds):
        """Read for `seconds`. False once the program has gone away."""
        end = time.time() + seconds
        while time.time() < end:
            r, _, _ = select.select([self.fd], [], [], 0.2)
            if not r:
                continue
            try:
                chunk = os.read(self.fd, 65536)
            except OSError:
                return False
            if not chunk:
                return False
            self.out.extend(chunk)
        return True

    def send(self, keys, wait=0.5):
        """Type `keys`, then read what they produced."""
        os.write(self.fd, keys.encode())
        if wait:
            self.drain(wait)

    def text(self):
        """Everything printed so far, ANSI-stripped."""
        return strip_ansi(bytes(self.out))

    def wait_for(self, pattern, timeout, after=""):
        """Read until `pattern` (a regex) appears, or `timeout` elapses.

        `after` restricts the match to the text following its last
        occurrence — how you ask "did it answer *this* question?" rather
        than matching the question's own echo.
        """
        end = time.time() + timeout
        while True:
            text = self.text()
            if after:
                cut = text.rfind(after)
                text = text[cut + len(after):] if cut >= 0 else ""
            m = re.search(pattern, text)
            if m:
                return m
            if time.time() >= end or not self.drain(min(1.0, max(0.2, end - time.time()))):
                return None

    def close(self, grace=5.0):
        """Let it exit, then make sure it has."""
        try:
            os.close(self.fd)
        except OSError:
            pass
        end = time.time() + grace
        while time.time() < end:
            try:
                pid, _ = os.waitpid(self.pid, os.WNOHANG)
            except ChildProcessError:
                return
            if pid:
                return
            time.sleep(0.1)
        try:
            os.kill(self.pid, 9)
            os.waitpid(self.pid, 0)
        except (OSError, ChildProcessError):
            pass


def require(*tools):
    """Exit with SKIP unless every tool is on PATH."""
    import shutil
    for tool in tools:
        if not shutil.which(tool):
            sys.exit("SKIP: %s not on PATH" % tool)
