#!/usr/bin/env python3
"""Tries eidra's TUI against the real model: Qwen3.8 on radiance, through llama-swap. eidra runs in
a pseudo-terminal, gets a trivial reply, runs a tool with a message queued behind it, is
interrupted mid-reply, asks to run a command on its confirm list, shows reasoning in full, hears of
a `!` command and an `@` file, quits, and goes on with --continue. About 40 s of GPU.

    export RADIANCE_API_KEY=...       # as for any eidra run
    scripts/tui/smoke.py              # with target/debug/eidra
    scripts/tui/smoke.py -v --binary target/release/eidra

It refuses to run unless qwen-assistant is already loaded: asking llama-swap for it would unload
whatever else the GPUs are doing. Standard library only.
"""

import argparse
import json
import os
import shutil
import sys
import tempfile
import time
import urllib.request
from pathlib import Path

from ptydrive import Session

ROOT = Path(__file__).resolve().parents[2]
SWAP = "http://127.0.0.1:8000"  # llama-swap, in front of radiance (docs/DESIGN.md §2)


def get(path):
    request = urllib.request.Request(SWAP + path)
    request.add_header("Authorization", f"Bearer {os.environ['RADIANCE_API_KEY']}")
    with urllib.request.urlopen(request, timeout=5) as response:
        return json.load(response)


def model_ready():
    """Why the smoke test should not run now, or None if it can."""
    try:
        running = get("/running").get("running", [])
    except OSError as e:
        return f"llama-swap does not answer at {SWAP}: {e}"
    loaded = {m.get("model"): m.get("state") for m in running}
    if loaded.get("qwen-assistant") != "ready":
        names = ", ".join(f"{k} ({v})" for k, v in loaded.items()) or "nothing"
        return f"qwen-assistant is not loaded and ready (llama-swap has {names})"
    load = get("/upstream/qwen-assistant/load")
    print(f"radiance: {load.get('running', 0)} running, {load.get('waiting', 0)} waiting")
    return None


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--binary", type=Path, default=ROOT / "target/debug/eidra")
    parser.add_argument("-v", "--verbose", action="store_true", help="print the screens")
    parser.add_argument(
        "--keep", action="store_true", help="keep the workspace and session log (kept on failure)"
    )
    args = parser.parse_args()
    if not os.environ.get("RADIANCE_API_KEY"):
        sys.exit("RADIANCE_API_KEY is not set")
    if not args.binary.exists():
        sys.exit(f"{args.binary} does not exist: build it with scripts/cargo build")
    why = model_ready()
    if why:
        sys.exit(f"not running: {why}")

    work = Path(tempfile.mkdtemp(prefix="eidra-smoke-"))
    workspace = work / "work"
    workspace.mkdir()
    (workspace / "notes.txt").write_text("alpha\nbeta\ngamma\n")
    (workspace / "scratch.txt").write_text("delete me\n")
    argv = [str(args.binary.resolve()), "-C", str(workspace), "--confirm", "rm"]
    argv += ["--session-dir", str(work / "sessions")]
    env = dict(os.environ, TERM="xterm-256color", EIDRA_HOME=str(work / "eidra"))
    s = Session(argv, cols=100, rows=32, env=env)
    started = time.time()

    def busy(s):
        return s.on_screen("esc to interrupt") or s.on_screen("interrupting")

    def idle(s):
        return s.on_screen("› Message eidra") and not busy(s)

    def after(s, mark, *words):
        """Whether everything shown after the last `mark` has all of `words`."""
        tail = s.dump().split(mark)[-1]
        return mark in s.dump() and all(word in tail for word in words)

    def step(what):
        print(f"{time.time() - started:5.1f} s  {what}", flush=True)
        if args.verbose:
            s.settle()
            print(s.dump())

    try:
        s.wait_for(idle, 15, "eidra to start")

        s.type("Reply with exactly one word: pong\r")
        s.wait_for(lambda s: s.contains("pong") and idle(s), 120, "the reply")
        step("a trivial reply")

        s.type("How many lines does notes.txt have? Check with a tool.\r")
        s.wait_for(busy, 10, "the turn")
        s.type("Then reply with the word: done\r")
        s.wait_for(lambda s: s.on_screen("queued › Then reply"), 5, "the queued message")
        s.wait_for(lambda s: s.contains("› Then reply with the word: done"), 240, "the queued send")
        s.wait_for(idle, 240, "both turns")
        assert s.contains("● ") and s.contains("⎿ "), "no tool call"
        step("a tool call, and a message queued behind it")

        s.type("Count from 1 to 3000, one number per line, with no other text.\r")
        s.wait_for(lambda s: s.on_screen("thinking") or s.on_screen("writing"), 120, "the reply")
        time.sleep(0.5)
        s.send("\x1b")
        s.wait_for(lambda s: s.contains("! interrupted") and idle(s), 30, "the interrupt")
        step("interrupted mid-reply")

        s.type("Delete scratch.txt with rm, then reply with: deleted\r")
        s.wait_for(lambda s: s.on_screen("Run this command?"), 180, "the question")
        step("asked to run rm")
        time.sleep(0.5)  # keys that come with the question are ignored
        s.send("y")
        s.wait_for(lambda s: not s.on_screen("Run this command?"), 10, "the answer to take")
        # The model may want more on the confirm list than it was asked for: say no to that.
        while True:
            s.wait_for(lambda s: idle(s) or s.on_screen("Run this command?"), 240, "the turn")
            if idle(s):
                break
            command = next(r for r in s.term.screen() if r.startswith("  $ ")).strip()
            step(f"declined something it was not asked to run: {command}")
            time.sleep(0.5)
            s.send("n")
            s.wait_for(lambda s: not s.on_screen("Run this command?"), 10, "the answer to take")
        assert not (workspace / "scratch.txt").exists(), "scratch.txt is still there"
        assert not s.contains("\n2999\n"), "the model took up the interrupted count again"
        step("approved; the interrupted count was not taken up again")

        s.send("\x14")  # ctrl+t
        s.type("What is 17 * 3? Reply with just the number.\r")
        s.wait_for(lambda s: s.contains("51") and idle(s), 120, "the reply")
        step("reasoning shown in full")

        s.send("\x14")  # ctrl+t, back to one line
        s.type("!echo the code word is $((6 * 7))-kestrel\r")
        s.wait_for(lambda s: s.contains("⎿ succeeded") and idle(s), 20, "the command")
        s.type("What code word did my command print, and what is the last line of @notes.txt? "
               "Reply with just the two, separated by a space.\r")
        s.wait_for(lambda s: after(s, "separated by a space.", "42-kestrel", "gamma") and idle(s),
                   180, "the reply")
        step("a ! command and an @ file, heard by the model")

        s.send("\x04")
        code = s.wait_exit(15)
        assert code == 0, f"exit code {code}"
        for mode in ["<1u", "?2004l", "?25h"]:
            assert mode in s.term.modes, f"{mode} was not restored"
        assert not s.term.wraps, f"the terminal wrapped rows itself: {s.term.wraps}"
        step("quit")

        s = Session(argv + ["--continue"], cols=100, rows=32, env=env)
        s.wait_for(idle, 15, "the session to resume")
        assert s.contains("resumed: the conversation goes on from here"), "no resumed conversation"
        s.type("What was the code word my command printed? Reply with just it.\r")
        s.wait_for(lambda s: after(s, "Reply with just it.", "42-kestrel") and idle(s), 180,
                   "the reply")
        step("--continue: the conversation goes on")
        s.send("\x04")
        assert s.wait_exit(15) == 0
        print(f"ok: the smoke test passed in {time.time() - started:.0f} s")
    except AssertionError as e:
        print(f"FAIL: {e}\n{s.dump()}")
        args.keep = True
        sys.exit(1)
    finally:
        s.kill()
        if args.keep:
            print(f"kept the workspace and session log in {work}")
        else:
            shutil.rmtree(work, ignore_errors=True)


if __name__ == "__main__":
    main()
