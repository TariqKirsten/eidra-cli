# eidra: design

Status, 2026-10-26: M0 and M1 are done, and M2 is under way. The probe confirmed R2–R5 on the
live server (§3.1), `eidra exec` completed the ledger task unattended (§16.1), and the first suite
runs are in §16.2.

## 1. What and why

eidra is a coding-agent harness written in Rust and tuned to get the most out of one model on one
server: **Qwen3.8 Flash-Next, served by radiance on this machine**. It speaks the OpenAI
chat-completions API, so other servers work too, but its defaults, prompts and tools are chosen
and measured against radiance.

It runs inside a Linux container with bash and the usual command-line tools as its main
interface. It has an inline terminal UI (like Claude Code) and a headless mode for evals. It
reads images and PDFs, supports Python out of the box (uv, ruff, ty) and reads AGENTS.md.

It is a personal tool first. It stays ready to open-source: no hardcoded paths or keys.

Not goals for now: other providers' native APIs, Windows or macOS, multi-arch builds,
embeddings or repo maps, and any security boundary beyond the container's.

## 2. The target, measured

| | |
|---|---|
| Endpoint | `http://127.0.0.1:8000/v1` (llama-swap in front of radiance); key in `RADIANCE_API_KEY` |
| Model | `qwen3.8-flash-next`, an alias of the llama-swap profile `qwen-assistant`: 225,280-token context, 4 sequences, vision |
| Architecture | hybrid linear-attention MoE on 2 × Radeon AI PRO R9700, tensor parallel 2 |
| Prefill | 2.2–4.3K tok/s, depending on the PCIe link (probe: 2.7–3.7K tok/s on 3–7K-token chunks) |
| Decode | ~110–130 tok/s for one stream on 512-token answers, and ~62–67 tok/s each with four at once (244 total). The probe saw 150–240 tok/s on short tool-call turns, where speculative decoding helps |
| Tool calls | Qwen3-Coder XML (`<tool_call><function=…><parameter=…>`), constrained to the tool's schema by a grammar, returned as OpenAI `tool_calls` |
| Reasoning | returned as `reasoning_content`. Efforts: `xhigh` (the template default), `medium`, `low`, or `none` for off. `thinking_token_budget` closes the block |
| Profile defaults | `reasoning_effort` low, thinking budget 2048, `max_tokens` 4096 |
| Per-reply stats | `timings`: `cache_n`, `prompt_n`, `prompt_ms`, `predicted_per_second`, `draft_n_accepted`, `ttft_ms` |
| Extras | `/apply-template`, `/tokenize`, `/v1/chat/completions/input_tokens`, a `session_id` label, `continue_final_message`, `tool_choice: "required"`, GBNF grammars on `/v1/completions`. Behind llama-swap, the non-OpenAI paths are reached at `/upstream/<model>/…` (the root path returns 404) |

Sources:
- `~/radiance/README.txt` and `~/radiance/LLAMA-SWAP.txt`
- `~/radiance/source/docs/GUIDE.md` (§4.8, §4.9, §5)
- the chat template at `~/radiance/source/tests/data/chat/qwen3.8/chat_template.jinja`, the same 8,952 bytes as the template the live server loaded
- `docker logs qwen-assistant`

## 3. Rule: an append-only, byte-stable prompt

Radiance resumes a prompt from a linear-state snapshot, and it takes one every 2048 tokens. When
anything early in the prompt changes, everything after that point is prefilled again. That costs
25–45 s per 100K tokens, and about 100 s near the full window. Only cache hits keep turns fast,
so the request builder follows these rules:

- **R1. Freeze these per session:** the system prompt text, the tool list (names, descriptions,
  schemas, order), the reasoning effort and any `chat_template_kwargs`. The template renders the
  tools into the system prompt.
- **R2. Effort is fixed per session; a single request is controlled only through the budget.**
  - The template writes `low` and `xhigh` instructions at the top of the system prompt.
  - `medium` and `none` render the same system prompt and differ only in the generation prompt
    at the end.
  - The sampler, not the prompt, applies `thinking_token_budget`.

  Sessions run at `xhigh` by default, the user's choice for Qwen3.8. At `xhigh`, a step can only
  be shortened, with the budget: switching it to `none` would drop the `xhigh` instructions from
  the top of the prompt. (A `medium` session could also turn single steps to `none` at no cost.)
  Effort is always sent explicitly, because the server's default, `low`, renders differently.
- **R3. Replay assistant turns exactly:** `content`, `reasoning_content` (verbatim, untrimmed),
  and `tool_calls` with the same ids and the `arguments` string byte for byte as received. This
  keeps the history's rendering stable. The reply itself is recomputed in the next request either
  way (§3.1), so leaving reasoning out entirely is just as cache-friendly, as long as it is left
  out every time.
- **R4. Never send `preserve_thinking: false`.** It drops earlier turns' reasoning when a new
  user message arrives, which rewrites the whole previous task.
- **R5. Never append a `system` message.** Radiance merges it into the first one. Reminders and
  late context (such as a nested AGENTS.md) go at the end of the newest tool result or user
  message.
- **R6. Never edit earlier messages.** Tool output is capped when it is produced, not trimmed
  later. Compaction starts a new conversation and pays one cold prefill.
- **R7. Store media bytes; don't regenerate them.** An image is encoded once into the session log
  and replayed from there.

How the rules are enforced:
- Requests are a pure function of the session log (§4.2), so R3 and R6 hold by construction.
- Integration tests render consecutive requests with `/apply-template` and check that each one
  starts with the previous one.
- The status line shows `cache_n` against the prompt size on every turn.
- `eidra probe` measures all of this against the live server.

### 3.1 Measured

`eidra probe`, 2026-10-26: six requests per policy over a session that grows from 4K to 17K
tokens.

| policy | rendered prompt | prompt tokens recomputed |
|---|---|---|
| effort `medium` throughout | only grows | 840 (at most 402 per request) |
| `medium` and `none` alternating | only grows | 1,261 (at most 453) |
| `medium` and `low` alternating | changes at byte 19 at every switch | 14,755: the first switch recomputes everything, each later one a turn's worth |
| reasoning not sent back | only grows | 1,269 (at most 457) |
| a `system` message appended before request 4 | changes once, where the system prompt ends | 4,436: that request recomputes everything after the system prompt |

What this shows about radiance:
- **Snapshot placement.** Snapshots sit every 2048 tokens. There is also one at the end of a
  prompt, rounded to 64 tokens, once the prompt runs about 512 tokens past the last snapshot.
  So an append-only request recomputes fewer than 512 tokens of the previous prompt. (The
  placement was inferred from the probe's numbers.)
- **Replies are never cached.** The next request computes them as part of its prompt. Whether
  reasoning is sent back therefore doesn't change the cache cost. It changes the prompt's size
  and what the model sees, which is an eval question (§17).
- **Each effort level keeps its own cached copy.** An occasional switch costs a full recompute,
  and alternating costs a turn's worth per request.
- **Time to first token** was 0.3–1.9 s up to 17K tokens while the cache held.

Server-side settings for `qwen-assistant`, applied 2026-10-26 in `~/radiance/llama-swap.yaml`:
- **40 snapshot slots.** At the old 24 GiB container limit, radiance could pin only 8 of the 40
  that `auto` wants for 4 × 225K. Subagents and other sessions then evicted a parent's snapshots.
  The profile now gets 37 GiB (`RADIANCE_MEM_GIB`).
- **An 8 GiB host tier** (`--prefix-cache-mib 8192`, about 574K tokens):
  - Without it, a cached conversation's VRAM counts as held and is never lent to the expert slab,
    so idle sessions slow decode.
  - With it, each finished turn is copied to RAM, and its VRAM goes back to the experts on demand.
  - A returning session is restored rather than prefilled again.
- **A 4 h expiry** (`--prefix-cache-ttl 14400`). Eviction under memory pressure is
  least-recently-used whatever the expiry is. The expiry drops sessions nobody has come back to.
- **Stay on `qwen-assistant`.** A request for another llama-swap profile unloads it, and the
  reload takes about 35 s and starts with a cold cache. The TUI reports this as "model loading".

## 4. Architecture

### 4.1 Crates

| crate | role | milestone |
|---|---|---|
| `eidra-llm` | OpenAI-compatible client: types, SSE streaming, turn accumulation, radiance extras. Knows nothing about agents | M0 |
| `eidra-core` | session log, agent loop, tools, confirmation policy, model profiles, context budget. Takes `Op`s, emits `Event`s | M1 |
| `eidra-tui` | inline ratatui frontend | M4 |
| `eidra` | the binary: `eidra` (TUI), `eidra exec` (headless), `eidra eval`, `eidra probe` | M0 on |

The core never prints anything. Frontends send `Op`s (a user message, an approval, an
interrupt). They render the `Event`s the core emits: reasoning and content deltas, tool calls
started and finished, approval requests and per-turn stats.

### 4.2 Session log

Each session is an append-only JSONL file at `~/.local/share/eidra/sessions/<id>.jsonl`.

- It starts with the frozen session header: profile hash, system prompt, tools and effort.
- Every message follows exactly as it was sent or received, with per-turn stats and the seed the
  request was sent with. Radiance is deterministic for a seed and a prompt, so any request can be
  sent again and give the same reply (§16.2).

The next request is built from the log alone. Resume, fork and replay read the same file.

### 4.3 Model profiles

Everything model-specific lives in a versioned directory, so tuning means editing files, not
recompiling:

```
profiles/qwen3.8-radiance/
  profile.toml   # endpoint, model, key env var, max_tokens, thinking policy, sampling
                 # overrides, context limits, tool-output caps, tool set
  system.md      # system prompt template, rendered once per session
  tools/*.toml   # name, description, parameter schema
```

Each eval run records a hash of the profile's contents. An experiment is a variant of a profile.

## 5. Tools

Guidelines for this model:
- **Flat schemas:** string and integer parameters only. The XML call format writes strings raw,
  with no JSON escaping, which suits code. Nested arrays and objects fit it less well.
- **Familiar names and shapes:** stay close to what open models are commonly trained on (Claude
  Code- and Qwen Code-style tools). Alternatives get tested with evals.
- **Plain-text results, capped when produced:** head and tail, with the full output saved to a
  file the model can read.
- **`max_tokens` on every request.** `finish_reason: "length"` is handled, either by resuming
  with `continue_final_message` or by asking for smaller edits.
- **Headless runs** use `tool_choice: "required"` with a `finish` tool. This doesn't affect the
  cache, because the template doesn't render `tool_choice`.

Initial set (M1):

| tool | parameters | notes |
|---|---|---|
| `bash` | `command`, `timeout_s?`, `background?` | §6 |
| `bash_output`, `bash_kill` | `id` | background jobs |
| `read` | `path`, `offset?`, `limit?` | text with line numbers; images as image parts; PDFs as text, with page images on request; `.ipynb` as cells |
| `edit` | `path`, `old`, `new`, `replace_all?` | exact match, which must be unique unless `replace_all`; tolerates a differing leading or trailing newline and CRLF endings; needs no read first |
| `write` | `path`, `content` | an existing file not read this session, or changed since, is shown instead of replaced, and the same call then goes through |
| `glob` | `pattern`, `path?` | gitignore-aware, in-process (`ignore`) |
| `grep` | `pattern`, `path?`, `glob?` | in-process (`grep-searcher`) |
| `todo` | `list` | a markdown checklist as one string, which keeps the schema flat |
| `finish` | `summary` | headless runs |

Later: LSP navigation (definition, references, hover), PDF page images and web fetch.

## 6. Bash

- **One `bash -c` per command**, with stdin from `/dev/null` and its own process group. The
  working directory and exported environment carry over between commands: the wrapper records
  `pwd` and `env -0` on exit, so `cd` and `source .venv/bin/activate` persist.
- **Non-interactive environment:** `PAGER=cat GIT_PAGER=cat GIT_EDITOR=true EDITOR=true
  PIP_NO_INPUT=1 DEBIAN_FRONTEND=noninteractive PYTHONUNBUFFERED=1 NO_COLOR=1`.
- **Timeouts:** 120 s by default, 600 s at most. On a timeout or Esc, the process group gets
  SIGTERM, then SIGKILL.
- **Background jobs** for servers and watchers: start one, read its output later, kill it.
- **Output:** stdout and stderr merged in order, ANSI codes stripped, and capped (head and tail).
  The full output is saved under the session directory.

## 7. Confirmation policy

Commands run freely unless they match the user's confirm list:

```toml
[confirm]
commands = ["git", "rm -rf", "docker"]   # "git" = every git command; "git push" = only pushes
```

Matching works on parsed commands, not raw strings:
1. Parse with tree-sitter-bash. Split pipelines, `&&`/`||`/`;` lists, subshells, and command and
   process substitutions.
2. Normalise each simple command:
   - drop `VAR=value` prefixes
   - unwrap `sudo`, `env`, `command`, `exec`, `nice`, `nohup`, `time`, `timeout N` and `xargs`
   - take the basename of argv[0]
3. Recurse into `bash -c` and `sh -c` strings.
4. A pattern matches when its words are a prefix of the normalised argv. For `git`, global
   options (`-C dir`, `-c k=v`, `--no-pager`, `--git-dir=…`) are skipped before the subcommand.
5. Anything that can't be analysed (`eval`, a variable as the command name) asks.

The prompt offers:
- allow once
- allow this exact command for the rest of the session
- always allow (adds an allow entry to the config)
- deny, with a note to the model

Headless runs deny and tell the model the command needs approval.

An optional second layer: shims on `PATH` for the listed programs ask eidra over a socket. This
catches calls from scripts, Makefiles and Python.

The policy guards against model mistakes. It is not a security boundary.

## 8. Context

- **Compaction threshold:** the window is 225K tokens, but quality and latency fall off earlier.
  The threshold is a profile setting, starting at 120K, tuned by evals.
- **Compaction:** summarise with a prompt that keeps decisions, open work and exact details, then
  start a new conversation with the summary. That costs one cold prefill, about 30–55 s at 120K.
- **Reasoning stays in history (R3)** and counts against the window. Evals decide the thinking
  budget.

## 9. Python

- **uv:** detect `pyproject.toml`, `uv.lock` or `.venv`, run `uv sync` with a cached volume, and
  pass the interpreter to the language server.
- **Language server and lint:** `ty server` for types and navigation; `ruff check` and
  `ruff format`. All Rust-native, with no Node. Languages are entries in a config registry, so
  adding others later is one entry each.
- **After each edit:**
  1. tree-sitter parse
  2. `ruff check`
  3. ty diagnostics, debounced, with a timeout
  4. append only the diagnostics this edit introduced
- **No formatting per edit,** because it would shift the text the model's next `old` refers to.
  Format at the end of a turn and tell the model.

## 10. Images and PDFs

- **Image limits:** `qwen-assistant` takes OpenAI `image_url` parts up to 2048 × 2048, about
  1,000 prompt tokens per 1024². Larger images are downscaled to fit. Don't go smaller, or small
  text gets misread.
- **Image input:** pasted or dragged file paths (via bracketed paste) and `@path` mentions attach
  images. `read` on an image returns it.
- **PDFs:** text per page range (`pdftotext -layout`), and page images (`pdftoppm`) on request.
- **Open question:** does radiance accept image parts in `tool` messages? If not, the image
  follows in a `user` message.

## 11. AGENTS.md

- The root `AGENTS.md` and `~/.config/eidra/AGENTS.md` go into the system prompt at session
  start.
- A nested `AGENTS.md` is appended to the first tool result that touches its directory (R5).

## 12. TUI (M4)

The TUI renders inline, like Claude Code, using ratatui's `Viewport::Inline` and
`Terminal::insert_before`. Finished cells go to the terminal's scrollback. The bottom region holds
the streaming tail, the input box and a status line: model, context used against the limit, cache
reuse, prefill and decode tok/s, and the current mode.

Keys:
- Enter sends; Shift+Enter or Ctrl+J adds a newline.
- Esc interrupts.
- `/` opens commands, `@` the file picker and `!` the shell.

Reference: codex-rs `tui`.

## 13. Container

- **Build:** the `Dockerfile` builds a static musl binary on `rust:1.99.1-alpine`.
  `docker build --target export --output dist .` writes it to `dist/`. For development,
  `scripts/cargo` runs cargo in the same image, so the host needs only Docker.
- **Runtime image (M7):**
  - Debian slim plus bash, git, ripgrep, fd, jq, curl, poppler-utils, uv, ruff, ty and tini, with
    the binary copied in.
  - A non-root user matching the host UID and GID; `--init`.
  - `--network host`, because the ufw rule admits only the LAN to port 8000.
  - Volumes for the workspace, `~/.local/share/eidra` and the uv cache.
  - `RADIANCE_API_KEY` passed at run time only.
  - `--detach-keys` changed so Ctrl-P reaches the TUI.
- **Host wrapper:** a script runs the container in the current directory.
- **TLS:** none yet, because radiance is plain http on localhost. The first https server brings
  rustls with the ring provider.

## 14. Sessions, checkpoints, observability

- **Sessions:** `--continue`, `--resume` and fork all work from the session log.
- **Checkpoints:** a shadow git repository per workspace, with its own `GIT_DIR` under
  `~/.local/share/eidra`, snapshots the tree after each turn. It catches changes made through
  bash too. `/undo` and `/diff` use it.
- **Observability:** `tracing` writes to a file. Optionally, OTLP traces go to the local
  Langfuse.

## 15. Evals

- **`eidra exec`:** headless, emits JSON events, sets an exit code, and uses
  `tool_choice: "required"` with `finish`.
- **`eidra eval run`** runs each task of a suite N times. Each attempt runs in its own container,
  with the static binary mounted and a fresh copy of the task's workspace. Checks then run with
  no network, on a copy of what the agent left, with the task's hidden `check/` directory
  mounted. Each attempt records:
  - pass or fail, per check
  - wall-clock time
  - requests, tool calls and tool errors
  - prompt, cached, computed, generated and reasoning tokens

  `eidra eval compare` shows two runs side by side, task by task. `eidra eval validate` checks the
  tasks themselves, without the model: their checks must fail on the starting workspace and pass
  on a reference solution. `eidra eval regrade` runs the current checks again on what a recorded
  run's attempts left, after a check is fixed. The format and the rules for writing tasks are in
  evals/README.md.
- **`eidra replay`** sends one request of a recorded session again. Radiance is deterministic for
  a seed and a prompt, so with the recorded seed it should reproduce the reply exactly; a reply
  that differs means the server computed something else. Other seeds show what else the model
  might have done at that step, and `--cold` empties the server's prefix cache first. It tests a
  change at one decision point in seconds, before a suite run tests it everywhere.
- **The suite** (M2) is seven tasks: four train, three held-out test:
  - building from a spec, with a hidden data set
  - a bug fix with a second, undocumented-by-tests bug
  - a feature with docs and tests
  - a crash with a second crash behind it
  - a log summariser checked on unseen logs
  - a cross-package rename with a deprecated alias
  - one default changed in code, config, docs and tests, among decoys

  Tuning looks only at train attempts; test results show whether the tuning generalises.
- **To add next:** harder tasks, since this suite saturates (§16.2).
  - Harbor-format tasks (`instruction.md`, `task.toml`, `environment/`, `tests/test.sh` writing a
    reward to `/logs/verifier/reward.txt`). The agent and the verifier run inside the task's own
    container. One adapter opens Terminal-Bench, SWE-bench and SWE-rebench.
  - SWE-rebench's monthly splits, newer than the model. Qwen3.8 recalls SWE-bench and
    Terminal-Bench environments (§16.2), so tasks it cannot have seen measure the harness, not
    memory. Their images are large, so each is pulled for its task and removed after.
  - tasks from the user's own repos
  - larger codebases, for M3's subagents to show what they are worth
- **Baselines on the same model:** Codex (`/v1/responses`), Claude Code (`/v1/messages`) and pi
  (chat completions).
- **Later:** radiance serves LoRA adapters, so successful trajectories could fine-tune the model
  on eidra's exact format.

## 16. Milestones

| | scope | done when |
|---|---|---|
| M0 | workspace, `eidra-llm`, `eidra probe`, Docker build | the probe confirms R2–R5 on the live server (done) |
| M1 | core loop, session log, tools, confirm policy, `eidra exec` | the ledger demo task passes headless (done) |
| M2 | `eidra eval`, first suite, baselines | a suite runs unattended and produces a report |
| M3 | subagents, measured against the M2 suite | the suite shows whether they help, and in which form |
| M4 | inline TUI | it's usable day to day |
| M5 | uv, ty, ruff, post-edit diagnostics | |
| M6 | images, PDFs, nested AGENTS.md, compaction, checkpoints | |
| M7 | runtime image and wrapper | |

Evals come first so that every later change, subagents included, is measured. Subagents share
radiance's four slots and its cache, so whether they pay off here is something to measure, not
assume (one agent did the ledger task in 66 s; radiance's five-agent demo took 118 s).

### 16.1 M1 acceptance run (2026-10-26)

**Setup.** The task was radiance's agent-demo ledger project: write five Python modules from
SPEC.md so that 36 standard-library tests pass. `eidra exec` ran with the built-in profile, as a
static binary inside `python:3.13-slim`.

**Result.** It finished in 66 s and 19 requests. An independent check confirmed:
- all 36 tests pass in a fresh container
- SPEC.md, models.py and tests/ are unchanged
- the CLI's output is byte-identical to the reference solution's on four invocations, including
  the error case

**Numbers:**
- 300,858 prompt tokens sent, 91% of them from the cache, so 26,682 were computed (8.2 s of
  prefill).
- 10,696 tokens generated, 3,799 of them reasoning.
- Decode ran at 160–227 tok/s, and time to first token was 0.1–1.0 s.

Generation is most of the wall time.

**Comparison.** Radiance's own demo used five agents (four in parallel) and took 118 s, with
18,775 tokens generated and 53% cache reuse. That run was on 2026-10-06, on an older radiance and
with a per-module task split, so it is a reference point, not a controlled comparison.

**First findings for tuning:**
- **Read-before-write refusals.** The model looked at the stub files with `cat`, then wrote over
  them. `write` refused, because only `read` counts as reading. That cost a request and about 1.2K
  tokens of regenerated file content. Candidates to measure:
  - count a refused file as read, and include its current content in the error
  - tell the model to look at files with `read`
- **Thinking budget.** Thinking hit the 2048-token budget once, in the planning step before
  writing all the modules. Measure whether a larger budget for that kind of step helps.
- **Short prompts.** A first prompt under 2048 tokens left nothing for the next request to reuse.
  It costs about half a second, once per session.

### 16.2 M2: first suite runs (2026-10-26)

Each run used the seven-task suite, one attempt per task:

| run | effort | passed | agent time | tokens generated | tool errors |
|---|---|---|---|---|---|
| first-pass | medium | 7/7 | 4.2 min | 40K | 11 |
| xhigh | xhigh | 7/7 | 15.6 min | 139K | 13 |
| file-tools | xhigh | 7/7 | 10.1 min | 95K | 3 |

xhigh's rename-api attempt first failed a check that was wrong. The check judged the README line
by line, and the agent's deprecation note had wrapped onto a second line. The check now reads
paragraphs, and `eidra eval regrade` re-ran it on both runs.

**The suite saturates.** Every attempt passes at both efforts, so the suite measures cost but not
quality. It cannot show what xhigh buys. Harder tasks are next (§15).

**xhigh costs 3.7 times the time.** Most of it is the ledger (80 s → 9.6 min). Its tests passed by
about request 19 at both efforts, but at xhigh the model kept probing edge cases (NaN, malformed
amounts) until the task's 80-request limit. Its hidden checks pass either way.

**Read-before refusals.** These are 22 of the 24 tool errors across the two runs. The model knew
the files from grep or cat, and its `old` strings matched, but only `read` counted as reading.
The change now waiting to be measured:
- `edit` needs no read, since `old` must match the file exactly
- `write` shows an unread or changed file instead of only refusing, and the same call then goes
  through

**Drift into an imagined task.** At request 25 of the xhigh ledger attempt (34K tokens), the
model's reasoning switched to another task. It planned a `pipeline.py` with init, add, delete and
report commands, in `/testbed` and `/app`, with a `task.yaml`. Those are SWE-bench's and
Terminal-Bench's layouts. The tool calls stayed on the ledger for 34 more requests. Then the model
wrote `/app/pipeline.py`, got "Permission denied", ran `ls /`, and went back to the task.

In a later run, log-report drifted from request 1. With only the system prompt and the task in
context, the model's reasoning began mid-thought about a database-backed `report.py`. By request 3
it was recalling a detailed history (patch scripts, digests, a tool error eidra never produces).
It spent 16 requests on that before reading SPEC.md.

That request had a recorded seed. Replayed with the cache emptied first, it gave the same reply,
bit for bit: the model produces the drift from the prompt. Radiance is not at fault:
- Every conversation radiance held was eidra's (`/sessions`), and none contained that content.
- Radiance is deterministic for a seed and a prompt, and a reply resumed from a snapshot is
  bit-identical to one computed from scratch.

It looks like recall of training trajectories. It appeared in 2 of the 14 xhigh attempts and none
of the 7 medium ones. Replaying log-report's first request with 30 new seeds, 1 drifted, again
starting mid-thought. Listing the workspace in the system prompt did not help: 1 of the same 30
seeds drifted, and the spec was read first in 28 replies against 23 without it, so the listing
was dropped. Errors for paths outside the workspace now name the workspace, which is what brought
the model back the first time.

**The file-tool change** (run file-tools, xhigh, one attempt per task): tool errors fell from 13 to
3. Two of the three were the new refusals, which showed the file. The third was an `edit` whose
`old` did not match, which is the cost of not requiring a read. It took one step, as the refusal
did. Times moved both ways (ledger 9.6 → 1.6 min, log-report 2.6 → 5.4 min with its drift), so at
xhigh, one attempt per task cannot show a time effect; that needs repeats.

## 17. Open questions

- **Keep or drop reasoning in history?** Both cost the same in cache (§3.1). Keeping it is the
  template's default and what the model was trained on; dropping it saves context. Evals decide.
- **The snapshot store under load:** the 40 slots are shared by every session. Measure them with
  subagents and anvil running, using radiance's `/metrics`
  (`radiance:prefix_hit_cut_tokens_total`, `radiance:prefix_cache_ckpt_evictions_by_reason_total`).
- **Images in tool messages:** does radiance accept image parts in `tool` messages?
- **Settings to tune with evals:** the compaction threshold, the thinking policy per kind of step,
  and tool-schema variants.
- **ty:** is it good enough on real projects?
