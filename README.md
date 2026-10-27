# eidra

A coding agent in Rust, tuned for one local model: Qwen3.8 Flash-Next served by
[radiance](https://codeberg.org/StillDeadcode/radiance). It speaks the OpenAI chat-completions API,
so other servers work too, but its defaults, prompts and tools are chosen and measured against
radiance.

It has:
- `eidra`: an interactive session in the terminal, drawn inline like Claude Code's, with every turn
  checkpointed for `/undo`, sessions you can `--continue`, `@file` attachments and `!` commands
- `eidra exec`: one task, unattended, for scripts and evals
- after each edit of a Python file, the problems it introduced (ruff, ty), and images and PDFs
  the model can see
- subagents, which the model can start with the `task` tool; in an interactive session they can
  work in the background while you keep talking to the agent
- `eidra eval`: task suites run in containers, including Harbor-format tasks (SWE-rebench,
  Terminal-Bench)
- `eidra probe`, `eidra replay` and `eidra relay`: the server's prefix cache measured, a recorded
  request sent again, and a model socket for agents with no network

The design and the plan are in [docs/DESIGN.md](docs/DESIGN.md). Everything specific to the model
(prompts, tool descriptions, request settings, the context limit) lives in
[profiles/qwen3.8-radiance](profiles/qwen3.8-radiance), so tuning means editing files there.

## Build

Only Docker is needed. `scripts/cargo` runs cargo in a pinned Rust image; the first time, it builds
a small dev image that adds clippy and rustfmt.

    scripts/cargo build --release        # static binary: target/release/eidra
    scripts/cargo test --workspace
    scripts/cargo clippy --workspace --all-targets

The binary is static (musl), so it runs on the host and in any Linux container.

## Run it in a container

The agent runs the commands it chooses, as you, without asking, except for the commands on its
confirm list. A container keeps them inside the project's directory rather than your whole machine.

**1. Build the image.** It has bash, git, ripgrep, fd, jq, curl, poppler-utils, Python 3.13 with uv,
ruff and ty, and eidra:

    docker build -t eidra .

**2. Give it the API key.** The key is passed in when the container starts and is never stored in
the image:

    export RADIANCE_API_KEY="$(sed -n 's/^RADIANCE_API_KEY=//p' ~/radiance/.env | tr -d "\"'")"

**3. Run it in a project.** `scripts/eidra-docker` runs the image on the current directory. With no
arguments it starts an interactive session; with arguments it runs that eidra command:

    cd ~/projects/something
    ~/dev/eidra-cli/scripts/eidra-docker                                  # interactive
    ~/dev/eidra-cli/scripts/eidra-docker exec "Make the tests pass, then finish."
    ~/dev/eidra-cli/scripts/eidra-docker --confirm "git push"            # options as for eidra

What it mounts and passes in:

| on the host | in the container | why |
|---|---|---|
| the current directory | the same path, as the working directory | the agent's workspace; paths read the same inside and out |
| `~/.eidra` | the same path, with `EIDRA_HOME` | your `AGENTS.md`, session logs and checkpoints, kept after the container is gone |
| `~/.cache/uv` | the same path, with `UV_CACHE_DIR` | Python packages are downloaded once |
| `~/.gitconfig`, if there is one | `/home/eidra/.gitconfig`, read-only | your name and email on the agent's commits |
| `/etc/localtime` | the same path, read-only | your time zone |
| `$EIDRA_PROFILE`, if set | the same path, read-only | a profile of your own |

| variable | |
|---|---|
| `RADIANCE_API_KEY` | the server's API key (the profile names the variable) |
| `EIDRA_BASE_URL`, `EIDRA_MODEL` | another server or model than the profile's |
| `EIDRA_PROFILE` | a profile directory instead of the built-in one |
| `EIDRA_HOME` | eidra's own directory instead of `~/.eidra` |
| `EIDRA_VAULT` | `off` leaves eidra's directory unencrypted, with no password |
| `EIDRA_IMAGE` | the image `scripts/eidra-docker` runs (default `eidra`) |
| `TERM`, `COLORTERM` | your terminal's, for colours and keys |

In the TUI, Docker's keys for detaching from a container are ctrl+^ then ctrl+q, not its default
ctrl+p ctrl+q, which would hold back the ctrl+p the TUI uses for its history.

It runs with `--user "$(id -u):$(id -g)"`, so the files the agent writes are yours, and HOME is
`/home/eidra`, which anyone can write to. It shares the host's network (`--network host`): radiance listens on the host's
`127.0.0.1:8000`, which a container on its own network cannot reach. To keep the agent off the
network entirely, except for the model, see the vacuum in docs/DESIGN.md §13 (`eidra relay` and
`--model-socket`); evals run that way.

The same thing without the script:

    docker run --rm -it --network host --user "$(id -u):$(id -g)" \
      -v "$PWD:$PWD" -w "$PWD" \
      -v "$HOME/.eidra:$HOME/.eidra" -e "EIDRA_HOME=$HOME/.eidra" \
      -v "$HOME/.cache/uv:$HOME/.cache/uv" -e "UV_CACHE_DIR=$HOME/.cache/uv" \
      -e RADIANCE_API_KEY \
      eidra

A project that needs its own toolchain (Rust, Node, a database) can use its own image instead, with
the binary mounted into it, since it is static:

    docker run --rm -it --network host --user "$(id -u):$(id -g)" -e HOME=/tmp -e RADIANCE_API_KEY \
      -v "$HOME/dev/eidra-cli/target/release/eidra:/usr/local/bin/eidra:ro" \
      -v "$PWD:$PWD" -w "$PWD" your-project-image eidra

On the host, without a container, it is just `target/release/eidra` in the project's directory.

### On Windows

With Docker Desktop and its WSL2 engine, the same image runs on a Windows host. Build it there from
a clone of this repository (`docker build -t eidra .`), or copy one over:
`docker save eidra | gzip > eidra.tar.gz` where it was built, then `docker load -i eidra.tar.gz`.

`scripts/eidra-docker` is bash, so a PowerShell function takes its place. Put it in your PowerShell
profile (`notepad $PROFILE`):

    $env:RADIANCE_API_KEY = "<key>"
    $env:EIDRA_BASE_URL   = "http://<model-server>:8000/v1"

    function eidra {
      $name = Split-Path -Leaf $PWD
      docker run --rm -it `
        -v "${PWD}:/workspace/${name}:ro" -w "/workspace/${name}" `
        -v eidra-data:/home/eidra/.eidra `
        -e RADIANCE_API_KEY -e EIDRA_BASE_URL -e COLORTERM=truecolor -e TZ=Africa/Johannesburg `
        --detach-keys "ctrl-^,ctrl-q" `
        eidra @args
    }

Then, in a project's folder, `eidra` starts a session, `eidra --continue` goes on with the last one
and `eidra sessions` lists them; any other arguments go to eidra as they are. The first start asks
you to choose the password that encrypts what the volume keeps (Encryption, below).

Where it differs from the Linux wrapper:
- The project is mounted read-only (`:ro`) at `/workspace/<folder>`, so the agent reads and
  explains the code but cannot change it. Leave out `:ro` to let it edit. The folder's name keeps
  each project's sessions apart.
- eidra's directory is a Docker volume, `eidra-data`, so sessions and checkpoints outlive the
  container; `docker volume rm eidra-data` deletes them.
- There is no `--network host`: the container reaches the model server at `EIDRA_BASE_URL`, on your
  network or over a VPN such as Tailscale.
- It runs as the container's root user, so git accepts the repository Windows mounts into it.
- `TZ` is your time zone, for the dates the agent reads and writes.

## The interactive session

`eidra` with no command starts a session in the current directory. Finished output goes into the
terminal's own scrollback; at the bottom are the reply streaming in, the input box and a status line
(model · effort · context used against the limit · cache reuse · prefill and decode speed).

- Enter sends; a message sent while the agent works waits for its turn to end. Ctrl+Enter
  interrupts the agent and sends at once. Shift+Enter, Alt+Enter or Ctrl+J add a newline.
- Esc interrupts the agent. Ctrl+C interrupts, or clears the input, or quits; Ctrl+D quits.
- Up and Down (or Ctrl+P and Ctrl+N) walk the history. Ctrl+T shows the model's reasoning whole
  instead of as one line.
- `@path` attaches a file to the message: an image as an image, a text file as `read` shows it.
  Tab completes the path.
- `!command` runs a command in the project; you see its output, and the agent gets it with your
  next message.
- A command on the confirm list asks first: y runs it once, a for the rest of the session, n not.
- `/help`, `/new` (a new session), `/compact` (go on from a summary of the conversation; words
  after it say what to keep), `/tasks` and `/stop` (subagents in the background), `/undo` and
  `/diff` (below), `/erase-unsecure` (below), `/reasoning`, `/quit`.

Ask the agent to do something in the background, such as "start a subagent to build a calculator
while we chat", and a subagent works on it while you go on talking; its report comes back to the
agent when it ends.

`eidra --continue` goes on with this directory's latest session, and `eidra --resume ID` with any
other; `eidra sessions` lists them.

Each turn is checkpointed in a git repository of eidra's own, apart from the project's:
`/undo` puts back the files the last turn changed (again for the turn before), and `/diff` shows
what changed since the last turn began (`/diff all`, since the session's first). A file you
changed after the turn is left alone.

Past 200K tokens the conversation is summarised and goes on from the summary, so a session does not
stop at the context window. If no summary can be made, it stops at the profile's limit (256K
tokens of the server's 262,144), and `/new` starts another.

eidra keeps what is its own in `~/.eidra`, or wherever `$EIDRA_HOME` says: an `AGENTS.md` of
yours, whose instructions go into every session, the session logs and the checkpoints. A session's
header names the `AGENTS.md` files it read: yours and the project's, or none. Each
session is logged as JSON lines under `~/.eidra/sessions`, or wherever `--session-dir` says, and
the path is printed when you quit.

### Encryption

`~/.eidra` is encrypted. The first time `eidra` starts, it asks you to choose a password, and
after that it asks for it each time it starts: it encrypts your sessions, your `AGENTS.md` and the
snapshots `/undo` uses, and without it none of them can be read again. What the agent saves while
it works, and the shell's files, stay in memory. A session's header says `encrypted`.

- To set or change your `AGENTS.md`, put a plain one in `~/.eidra`: the next start encrypts it and
  removes the plain one. `eidra instructions` prints it.
- Sessions from before the encryption stay unencrypted, and do not go on in it: `/erase-unsecure`
  lists them, and `/erase-unsecure yes` erases them, with their snapshots.
- `eidra exec` never encrypts `~/.eidra`, so scripts and evals run as before, but asks for the
  password once it is.
- `EIDRA_VAULT=off` leaves it unencrypted. The names of the files in `/undo` snapshots stay
  readable, and so do each session's id, time and directory.

docs/DESIGN.md §18 has the details.

## Unattended runs

    eidra exec "Make the tests pass, then finish."
    eidra exec --task-file task.md --json > events.jsonl

Progress goes to stderr and the model's summary to stdout; `--json` prints events as JSON lines
instead. The exit code tells you how it ended:

| code | meaning |
|---|---|
| 0 | finished |
| 1 | error |
| 2 | request limit reached |
| 3 | context limit reached |
| 4 | the model replied without finishing |
| 130 | interrupted |

`--confirm PATTERN` adds a command that needs approval, such as `git push`; an unattended run
declines it and tells the model why.

## Evals

`eidra eval run` runs a suite's tasks, each in its own container with the binary mounted and no
network but the model, then checks what the agent left. `report`, `compare`, `validate`, `regrade`
and `select` work on recorded runs and task sets. [evals/README.md](evals/README.md) has the task
format and the commands.

## Probe the prefix cache

    target/release/eidra probe                        # all five policies, about 40 s
    target/release/eidra probe --variant baseline,toggle-low --turns 8

The probe runs a scripted tool-calling session under five request policies and reports, for each
request, what the server's prefix cache supplied and whether the rendered prompt still began with
the previous one (docs/DESIGN.md §3). It asks for `qwen3.8-flash-next` through llama-swap, which
loads `qwen-assistant` if another model is loaded.

## Development

    scripts/cargo fmt --all
    scripts/cargo clippy --workspace --all-targets
    scripts/cargo test --workspace
    scripts/tui/offline.py       # the TUI end to end against a fake server, no GPU, about 20 s
    scripts/tui/offline.py --docker eidra   # the same, in the image through scripts/eidra-docker
    scripts/tui/smoke.py         # the TUI against radiance, about 30 s of GPU
