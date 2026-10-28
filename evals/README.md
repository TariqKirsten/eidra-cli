# evals

Tasks that measure eidra against the model it is tuned for, so every change to the harness or
the profile can be judged by numbers rather than by impression (docs/DESIGN.md §15).

    eidra eval validate                        # check the tasks themselves; uses no model
    eidra eval run --repeats 3                 # every task three times, recorded in evals/runs/
    eidra eval run --task ledger --repeats 1   # one task
    eidra eval run --profile my-profile/       # a profile variant
    eidra eval run --network host              # agents get the host's network (default: none, model only)
    eidra eval report evals/runs/<run>         # print a recorded run again
    eidra eval compare evals/runs/<a> evals/runs/<b>
    eidra eval regrade evals/runs/<run>        # rerun fixed checks on what the attempts left
    eidra eval run --resume evals/runs/<run>   # run a stopped run's lost and missing attempts
    eidra eval vacuum-check                     # check the sandbox itself, not the model

## How an attempt runs

1. The task's `workspace/` is copied to `evals/runs/<run>/<task>/<n>/work`.
2. A container from the task's `image` runs `eidra exec` on the prompt, with that copy at `/work`.
   - The container gets the running eidra binary mounted at `/usr/local/bin/eidra`, and no
     network: the agent reaches the model server through a relay's socket, and its commands reach
     nothing (`--network host` gives it the host's network instead).
   - It runs as you, with the API key passed through.
3. The checks run one after another, in another container with no network. They work on a copy
   of what the agent left, with the task's `check/` directory mounted at `/check`. The attempt
   passes when every check exits 0.

Each attempt keeps its session log (`sessions/`), its JSON events (`events.jsonl`), the
workspace as the agent left it (`work/`) and each check's output (`checks/`). `results.jsonl`
has one line per attempt.

## When something outside the agent fails

An attempt can say nothing about the agent: its image could not be made, or the model server
stopped answering while it ran. Such an attempt is **lost**: its line in `results.jsonl` gives the
reason under `lost`, and reports leave it out. Two things keep a long run from losing more than
it must:
- A fetch of a task's image that fails on the network is tried again, four times over about 15
  minutes.
- A watchdog asks the model server for one token every two minutes. When two requests in a row
  go unanswered for 150 s, the attempts running are stopped and lost, and none starts until the
  server answers again. After 30 minutes without an answer the run stops, exiting 2.

`--resume` takes a recorded run up again with its own suite, tasks, attempts, label, network and
checkers: it runs the attempts it is missing and again those it lost. Lost lines move to
`lost.jsonl` and their directories under `lost/`, and `results.jsonl` as it was is kept in
`results.before-resume.jsonl`. The agent binary and the profile must be the run's own. `--agent`
runs a frozen binary from a newer eval, and `--profile` names the profile directory when the
binary's built-in one is not the run's:

    git archive <commit> profiles/qwen3.8-radiance | tar -x -C /tmp/run-profile
    eidra eval run --resume evals/runs/<run> --agent evals/runs/bin/eidra-<commit> \
        --profile /tmp/run-profile/profiles/qwen3.8-radiance --remove-images

## Checking the vacuum

The vacuum (docs/DESIGN.md §13) is what stops a task's agent from reaching anything but the
model: no setting inside the container turns the network back on, because the network is not
there. `eidra eval vacuum-check` tests that, standing in for the agent. It runs a probe in the
same container an attempt gets and has it try every way out — DNS, the model server's own port,
the internet, making an interface or route, `unshare`/`nsenter`, a Docker socket, and pointing
eidra's `--base-url` at the server over the network. The probe exits non-zero if any worked.

    eidra eval vacuum-check              # none, disconnected, and a control, in turn
    eidra eval vacuum-check --mode none  # just the agent's --network none container
    eidra eval vacuum-check --out dir/   # keep the probe's per-mode logs

It checks both shapes of vacuum — a suite task's `--network none` container, and a Harbor task's
container disconnected from the bridge — and a `control` on the host's network, which *must*
find a way out: a probe that passed everywhere would prove nothing. The command exits non-zero
if any vacuum leaked or the control did not, so it belongs in CI.

## Writing a task

A task is a directory under `tasks/`:

| path | |
|---|---|
| `task.toml` | the task (below) |
| `workspace/` | what the agent starts with |
| `check/` | what the checks need: hidden tests, expected output, data the agent never sees |
| `solution/` | a reference solution, laid over `workspace/` by `eidra eval validate` |

```toml
id = "lru-fix"            # the directory's name
split = "train"           # "train" tasks are for tuning; "test" tasks are held out
image = "python:3.13-slim"
max_requests = 60         # passed to eidra exec
timeout_s = 600           # wall-clock limit for the agent
prompt = """
What the agent is asked to do.
"""

[[checks]]
name = "tests"
run = "rm -rf tests && cp -r /check/tests tests && python3 -m unittest -q"
timeout_s = 300           # optional
```

Some guidelines:

- **Restore what the agent must not weaken.** If the agent is told not to change the tests, a
  check restores them from `check/` before running them.
- **Check behaviour the agent could not see:** hidden tests, a second data set, edge cases. This
  is what separates a general solution from one fitted to the visible examples.
- **Keep copied-in files out of the way.** Files copied in for a check go to the project root,
  under names that unittest discovery skips (`hidden_checks.py`, `original_checks.py`), because
  checks share one copy of the workspace.
- **Check exactly what is specified.** Where a spec leaves wording open (an error message, say),
  the check accepts any wording.
- **Validate the task.** `eidra eval validate` must report `ok`: every check fails on the
  starting workspace, or the task tests nothing, and passes on the solution, or it cannot be
  done.
- **Keep the split honest.** Tune on train tasks only. Look at test tasks' results, never at their
  attempts, so that test stays a measure of whether tuning generalises.

## The suite

| task | split | what it asks |
|---|---|---|
| `ledger` | train | a five-module package from a spec; the report is checked on hidden data |
| `lru-fix` | train | a bug fix from a failing test; a second bug only the docstring describes |
| `todo-json` | train | two new CLI options, with docs and tests |
| `crash-trace` | train | a crash from a traceback, with a second crash behind it, and a test |
| `log-report` | test | a log summariser from a spec, run on logs it never saw |
| `rename-api` | test | a rename across a package, with a deprecated alias |
| `timeout-config` | test | one default changed in code, config, docs and tests, among decoys |

`ledger` comes from radiance's agent demo. The others were written for eidra.

## Harbor tasks

`eidra eval` also runs tasks in [Harbor's format](https://docs.harborframework.com/tasks/overview),
which is how Terminal-Bench and SWE-rebench publish theirs. A directory with an `instruction.md`
is one:

| path | |
|---|---|
| `instruction.md` | the prompt |
| `task.toml` | timeouts, resources, network, environment variables |
| `environment/Dockerfile` | the environment, unless `task.toml` names a prebuilt `docker_image` |
| `tests/test.sh` | the verifier: it writes a reward to `/logs/verifier/reward.txt` (or `.json`) |
| `solution/solve.sh` | the reference solution, for `eidra eval validate` |

An attempt runs as Harbor runs a task without a separate verifier:

1. The image is built from `environment/`, tagged `eidra-harbor/<task>:<hash of environment/>`
   and reused while `environment/` is unchanged. A prebuilt image is pulled.
2. A container of it starts with the task's CPU and memory limits and `environment.env`, the
   eidra binary and a sessions directory mounted. `eidra exec` runs in the image's working
   directory (or `environment.workdir`), as `agent.user`, for at most `agent.timeout_sec`. In a
   vacuum (the default), the container starts on Docker's bridge network and is disconnected
   from it before the agent runs, so the agent reaches only the model, through the relay's
   socket; it is connected again for the verifier, unless the task says `network_mode =
   "no-network"`.
3. `agent.patch` records what the agent changed, if the working directory is a git repository.
4. `tests/` is copied to `/tests` and `bash /tests/test.sh` runs in the same container, as
   `verifier.user` with `verifier.env`. `/logs/verifier` is copied to `checks/verifier/`. The
   attempt passes at a reward of 1.

When the agent runs out of time, its container is stopped (what the agent started cannot be
stopped otherwise) and the verifier runs in a new one made from a snapshot of its files. A task
whose image cannot be built counts as a failed attempt with an `error.log`, so one bad image does
not stop a long run. Harbor tasks have no split, and count as test. Not supported: multi-step
tasks, verifiers in a separate environment, docker-compose environments. `regrade` leaves Harbor
attempts as they were, since their containers are gone.

`evals/harbor-example/` holds one small task, for checking the adapter:

    eidra eval validate --suite evals/harbor-example
    eidra eval run --suite evals/harbor-example --repeats 1

### SWE-rebench

[SWE-rebench](https://swe-rebench.com) collects fresh GitHub issues every month, so tasks newer
than the model are ones it cannot have seen. Harbor's CLI downloads its task directories (text
only; the images come later, when tasks run):

    uvx harbor download "ibragim-badertdinov/swe-rebench-07-2026@2026-07" -o evals/harbor
    uvx harbor download "swe-rebench/swe-rebench-leaderboard@latest" -o evals/harbor

The first is the July 2026 leaderboard: 111 tasks from issues of 2026-05-15 to 2026-06-15, 20 of
them in Python. The second is 860 older Python tasks. `eidra eval select` links some of them into
a suite, newest first, using each task's `tests/config.json`:

    eidra eval select --from evals/harbor/swe-rebench-07-2026 --to evals/harbor/swe-july-python \
        --language python --exclude verl-project__verl-6506
    eidra eval run --suite evals/harbor/swe-july-python --repeats 1 --remove-images

eidra checks each edit with ruff and ty (docs/DESIGN.md §9), which task images rarely have.
`--tools DIR` mounts a directory of them into every attempt, named by `EIDRA_TOOLS` and kept off
the agent's own `PATH`. The runtime image has both:

    mkdir -p evals/runs/tools && id=$(docker create eidra)
    docker cp "$id:/usr/local/bin/ruff" evals/runs/tools/ && docker cp "$id:/usr/local/bin/ty" evals/runs/tools/
    docker rm "$id"
    eidra eval run --suite evals/harbor/swe-july-python --repeats 1 --remove-images --tools evals/runs/tools

Each task's image (`swerebenchv2/<task>`) is 0.4–6.5 GB compressed. `--remove-images` removes a
task's images once its attempts are done, and runs each task's attempts together so that each
image is fetched once. Building an image fetches uv. The verifiers parse test logs with plain
Python, but a few run pytest through `uv run`, which may fetch packages, so a verifier keeps the
network unless `task.toml` says `network_mode = "no-network"`.
