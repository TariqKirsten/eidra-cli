# eidra

A coding agent in Rust, tuned for one local model: Qwen3.8 Flash-Next served by
[radiance](https://codeberg.org/StillDeadcode/radiance). It speaks the OpenAI chat-completions API.

Status: M1. It has:
- `eidra exec`, which does one task unattended (the agent itself is in `crates/eidra-core`)
- `eidra probe`, which measures how well the server's prefix cache holds up
- an API client, in `crates/eidra-llm`

The design and the plan are in [docs/DESIGN.md](docs/DESIGN.md). Everything that is specific to
the model (prompts, tool descriptions, request settings) lives in
[profiles/qwen3.8-radiance](profiles/qwen3.8-radiance), so tuning means editing files there.

## Build

Only Docker is needed. `scripts/cargo` runs cargo in a pinned Rust image. The first time, it
builds a small dev image that adds clippy and rustfmt.

    scripts/cargo build --release        # static binary: target/release/eidra
    scripts/cargo test
    scripts/cargo clippy --all-targets -- -D warnings

For the release binary on its own:

    docker build --target export --output dist .    # dist/eidra

## Run a task

The binary is static, so the agent runs in whatever container holds the project's tools. Run it
like this, rather than on the host, so that its commands stay in the container:

    export RADIANCE_API_KEY="$(sed -n 's/^RADIANCE_API_KEY=//p' ~/radiance/.env | tr -d "\"'")"
    docker run --rm -it --init --network host --user "$(id -u):$(id -g)" -e HOME=/tmp -e RADIANCE_API_KEY \
      -v "$PWD/target/release/eidra:/usr/local/bin/eidra:ro" -v "$PROJECT:/work" -w /work \
      python:3.13-slim eidra exec "Make the tests pass, then finish."

Progress goes to stderr and the model's summary to stdout; `--json` prints events as JSON lines
instead. The exit code tells you how it ended:

| code | meaning |
|---|---|
| 0 | finished |
| 1 | error |
| 2 | request limit reached |
| 3 | context limit reached |

`--confirm PATTERN` adds a command that needs approval, such as `git push`. An unattended run
declines such commands and tells the model why. Each session is logged as JSON lines under
`~/.local/share/eidra/sessions`, or wherever `--session-dir` says.

## Probe the prefix cache

    export RADIANCE_API_KEY="$(sed -n 's/^RADIANCE_API_KEY=//p' ~/radiance/.env | tr -d "\"'")"
    target/release/eidra probe                        # all five policies, about 40 s
    target/release/eidra probe --variant baseline,toggle-low --turns 8
    target/release/eidra probe --json > probe.json

The probe runs a scripted tool-calling session, listing and reading a small generated Python
project, under five request policies. For each request it reports:
- what the cache supplied
- how much of the previous prompt was computed again
- whether the rendered prompt, from radiance's `/apply-template`, still began with the previous
  one

It asks for `qwen3.8-flash-next` through llama-swap. If another profile is loaded, llama-swap
loads `qwen-assistant` in its place.
