#!/usr/bin/env python3
"""Checks eidra's TUI end to end without a model: eidra runs in a pseudo-terminal against a fake
server that streams scripted replies, and each scenario drives the keyboard and reads the screen.
No GPU, about 15 s. Run it after any change to the TUI; scripts/tui/smoke.py then tries the real
model.

    scripts/tui/offline.py                    # every scenario, with target/debug/eidra
    scripts/tui/offline.py approval queue -v  # these two, printing the screen as they go
    scripts/tui/offline.py --binary target/release/eidra
    scripts/tui/offline.py --docker eidra     # in the image, through scripts/eidra-docker

Each scenario prints one line, and the script exits non-zero if any failed. Standard library only.
"""

import argparse
import base64
import os
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time
import tomllib
from pathlib import Path

from fake_server import Server, reply
from ptydrive import Session

ROOT = Path(__file__).resolve().parents[2]
SCENARIOS = []


def scenario(fn):
    SCENARIOS.append(fn)
    return fn


def idle(s):
    """The turn is the user's: the input box is empty and nothing is running."""
    return s.on_screen("› Message eidra") and not s.on_screen("esc to interrupt")


class Case:
    """One scenario's world: a workspace and a session directory, removed afterwards, and the
    eidra sessions it starts."""

    def __init__(self, binary, verbose, image=None):
        self.binary = binary
        self.verbose = verbose
        self.image = image
        self.dir = Path(tempfile.mkdtemp(prefix="eidra-tui-"))
        self.workspace = self.dir / "work"
        self.workspace.mkdir()
        # A home of the scenario's own, so that ~/.agents/skills is its own too.
        self.home = self.dir / "home"
        self.home.mkdir()
        self.sessions = []

    def start(self, *args, server=None, cols=90, rows=24):
        url = server.url if server else f"http://127.0.0.1:{free_port()}/v1"
        # Sessions and checkpoints go to an eidra directory of the scenario's own, not the user's.
        env = dict(os.environ, TERM="xterm-256color", EIDRA_HOME=str(self.dir / "eidra"))
        env.pop("RADIANCE_API_KEY", None)
        if self.image:
            # The wrapper works in the current directory and keeps sessions in EIDRA_HOME.
            argv = [str(ROOT / "scripts/eidra-docker"), "--base-url", url, *args]
            env["EIDRA_IMAGE"] = self.image
            env["EIDRA_SKILLS"] = str(self.home / ".agents/skills")
        else:
            argv = [str(self.binary), "-C", str(self.workspace), "--base-url", url]
            argv += ["--session-dir", str(self.dir / "sessions"), *args]
            env["HOME"] = str(self.home)
        s = Session(argv, cols=cols, rows=rows, env=env, cwd=self.workspace)
        self.sessions.append(s)
        s.wait_for(idle, 10, "eidra to start")
        return s

    def show(self, s, what):
        if self.verbose:
            s.settle()
            print(f"\n===== {what}\n{s.dump()}")

    def close(self):
        for s in self.sessions:
            s.kill()
        shutil.rmtree(self.dir, ignore_errors=True)


def free_port():
    """A port nothing listens on, so that a request to it is refused at once."""
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def quit_cleanly(s):
    """Ctrl+D on an empty input box: eidra exits 0, leaves its log's path, and puts the terminal
    back as it found it."""
    s.send("\x04")
    code = s.wait_exit(10)
    assert code == 0, f"exit code {code}\n{s.dump()}"
    assert s.contains("session log:"), s.dump()
    for mode in ["<1u", "?2004l", "?25h"]:
        assert mode in s.term.modes, f"{mode} was not restored: {s.term.modes}"
    assert not s.term.wraps, f"the terminal wrapped rows itself: {s.term.wraps}"


def interrupted_note():
    profile = tomllib.loads((ROOT / "profiles/qwen3.8-radiance/profile.toml").read_text())
    return profile["prompts"]["interrupted"]


@scenario
def basics(case):
    """Typing, a request that fails, commands, history, a resize, and quitting."""
    s = case.start(cols=80, rows=20)
    assert s.contains("instructions: none (no "), s.dump()
    s.type("hello there")
    assert s.on_screen("› hello there"), s.dump()
    s.send("\x1b\r")  # alt+enter
    s.type("second line")
    assert s.on_screen("  second line"), s.dump()
    s.send("\r")
    s.wait_for(lambda s: s.contains("✗ request 1") and idle(s), 10, "the error")
    assert s.contains("Connection refused"), s.dump()
    case.show(s, "after a failed request")

    s.type("/help\r")
    s.wait_for(lambda s: s.contains("ctrl+t"), 5, "the help")
    s.type("/bogus\r")
    s.wait_for(lambda s: s.contains("/bogus is not a command"), 5, "the unknown command")
    s.resize(60, 16)
    s.settle()
    assert s.on_screen("─" * 59) and not s.on_screen("─" * 60), s.dump()
    case.show(s, "after /help, /bogus and a resize to 60x16")

    s.send("\x1b[A")  # up: the last thing sent
    assert s.on_screen("› /bogus"), s.dump()
    s.send("\x03")  # ctrl+c clears the input
    assert idle(s), s.dump()
    quit_cleanly(s)


@scenario
def reply_and_status(case):
    """A reply: its reasoning as one line, its text as markdown, and the status line's numbers;
    the header names the workspace's AGENTS.md."""
    (case.workspace / "AGENTS.md").write_text("Answer briefly.\n")
    text = "# Done\n\nSome **bold** text.\nline one\nline two"
    with Server([reply(reasoning="A short thought.", text=text, reasoning_tokens=7)]) as server:
        s = case.start(server=server, cols=100)
        assert s.contains("instructions: AGENTS.md"), s.dump()
        s.type("hi\r")
        s.wait_for(lambda s: s.contains("line two") and idle(s), 10, "the reply")
        case.show(s, "after the reply")
        for row in ["∴ thought for 7 tokens", "Done", "Some bold text.", "line one", "line two"]:
            assert any(r == row for r in s.term.screen()), f"no row {row!r}\n{s.dump()}"
        status = "1.2K/256K · cache 96% · prefill 4.0K tok/s · decode 150 tok/s"
        assert s.on_screen(status), s.dump()
        quit_cleanly(s)


@scenario
def tool_calls(case):
    """A tool call and its result, then the reply; the model gets the output."""
    replies = [reply(calls=[("bash", {"command": "echo hello"})]), reply(text="It said hello.")]
    with Server(replies) as server:
        s = case.start(server=server)
        s.type("say hello\r")
        s.wait_for(lambda s: s.contains("It said hello.") and idle(s), 10, "the reply")
        case.show(s, "after the tool call")
        assert s.contains("● $ echo hello") and s.contains("  ⎿ hello"), s.dump()
        result = server.requests[1]["messages"][-1]
        assert result["role"] == "tool" and result["content"] == "hello", result
        quit_cleanly(s)


@scenario
def approval(case):
    """A command on the confirm list waits for an answer; keys in its first moments are ignored."""
    (case.workspace / "keep.txt").write_text("keep me\n")
    replies = [reply(calls=[("bash", {"command": "rm keep.txt"})]), reply(text="Left it alone.")]
    with Server(replies) as server:
        s = case.start("--confirm", "rm", server=server)
        s.type("remove keep.txt\r")
        s.wait_for(lambda s: s.on_screen("Run this command?"), 10, "the question")
        case.show(s, "the question")
        s.send("n", 0.05)  # typed as it appeared: ignored
        assert s.on_screen("Run this command?"), s.dump()
        time.sleep(0.5)
        s.send("n")
        s.wait_for(lambda s: s.contains("Left it alone.") and idle(s), 10, "the reply")
        assert (case.workspace / "keep.txt").exists()
        assert s.contains("⎿ Error: not run: this command needs the user's approval"), s.dump()
        result = server.requests[1]["messages"][-1]["content"]
        assert result.startswith("Error: not run") and "declined" in result, result
        quit_cleanly(s)


@scenario
def interrupt(case):
    """Esc stops a reply mid-stream, and the next message tells the model so."""
    replies = [reply(reasoning="Let me think about this", stall=30), reply(text="Fine.")]
    with Server(replies) as server:
        s = case.start(server=server)
        s.type("a long task\r")
        s.wait_for(lambda s: s.on_screen("thinking"), 10, "the reasoning")
        case.show(s, "thinking")
        s.send("\x1b")
        s.wait_for(lambda s: s.contains("! interrupted") and idle(s), 10, "the interrupt")
        s.type("something else\r")
        s.wait_for(lambda s: s.contains("Fine.") and idle(s), 10, "the next reply")
        case.show(s, "after the next reply")
        messages = server.requests[1]["messages"]
        roles = [m["role"] for m in messages]
        assert roles == ["system", "user", "user"], roles
        last = messages[-1]["content"]
        assert last == f"{interrupted_note()}\n\nsomething else", last
        quit_cleanly(s)


@scenario
def queue(case):
    """A message sent during a turn shows as queued and goes out when the turn ends."""
    replies = [reply(text="First answer.", wait=1.5), reply(text="Second answer.")]
    with Server(replies) as server:
        s = case.start(server=server)
        s.type("first\r")
        s.wait_for(lambda s: s.on_screen("esc to interrupt"), 5, "the turn")
        s.type("second\r")
        s.wait_for(lambda s: s.on_screen("queued › second"), 5, "the queued message")
        case.show(s, "queued")
        s.wait_for(lambda s: s.contains("Second answer.") and idle(s), 10, "both turns")
        assert server.requests[1]["messages"][-1]["content"] == "second"
        quit_cleanly(s)


@scenario
def send_now(case):
    """Ctrl+Enter during a turn stops it and sends at once, after what was queued."""
    replies = [reply(reasoning="Let me think about this", stall=30), reply(text="Done that.")]
    with Server(replies) as server:
        s = case.start(server=server)
        s.type("a long task\r")
        s.wait_for(lambda s: s.on_screen("thinking"), 10, "the reasoning")
        assert s.on_screen("› Queue message"), s.dump()
        s.type("first\r")
        s.wait_for(lambda s: s.on_screen("queued › first"), 5, "the queued message")
        s.type("do this instead")
        s.send("\x1b[13;5u")  # ctrl+enter, as the kitty keyboard protocol sends it
        s.wait_for(lambda s: s.contains("Done that.") and idle(s), 10, "the next reply")
        case.show(s, "after ctrl+enter")
        assert s.contains("! interrupted"), s.dump()
        last = server.requests[1]["messages"][-1]["content"]
        assert last == f"{interrupted_note()}\n\nfirst\n\ndo this instead", last
        quit_cleanly(s)


@scenario
def compact(case):
    """/compact summarises the conversation when asked, keeping what the user names, and the
    conversation goes on from the summary."""
    replies = [
        reply(text="First answer."),
        reply(text="SUMMARY: one answer so far."),
        reply(text="Second answer."),
    ]
    with Server(replies) as server:
        s = case.start(server=server)
        s.type("first\r")
        s.wait_for(lambda s: s.contains("First answer.") and idle(s), 10, "the first reply")
        s.type("/compact the numbers\r")
        s.wait_for(lambda s: s.contains("! compacted: the conversation goes on") and idle(s), 10,
                   "the summary")
        case.show(s, "after /compact")
        asked = server.requests[1]["messages"][-1]["content"]
        assert asked.endswith("wants it to keep in particular: the numbers]"), asked
        s.type("next\r")
        s.wait_for(lambda s: s.contains("Second answer.") and idle(s), 10, "the next reply")
        messages = server.requests[2]["messages"]
        assert [m["role"] for m in messages] == ["system", "user", "user"], messages
        assert "SUMMARY: one answer so far." in messages[1]["content"], messages[1]
        quit_cleanly(s)


@scenario
def background(case):
    """A subagent started in the background works while the chat goes on; its report goes to the
    agent when it ends, and the agent tells the user."""
    task = {"description": "write hello.py", "prompt": "Write hello.py.", "background": True}
    replies = [
        reply(calls=[("task", task)]),
        reply(text="Started it; ask away meanwhile."),
        reply(text="2 + 2 = 4."),
        reply(text="hello.py is ready."),
    ]
    subagent = [
        reply(calls=[("bash", {"command": "sleep 3; echo 'print(1)' > hello.py"})]),
        reply(calls=[("finish", {"summary": "Wrote hello.py."})]),
    ]
    with Server(replies, subagent=subagent) as server:
        s = case.start(server=server, cols=100)
        s.type("write hello.py in the background\r")
        s.wait_for(lambda s: s.contains("ask away meanwhile.") and idle(s), 10, "the start")
        assert s.contains("● task: write hello.py  (background)"), s.dump()
        assert s.on_screen("· 1 task running ·"), s.dump()
        s.type("what is 2 + 2?\r")
        s.wait_for(lambda s: s.contains("2 + 2 = 4.") and idle(s), 10, "the answer")
        assert not s.contains("ended after"), f"the task ended before the chat went on\n{s.dump()}"
        case.show(s, "chatting while the task works")
        s.wait_for(lambda s: s.contains("hello.py is ready.") and idle(s), 20, "the report")
        case.show(s, "after the report")
        assert s.contains('◆ background task "write hello.py" ended after'), s.dump()
        assert not s.contains("Started in the background") and not s.contains("● finish"), s.dump()
        assert (case.workspace / "hello.py").exists()
        report = server.requests[3]["messages"][-1]["content"]
        assert "write hello.py" in report and "\n\nWrote hello.py.\n\n" in report, report
        assert not s.on_screen("task running"), s.dump()
        quit_cleanly(s)


# A 1 × 1 PNG.
PIXEL = base64.b64decode(
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg=="
)


@scenario
def attachments(case):
    """An image the message mentions goes to the model with it."""
    (case.workspace / "chart.png").write_bytes(PIXEL)
    with Server([reply(text="One pixel.")]) as server:
        s = case.start(server=server)
        s.type("what is in @chart.png?\r")
        s.wait_for(lambda s: s.contains("One pixel.") and idle(s), 10, "the reply")
        assert s.contains("⎿ attached chart.png: a 1 × 1 PNG image"), s.dump()
        content = server.requests[0]["messages"][-1]["content"]
        assert isinstance(content, list) and content[0]["text"] == "what is in @chart.png?", content
        assert content[1]["image_url"]["url"].startswith("data:image/png;base64,"), content
        quit_cleanly(s)


@scenario
def pdf_pages(case):
    """read gives a PDF's pages as their text layer and their images."""
    if not all(shutil.which(t) for t in ("pdftoppm", "pdfinfo", "pdftotext", "magick")):
        print("      (skipped: needs poppler and ImageMagick)")
        return
    subprocess.run(["magick", "-size", "200x100", "xc:white", str(case.workspace / "scan.pdf")],
                   check=True)
    replies = [reply(calls=[("read", {"path": "scan.pdf"})]), reply(text="A blank page.")]
    with Server(replies) as server:
        s = case.start(server=server)
        s.type("what is in scan.pdf?\r")
        s.wait_for(lambda s: s.contains("A blank page.") and idle(s), 15, "the reply")
        result = server.requests[1]["messages"][-1]
        assert result["role"] == "tool" and isinstance(result["content"], list), result
        text = result["content"][0]["text"]
        assert "a PDF of 1 page" in text and "no text layer" in text, text
        assert result["content"][1]["type"] == "image_url", result["content"]
        quit_cleanly(s)


@scenario
def undo(case):
    """/diff shows what a turn changed, through bash too; /undo puts it back and the next message
    tells the model."""
    if not shutil.which("git"):
        print("      (skipped: needs git)")
        return
    (case.workspace / "a.py").write_text("a = 1\n")
    calls = [("bash", {"command": "echo 'a = 2' > a.py"}),
             ("write", {"path": "b.py", "content": "b = 1\n"})]
    replies = [reply(calls=calls), reply(text="Changed both."), reply(text="Back to one.")]
    with Server(replies) as server:
        s = case.start(server=server)
        s.type("change things\r")
        s.wait_for(lambda s: s.contains("Changed both.") and idle(s), 10, "the reply")
        s.type("/diff\r")
        s.wait_for(lambda s: s.contains("+a = 2"), 10, "the diff")
        assert s.contains("changes since the last turn (turn 1) began") and s.contains("+b = 1")
        s.type("/undo\r")
        s.wait_for(lambda s: s.contains("undid turn 1 · change things"), 10, "the undo")
        case.show(s, "after the undo")
        assert s.contains("⎿ put back: a.py") and s.contains("⎿ removed, since the turn made them:")
        assert (case.workspace / "a.py").read_text() == "a = 1\n"
        assert not (case.workspace / "b.py").exists()
        s.type("what now?\r")
        s.wait_for(lambda s: s.contains("Back to one.") and idle(s), 10, "the reply")
        message = server.requests[2]["messages"][-1]["content"]
        assert message.startswith("[The user undid what your turn that began with \"change things\"")
        assert message.endswith("]\n\nwhat now?"), message
        quit_cleanly(s)


@scenario
def resume(case):
    """--continue goes on with the last session: its conversation is shown, and the next request
    holds all of it."""
    replies = [reply(text="The answer is 42."), reply(text="You asked about the answer.")]
    with Server(replies) as server:
        s = case.start(server=server)
        s.type("what is the answer?\r")
        s.wait_for(lambda s: s.contains("The answer is 42.") and idle(s), 10, "the reply")
        quit_cleanly(s)
        s = case.start("--continue", server=server)
        case.show(s, "resumed")
        assert s.contains("› what is the answer?") and s.contains("The answer is 42."), s.dump()
        assert s.contains("resumed: the conversation goes on from here (3 messages)"), s.dump()
        s.type("what did I ask?\r")
        s.wait_for(lambda s: s.contains("You asked about") and idle(s), 10, "the reply")
        roles = [m["role"] for m in server.requests[1]["messages"]]
        assert roles == ["system", "user", "assistant", "user"], roles
        quit_cleanly(s)


@scenario
def bang_and_mention(case):
    """!command runs in the workspace and the model hears of it with the next message, which also
    carries the file it mentions; Tab completes the mention."""
    (case.workspace / "notes.txt").write_text("remember the milk\n")
    with Server([reply(text="Milk, and it said hello.")]) as server:
        s = case.start(server=server)
        s.type("!echo hello from the shell\r")
        s.wait_for(lambda s: s.contains("⎿ succeeded") and idle(s), 10, "the command")
        assert s.contains("  hello from the shell"), s.dump()
        s.type("what do @no")
        s.type("\t")
        s.wait_for(lambda s: s.on_screen("what do @notes.txt"), 5, "the completion")
        s.type("say?\r")
        s.wait_for(lambda s: s.contains("Milk, and it said") and idle(s), 10, "the reply")
        case.show(s, "after the reply")
        assert s.contains("⎿ attached notes.txt: 1 line"), s.dump()
        message = server.requests[0]["messages"][-1]["content"]
        assert message.startswith("[The user ran this command in the workspace: `echo hello"), message
        assert "what do @notes.txt say?" in message and message.endswith("1\tremember the milk")
        quit_cleanly(s)


@scenario
def skills(case):
    """~/.agents/skills is made if it is not there; a slash opens the list of commands and skills,
    which narrows as you type; Tab completes a skill, whose instructions go with the message; the
    model loads another with `skill`; /skills lists them."""
    s = case.start()
    skills = case.home / ".agents/skills"
    assert skills.is_dir(), f"{skills} was not made"
    assert s.contains("skills: none in ~/.agents/skills"), s.dump()
    quit_cleanly(s)

    for name, description, instructions in [
        ("greet", "Greets people by name.", "Say hello to $ARGUMENTS."),
        ("wave", "Waves at people.", "Wave with both hands.\nThen bow."),
    ]:
        (skills / name).mkdir()
        text = f"---\nname: {name}\ndescription: {description}\n---\n{instructions}\n"
        (skills / name / "SKILL.md").write_text(text)
    replies = [reply(calls=[("skill", {"name": "wave"})]), reply(text="Waved, and said hello.")]
    with Server(replies) as server:
        s = case.start(server=server)
        assert s.contains("skills: 2 in ~/.agents/skills · /skills lists them"), s.dump()
        s.type("/")
        s.wait_for(lambda s: s.on_screen("/compact [what]") and s.on_screen("/greet"), 5, "the list")
        s.type("gr")
        s.wait_for(lambda s: not s.on_screen("/compact"), 5, "the list to narrow")
        assert s.on_screen("/greet  Greets people by name."), s.dump()
        case.show(s, "after typing /gr")
        s.type("\t")
        # The list closes once the name is complete, with a space after it for what follows.
        done = lambda s: s.on_screen("› /greet") and not s.on_screen("Greets people")
        s.wait_for(done, 5, "the completion")
        s.type("Ada\r")
        s.wait_for(lambda s: s.contains("Waved, and said hello.") and idle(s), 10, "the reply")
        case.show(s, "after the reply")
        assert s.contains("⎿ skill greet: 1 line of instructions"), s.dump()
        assert s.contains("● skill wave") and s.contains("⎿ loaded, 4 lines"), s.dump()
        first = server.requests[0]
        assert "skill" in {t["function"]["name"] for t in first["tools"]}, first["tools"]
        system = first["messages"][0]["content"]
        assert "- greet: Greets people by name.\n- wave: Waves at people.\n" in system, system
        message = first["messages"][-1]["content"]
        assert message.startswith("/greet Ada\n\n[The skill greet. Its files are in "), message
        assert message.endswith("Follow its instructions:]\n\nSay hello to Ada."), message
        loaded = server.requests[1]["messages"][-1]
        assert loaded["role"] == "tool" and loaded["content"].endswith("Wave with both hands.\nThen bow."), loaded
        s.type("/skills\r")
        s.wait_for(lambda s: s.contains("skills in ~/.agents/skills; /name runs one"), 5, "/skills")
        assert s.contains("  wave   Waves at people."), s.dump()
        quit_cleanly(s)


@scenario
def sigterm(case):
    """SIGTERM while a command runs: eidra quits cleanly and the command's process group goes."""
    command = "(sleep 2; touch marker) & sleep 30"
    with Server([reply(calls=[("bash", {"command": command})]), reply(text="ok")]) as server:
        s = case.start(server=server)
        s.type("go\r")
        s.wait_for(lambda s: s.on_screen("● $ (sleep 2"), 10, "the command")
        s.signal(signal.SIGTERM)
        code = s.wait_exit(10)
        assert code == 0, f"exit code {code}\n{s.dump()}"
        assert s.contains("interrupted this call"), s.dump()
        for mode in ["<1u", "?2004l", "?25h"]:
            assert mode in s.term.modes, f"{mode} was not restored"
        time.sleep(2.5)
        assert not (case.workspace / "marker").exists(), "the command outlived eidra"


def stale(binary):
    """Whether a source file is newer than the binary."""
    built = binary.stat().st_mtime
    sources = [p for d in ["crates", "profiles"] for p in (ROOT / d).rglob("*") if p.is_file()]
    return any(p.stat().st_mtime > built for p in sources)


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("names", nargs="*", metavar="SCENARIO", help="the scenarios to run")
    parser.add_argument("--binary", type=Path, default=ROOT / "target/debug/eidra")
    parser.add_argument("--docker", metavar="IMAGE",
                        help="run eidra in this image through scripts/eidra-docker instead")
    parser.add_argument("-v", "--verbose", action="store_true", help="print the screens")
    args = parser.parse_args()
    chosen = [f for f in SCENARIOS if not args.names or f.__name__ in args.names]
    unknown = set(args.names) - {f.__name__ for f in SCENARIOS}
    if unknown:
        known = ", ".join(f.__name__ for f in SCENARIOS)
        sys.exit(f"no scenario {', '.join(sorted(unknown))}; there are {known}")
    if args.docker:
        pass
    elif not args.binary.exists():
        sys.exit(f"{args.binary} does not exist: build it with scripts/cargo build")
    elif stale(args.binary):
        print(f"warning: {args.binary} is older than the sources: scripts/cargo build rebuilds it")

    failed = 0
    for fn in chosen:
        case = Case(args.binary.resolve(), args.verbose, args.docker)
        started = time.time()
        try:
            fn(case)
            print(f"  ok  {fn.__name__} ({time.time() - started:.1f} s)", flush=True)
        except AssertionError as e:
            failed += 1
            print(f"FAIL  {fn.__name__}: {e}", flush=True)
        finally:
            case.close()
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
