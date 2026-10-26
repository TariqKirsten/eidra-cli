# evals

Tasks that measure eidra against the model it is tuned for, so every change to the harness or
the profile can be judged by numbers rather than by impression (docs/DESIGN.md §15).

    eidra eval validate                        # check the tasks themselves; uses no model
    eidra eval run --repeats 3                 # every task three times, recorded in evals/runs/
    eidra eval run --task ledger --repeats 1   # one task
    eidra eval run --profile my-profile/       # a profile variant
    eidra eval report evals/runs/<run>         # print a recorded run again
    eidra eval compare evals/runs/<a> evals/runs/<b>
    eidra eval regrade evals/runs/<run>        # rerun fixed checks on what the attempts left

## How an attempt runs

1. The task's `workspace/` is copied to `evals/runs/<run>/<task>/<n>/work`.
2. A container from the task's `image` runs `eidra exec` on the prompt, with that copy at `/work`.
   - The container gets the running eidra binary mounted at `/usr/local/bin/eidra`, and the host
     network so that the agent reaches the model server.
   - It runs as you, with the API key passed through.
3. The checks run one after another, in another container with no network. They work on a copy
   of what the agent left, with the task's `check/` directory mounted at `/check`. The attempt
   passes when every check exits 0.

Each attempt keeps its session log (`sessions/`), its JSON events (`events.jsonl`), the
workspace as the agent left it (`work/`) and each check's output (`checks/`). `results.jsonl`
has one line per attempt.

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
