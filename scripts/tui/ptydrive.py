"""Runs a program in a pseudo-terminal and keeps a copy of its screen, for testing eidra's TUI.

`Session` starts the program in a pty of a given size and feeds what it writes to `Term`, a small
terminal emulator: printing with a deferred wrap at the right margin, carriage return, line feed
scrolling the top row into a scrollback, cursor movement and erasing. Styles are ignored; private
modes are recorded, so a test can check that the terminal was put back.

Two things a test must get right, or it reports rendering bugs that are not there:
- Output arrives in pieces that can split a character, so it is decoded incrementally.
- A frame arrives in pieces too, so the screen is judged only between synchronized updates
  (CSI ? 2026 h ... l), which is all a terminal that supports them shows.

Standard library only.
"""

import codecs
import fcntl
import os
import pty
import re
import select
import signal
import struct
import termios
import time
import unicodedata

CSI = re.compile(r"\x1b\[([0-9;?<>=]*)([\x40-\x7e])")


class Term:
    """The screen, the scrollback above it, and the cursor."""

    def __init__(self, cols, rows):
        self.cols, self.rows = cols, rows
        self.grid = [[" "] * cols for _ in range(rows)]
        self.scrollback = []
        self.r = self.c = 0
        self.wrap_pending = False
        self.pending = ""
        self.modes = []
        # Inside a synchronized update, the screen is half drawn.
        self.syncing = False
        # Rows the terminal had to wrap itself: the program measured them differently.
        self.wraps = []

    @staticmethod
    def width(ch):
        if unicodedata.combining(ch) or ch in "​‍︎️":
            return 0
        return 2 if unicodedata.east_asian_width(ch) in "WF" else 1

    def feed(self, data):
        b = self.pending + data
        i = 0
        while i < len(b):
            ch = b[i]
            if ch == "\x1b":
                if i + 1 >= len(b):
                    break
                if b[i + 1] != "[":
                    i += 2
                    continue
                m = CSI.match(b, i)
                if not m:
                    if len(b) - i < 40:
                        break  # the rest of the sequence is still to come
                    i += 2
                    continue
                self.csi(m.group(1), m.group(2))
                i = m.end()
                continue
            if ch == "\r":
                self.c = 0
                self.wrap_pending = False
            elif ch == "\n":
                self.line_feed()
                self.wrap_pending = False
            elif ch >= " " and ch != "\x7f":
                self.put(ch)
            i += 1
        self.pending = b[i:]

    def line_feed(self):
        if self.r + 1 < self.rows:
            self.r += 1
        else:
            self.scrollback.append("".join(self.grid.pop(0)).rstrip())
            self.grid.append([" "] * self.cols)

    def put(self, ch):
        w = self.width(ch)
        if w == 0:
            return
        if self.wrap_pending or self.c + w > self.cols:
            self.wraps.append("".join(self.grid[self.r]).rstrip())
            self.c = 0
            self.line_feed()
            self.wrap_pending = False
        self.grid[self.r][self.c] = ch
        if w == 2 and self.c + 1 < self.cols:
            self.grid[self.r][self.c + 1] = ""
        self.c += w
        if self.c >= self.cols:
            self.c = self.cols - 1
            self.wrap_pending = True

    def csi(self, params, final):
        if params[:1] in ("?", "<", ">", "="):
            if params == "?2026":
                self.syncing = final == "h"
            elif len(self.modes) < 1000:
                self.modes.append(params + final)
            return
        nums = [int(p) if p else 0 for p in params.split(";")] if params else []
        n = nums[0] if nums else 0
        if final == "A":
            self.r = max(0, self.r - max(n, 1))
            self.wrap_pending = False
        elif final == "B":
            self.r = min(self.rows - 1, self.r + max(n, 1))
        elif final == "C":
            self.c = min(self.cols - 1, self.c + max(n, 1))
        elif final == "D":
            self.c = max(0, self.c - max(n, 1))
        elif final == "G":
            self.c = max(n, 1) - 1
            self.wrap_pending = False
        elif final == "H":
            self.r = (nums[0] if nums else 1) - 1
            self.c = (nums[1] if len(nums) > 1 else 1) - 1
        elif final == "J":
            if n == 0:
                self.grid[self.r][self.c :] = [" "] * (self.cols - self.c)
                for row in self.grid[self.r + 1 :]:
                    row[:] = [" "] * self.cols
            elif n == 2:
                for row in self.grid:
                    row[:] = [" "] * self.cols
        elif final == "K":
            self.grid[self.r][self.c :] = [" "] * (self.cols - self.c)
        elif final != "m":
            self.modes.append(f"unhandled CSI {params}{final}")

    def resize(self, cols, rows):
        """As a terminal that cuts rows rather than reflowing them, keeping the cursor's row."""
        grid = [(row + [" "] * cols)[:cols] for row in self.grid]
        while len(grid) > rows:
            if self.r > 0:
                self.scrollback.append("".join(grid.pop(0)).rstrip())
                self.r -= 1
            else:
                grid.pop()
        while len(grid) < rows:
            grid.append([" "] * cols)
        self.grid, self.cols, self.rows = grid, cols, rows
        self.c = min(self.c, cols - 1)

    def screen(self):
        return ["".join(row).rstrip() for row in self.grid]

    def text(self):
        return "\n".join(self.scrollback + self.screen())


class Session:
    """A program running in a pty, and the terminal it sees."""

    def __init__(self, argv, cols=100, rows=30, env=None):
        self.term = Term(cols, rows)
        pid, fd = pty.fork()
        if pid == 0:
            # The size is set before the program starts, so that it reads the right one.
            try:
                fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
                os.execvpe(argv[0], argv, env or os.environ)
            finally:
                os._exit(127)  # not on into the caller's code, in the child
        self.pid, self.fd = pid, fd
        self.status = None
        self.decoder = codecs.getincrementaldecoder("utf-8")(errors="replace")

    def pump(self, timeout=0.05):
        """Reads what the program wrote within `timeout`; true if there was something."""
        ready, _, _ = select.select([self.fd], [], [], timeout)
        data = b""
        if ready:
            try:
                data = os.read(self.fd, 65536)
            except OSError:
                pass
            if data:
                self.term.feed(self.decoder.decode(data))
        if self.status is None:
            pid, status = os.waitpid(self.pid, os.WNOHANG)
            if pid:
                self.status = status
        return bool(data)

    def settle(self, quiet=0.3):
        """Reads for up to `quiet` seconds, then on to the end of a frame: the output of a busy
        program may never pause."""
        end = time.time() + quiet
        while time.time() < end and self.pump(0.05):
            pass
        end = time.time() + 2
        while self.term.syncing and time.time() < end:
            self.pump(0.02)

    def wait_for(self, predicate, timeout, what=""):
        """Reads until `predicate(session)` holds on a whole frame."""
        end = time.time() + timeout
        while time.time() < end:
            self.pump()
            if not self.term.syncing and predicate(self):
                return
            if self.status is not None:
                while self.pump(0.05):
                    pass
                if predicate(self):
                    return
                break
        raise AssertionError(f"waited {timeout}s for {what or predicate}\n{self.dump()}")

    def contains(self, text):
        """Whether `text` is anywhere in the scrollback or on the screen."""
        return text in self.term.text()

    def on_screen(self, text):
        return any(text in row for row in self.term.screen())

    def send(self, data, pause=0.15):
        """Writes keys to the program, then reads its output for `pause` seconds."""
        os.write(self.fd, data.encode() if isinstance(data, str) else data)
        end = time.time() + pause
        while time.time() < end:
            self.pump(0.02)

    def type(self, text):
        """Types `text` a key at a time; a \\r in it is Enter."""
        for ch in text:
            self.send(ch, 0.01)
        self.send(b"", 0.2)

    def resize(self, cols, rows):
        self.term.resize(cols, rows)
        fcntl.ioctl(self.fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        os.kill(self.pid, signal.SIGWINCH)
        self.send(b"", 0.3)

    def signal(self, sig):
        os.kill(self.pid, sig)

    def wait_exit(self, timeout):
        """The program's exit code, once it has exited and its last output is read."""
        end = time.time() + timeout
        while self.status is None and time.time() < end:
            self.pump()
        while self.pump(0.05):
            pass
        if self.status is None:
            raise AssertionError(f"it did not exit within {timeout}s\n{self.dump()}")
        return os.waitstatus_to_exitcode(self.status)

    def kill(self):
        if self.status is None:
            os.kill(self.pid, signal.SIGKILL)
            os.waitpid(self.pid, 0)
            self.status = 0

    def dump(self, scrollback=40):
        t = self.term
        lines = [f"--- scrollback, last {scrollback} of {len(t.scrollback)} lines:"]
        lines += t.scrollback[-scrollback:]
        lines.append(f"--- screen {t.cols}x{t.rows}, cursor at row {t.r}, column {t.c}:")
        lines += [f"{i:2}|{row}" for i, row in enumerate(t.screen())]
        return "\n".join(lines)
