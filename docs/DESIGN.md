# eidra: design

Status, 2026-10-26: M0 and M1 are done. M2's runner and first suite are done; harder tasks and
baselines are next. M3's subagents are built and wait for the harder suite to judge them. M4's
inline TUI is built (§12). The probe confirmed R2–R5 on the live server (§3.1), `eidra exec`
completed the ledger task unattended (§16.1), the first suite runs are in §16.2, and the first
subagent run is in §16.3.

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
| Model | `qwen3.8-flash-next`, an alias of the llama-swap profile `qwen-assistant`: 262,144-token context (the model's trained length; 225,280 until 2026-10-26), 4 sequences, vision |
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
  that `auto` wants for 4 full-length sessions. Subagents and other sessions then evicted a
  parent's snapshots.
  The profile now gets 37 GiB (`RADIANCE_MEM_GIB`).
- **An 8 GiB host tier** (`--prefix-cache-mib 8192`, about 574K tokens):
  - Without it, a cached conversation's VRAM counts as held and is never lent to the expert slab,
    so idle sessions slow decode.
  - With it, each finished turn is copied to RAM, and its VRAM goes back to the experts on demand.
  - A returning session is restored rather than prefilled again.
- **A 4 h expiry** (`--prefix-cache-ttl 14400`). Eviction under memory pressure is
  least-recently-used whatever the expiry is. The expiry drops sessions nobody has come back to.
- **Stay on `qwen-assistant`.** A request for another llama-swap profile unloads it, and the
  reload takes about 35 s and starts with a cold cache. The TUI shows the wait as "waiting for
  the model".

## 4. Architecture

### 4.1 Crates

| crate | role | milestone |
|---|---|---|
| `eidra-llm` | OpenAI-compatible client: types, SSE streaming, turn accumulation, radiance extras. Knows nothing about agents | M0 |
| `eidra-core` | session log, agent loop, tools, confirmation policy, model profiles, context budget. Takes `Op`s, emits `Event`s | M1 |
| `eidra-tui` | inline terminal frontend, drawn with crossterm | M4 |
| `eidra` | the binary: `eidra` (TUI), `eidra exec` (headless), `eidra eval`, `eidra probe` | M0 on |

The core never prints anything. Frontends send `Op`s (a user message, an approval, an
interrupt). They render the `Event`s the core emits: reasoning and content deltas, tool calls
started and finished, approval requests and per-turn stats.

### 4.2 Session log

Each session is an append-only JSONL file at `~/.eidra/sessions/<id>.jsonl` (§14).

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
| `bash_output`, `bash_kill` | `id`, `wait_s?` | background jobs; `wait_s` waits for new output or the end |
| `read` | `path`, `offset?`, `limit?` | text with line numbers; images as image parts; PDFs page by page, each as its text layer and its image (§10); `.ipynb` as cells |
| `edit` | `path`, `old`, `new`, `replace_all?` | exact match, which must be unique unless `replace_all`; tolerates a differing leading or trailing newline and CRLF endings; needs no read first |
| `write` | `path`, `content` | an existing file not read this session, or changed since, is shown instead of replaced, and the same call then goes through |
| `glob` | `pattern`, `path?` | gitignore-aware, in-process (`ignore`) |
| `grep` | `pattern`, `path?`, `glob?` | in-process (`grep-searcher`) |
| `todo` | `list` | a markdown checklist as one string, which keeps the schema flat |
| `finish` | `summary` | headless runs |

Later: LSP navigation (definition, references, hover) and web fetch.

### 5.1 Subagents (M3)

`task` hands a self-contained piece of work to a subagent. Unattended runs, the evals among them,
go without it until the evals show where it helps (§16.3); adding `task` to `[tools] enabled` turns
it on there. Interactive sessions get it through `[tools] interactive`, with one more parameter,
`background` (below).

- **A subagent is a fresh, unattended agent** in the same workspace, with its own session log.
  The log's header names the parent session and the task. It gets the profile's tools, or
  `[subagents] tools`, but never `task`, so subagents do not nest.
- **Only its summary comes back.** The subagent ends with `finish`, and its summary is the
  `task` call's result. If it stops at its request limit (`[subagents] max_requests`) or its
  context limit, the result is an error saying so, with a warning that files may have changed.
- **Side by side.** The `task` calls in one reply run together, at most `max_parallel` at once:
  three, since radiance serves four and something else may be using it. Results go back in call
  order, because Qwen's tool-result format does not carry call ids. Subagents share the
  workspace, so the tool's description tells the model to give each its own files.
- **Cache.** A subagent's system prompt is the parent's with the `{{mode}}` section replaced, and
  the same date, so it starts as the parent's does. The shared part is shorter than one
  2048-token snapshot, though, so a subagent's first request is computed whole: about 3K tokens,
  under a second.
- **Approvals.** A subagent cannot ask the user. A command that needs approval is declined with a
  note asking it to name the command in its summary, so that the parent can run it.
- **Frontends** see a subagent's events wrapped as `subagent` events, labelled with the task's
  description. Eval metrics add subagents' requests, tokens and tool calls to the attempt's totals,
  and count them.

**In the background** (interactive sessions; the user asked for it on 2026-10-26, to have a
calculator built "while we chat"):
- A `task` call with `background` returns at once, saying the subagent has started. The frontend
  runs the subagent while the user and the agent go on talking (`Frontend::background`); a
  frontend that cannot hands it back, and it runs within the call as above.
- At most `max_parallel` run in the background at once, each with `background_max_requests`
  (150) rather than `max_requests`: it is a whole task, not a piece of a turn.
- When one ends, its report reaches the agent as a message of its own, worded by
  `[prompts] task_report` and appended like any other (R5, R6). It goes at once if the agent is
  idle, so that the agent tells the user; at the end of a turn that is running; or, after an
  interrupt, with the user's next message.
- The TUI shows its steps under its label and counts it in the status line; `/tasks` lists them
  and `/stop` stops them. `/new` stops the old session's, and their reports go nowhere.

The first run was the user's own request, against Qwen. It started the subagent in the background
and answered a question while it worked, as intended. But at 40 requests the subagent ran out
before testing the 12 modules it wrote, and on that unfinished report the agent finished the task
itself, in a long turn nobody had asked for. Hence the separate request limit, and the report's
last line, which asks the agent to tell the user how it went and to ask before taking unfinished
work up.

## 6. Bash

- **One `bash -c` per command**, with stdin from `/dev/null` and its own process group. The
  working directory and exported environment carry over between commands: the wrapper records
  `pwd` and `env -0` on exit, so `cd` and `source .venv/bin/activate` persist.
- **Non-interactive environment:** `PAGER=cat GIT_PAGER=cat GIT_EDITOR=true EDITOR=true
  PIP_NO_INPUT=1 DEBIAN_FRONTEND=noninteractive PYTHONUNBUFFERED=1 NO_COLOR=1`.
- **Timeouts:** 120 s by default, 600 s at most. On a timeout or Esc, the process group gets
  SIGTERM, then SIGKILL. `timeout` is taken for `timeout_s` (the model wrote it 23 times in the
  July baseline, and it was ignored), a value over 600 runs with 600 and the result says so, and
  a default timeout says how to raise it.
- **Background jobs** for servers and watchers: start one, read its output later, kill it.
  `bash_output` names the job's command and how long it has run, and `wait_s` waits up to 600 s
  for new output or the job's end. In the July baseline the model waited for jobs with `sleep`
  instead: up to 971 s of one attempt, polling a job whose output a `| tail` held back.
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

- **Context limit:** sessions stop at 256K tokens (`[context] limit`; 160K until 2026-10-26),
  subagents' too. The server's window is 262,144 tokens, the model's trained length: raised from
  225,280 the same day, with no RoPE scaling. It took no more memory: the KV pool is what the
  experts leave either way (1.6 full-length sessions at once, about 417K tokens across them), and
  radiance still keeps 40 snapshots. A near-full retrieval test passed (§16.4). Every request still
  asks for `max_tokens` 32,768: past about 229K of prompt radiance cuts it to the room left rather
  than refusing, so replies near the end get shorter, and one cut off at the window ends with
  `finish_reason: "length"` (§5). A prompt that does not fit at all is refused with
  `context_length_exceeded`, and that ends the session as reaching the limit does.
- **Compaction** (M6, 2026-10-27): once a reply leaves the conversation past
  `[context] compact_at` (200K), the next request asks for a summary instead: the whole
  conversation, then the profile's `compact` prompt. That prompt asks for the task and the user's
  words, what was done and decided, the files changed, what is left, and every exact detail
  needed to go on. It also asks for no tool calls, and any the reply makes are ignored: the
  request keeps its tools and `tool_choice: "auto"`, because radiance takes the tools out of the
  prompt for `"none"`, which would compute the whole conversation again. The conversation then starts again: the same system prompt (so
  the server's cache still has it), then a user message with the summary (`compacted`). The
  session log keeps both conversations, with a `compaction` record between them, so replay and
  resume read the last one. Subagents compact the same way.
  - The summary request is a prefix hit, since it only adds a message; the new conversation
    costs one cold prefill of the summary, a few seconds.
  - Interrupting it leaves the conversation as it was. A summary that comes back empty leaves the
    conversation to run on to the limit, and a summary request the server cannot fit ends the
    session there.
  - 200K is a first guess: quality and latency fall off before the window's end, so evals will
    tune it, from around 120K.
- **`/compact [what]`** (2026-10-27) compacts on demand in the TUI, in a turn of its own that Esc
  stops: the same request and new conversation, with `compact_focus` added to the prompt when
  the user names what the summary should keep. Before the first reply there is nothing to
  compact. An interrupted `/compact` does not mark the last reply as cut short, and an empty
  summary leaves automatic compaction on. The status line's context goes to 0 until the next
  request, since the last reply's numbers were those of the conversation that went.

  Tested live with `compact_at` at 14K on a task that read two source files whole and then
  answered three questions about them: the summary request found 15,168 of its 19,058 tokens in
  the cache, the summary took 1,182 tokens and named the answers already found and the read still
  to do, and the agent did that read and answered all three right from the new conversation.
- **Reasoning stays in history (R3)** and counts against the window. Evals decide the thinking
  budget.

## 9. Python

Built in M5 (2026-10-26):
- **After each edit or write** of a `.py` or `.pyi` file, eidra runs the profile's checks
  (`[diagnostics] python`) on the file before and after the change. The tool's result gains only
  the problems the change introduced: a problem that was there before is the project's, not the
  agent's. Problems are compared without their line numbers, since lines move.
  - `ruff check`, on the content through stdin, as the project configures it, with no fixes and no
    cache left behind.
  - `ty check`, on the file on disk, with the project's `.venv` if it has one. Unresolved imports
    are left out, since they depend on ty finding the environment, and so are undefined names ruff
    already reported.
  - A parse with tree-sitter's Python grammar, when ruff is not there: syntax errors need no tool.
- **Each check has a timeout** (`timeout_s`, 10 s), and one whose tool cannot be found is left
  out. Tools are looked for in `$EIDRA_TOOLS`, the project's `.venv/bin`, then `PATH`. Task images
  rarely have ruff or ty, so `eidra eval run --tools DIR` mounts a directory of them into every
  attempt, off the agent's own `PATH`.
- **Languages are entries in `[diagnostics]`**: adding one is a key and the code for its checks.
- **A change to the project's tests is pointed out** (2026-10-27): an edit or write to a Python
  test file that alters or removes a test function of the last commit (`git show HEAD:`) gets the
  profile's `changed_tests` note, naming them. Tests the session added do not count, nor helpers,
  imports or blank lines. It only says so: some tasks do ask for a tested behaviour to change. In
  the July baseline three of six failures were the agent rewriting existing tests until its
  change passed them (§16.5).

Not done yet:
- **Formatting at the end of a turn.** It rewrites whole files in projects that do not use ruff's
  formatter, which makes noisy diffs, so it waits for a measurement.
- **`uv sync` on its own:** `uv run` syncs the environment before it runs anything.
- **Navigation** (definition, references, hover) through `ty server`.

## 10. Images and PDFs

Built in M6 (2026-10-27):
- **`read` shows images.** PNG, JPEG, GIF and WebP go to the model as image parts of the tool's
  result: as they are if they fit in 2048 × 2048, the largest radiance takes, or else as a smaller
  copy (a JPEG stays a JPEG; anything else becomes a PNG). Each costs about 1,000 prompt tokens
  per 1024². Radiance takes image parts in `tool` messages (checked 2026-10-26: shown a green
  square that way, the model said so), so a tool's result carries its own images.
- **`read` gives a PDF page by page**, five at a time (offset and limit count pages, at most ten):
  - each page's text layer (`pdftotext -layout`), for copying exact values;
  - then the page as an image (`pdftoppm`, 1,600 px on the long side), for headings, tables,
    figures and layout, which plain text loses.

  A page without a text layer is marked as a scan, to be read from its image. Without poppler the
  result says what is missing, and without `pdftoppm` alone it gives the text.
- **Images attached to a message:** in the TUI, an `@path` mention, or a path pasted or dropped
  into the input box (quoted, with escaped spaces, or as a `file://` URL), attaches the image after
  the message's text. A plain word such as `photo.png` in a sentence is left alone.
- **R7 holds:** an image is encoded once into the session log and replayed from there.

Measured on 2026-10-27, with an invoice PDF, an image-only scanned receipt and a chart PNG: the
agent read all three with `read` and answered every question right in 48 s. Without this, the same
task took it about twenty shell commands, installing Pillow and an OCR library and analysing
pixels.

## 11. AGENTS.md

- The root `AGENTS.md` and `~/.eidra/AGENTS.md` go into the system prompt at session start.
- The TUI's header names the ones the system prompt holds, so that the user can see theirs were
  read (2026-10-27): `instructions: ~/.eidra/AGENTS.md, AGENTS.md`, or `instructions: none (no
  ~/.eidra/AGENTS.md, no AGENTS.md here)`; `/new` says it again. The list is read back from the
  session's system prompt, so a resumed session shows those it began with.
- A nested `AGENTS.md` (M6, 2026-10-27) is appended to the first tool result that touches its
  directory or one below it, headed with where it came from, and never again in that session
  (R5). `read`, `edit`, `write`, `glob` and `grep` touch the directory of the path they name;
  `bash`, the directory the shell is in after the command. Files between the workspace and that
  directory come outermost first, each cut at 8,000 characters. The prompt grows only for the
  parts of the project the agent works in.

## 12. TUI (M4)

`eidra` with no command starts an interactive session in the current directory. It takes exec's
options (`--profile`, `-C`, `--base-url`, `--model`, `--session-dir`, `--confirm`) and refuses to
start without a terminal. The session uses the profile's interactive prompt and has no `finish`
tool: a reply without tool calls ends the turn.

**Rendering.** The TUI renders inline, like Claude Code, and draws with crossterm alone. ratatui
was the plan, but ratatui-core 0.1.2 fixes an inline viewport's height when it is created, and the
live region here grows and shrinks.
- Finished output goes into the terminal's own scrollback, where the terminal scrolls, selects and
  searches it.
- A live region at the bottom is redrawn in place: move to its top, clear everything below, write
  the newly finished lines, then the region again. Each draw is one synchronized update.
- Between draws the cursor rests at the region's top-left, so a draw starts there without knowing
  where on screen the region is. That also survives a terminal reflowing its contents on resize.
- eidra wraps everything itself, measured with unicode-width, so it knows exactly how many rows a
  draw takes. Live rows are wrapped one column short of the width, so the terminal never wraps
  them, and the region stays shorter than the terminal, so its top is always on screen.
- Raw mode, bracketed paste, and the kitty keyboard protocol's unambiguous keys where the terminal
  has them, so that Shift+Enter and Ctrl+Enter are not Enter. Exiting or panicking restores the
  terminal.
- Control characters in model or tool output are never written to the terminal.

**The live region**, top to bottom:
- the streaming tail: the reply as markdown, or its reasoning, dimmed, until the reply starts
- a busy line: spinner, what the agent is doing, the turn's time, "esc to interrupt"
- messages queued for the end of the turn
- a command waiting for approval
- the input box, under a rule
- the status line: model · effort · background tasks running and reports waiting, if any · context
  used/limit · cache % · prefill and decode tok/s, all from the last request

**The scrollback** holds the user's messages, then for each request:
- its reasoning as one line, "∴ thought for N tokens", or whole after Ctrl+T
- its text as markdown, with the model's line breaks kept
- each tool call in one line, and its result summarised and dimmed (red for an error)
- todo lists as checklists, subagents' steps under their task's label, and notices
- a background task's end, with its report summarised (§5.1)

A step's reasoning, calls and results stay together; a blank line separates everything else.

**Keys:**
- Enter sends. While the agent works, the message is queued and sent when the turn ends; the
  input box then reads "Queue message".
- Ctrl+Enter (2026-10-27) interrupts the turn and sends the message as soon as it has stopped,
  after anything queued; with no turn under way it is Enter. Only a terminal with the kitty
  protocol tells it from Enter; elsewhere it arrives as Enter or Ctrl+J.
- Shift+Enter, Alt+Enter or Ctrl+J add a newline.
- Esc interrupts. Ctrl+C interrupts when the agent works, else clears the input, else quits.
  Ctrl+D quits when the input is empty.
- Up and Down (or Ctrl+P and Ctrl+N) move between rows, and walk the history from the first or
  the last.
- Ctrl+T shows reasoning whole or as one line.
- Tab after an `@path` completes it from the workspace's files: all of it if one file fits (a
  directory gets its `/`), else as far as they agree, listing them under the input box. Anywhere
  else Tab is four spaces.
- An approval takes y (once), a (this command, for the rest of the session) or n. Keys that arrive
  in the first 0.4 s are ignored, so that typing meant for the input box does not answer it.
- Commands: `/help`, `/new`, `/compact [what]` (§8), `/tasks` and `/stop [n]` for background
  tasks, `/undo` and `/diff [all]` (§14), `/reasoning`, `/quit` (or `/exit`). Quitting stops the
  turn and the background tasks first, and waits for an undo under way.

**What a message brings along** (M8, 2026-10-27):
- **`@path` to a text file** attaches it after the message, as `read` shows it, numbered lines
  and all, in the profile's `attached_file` wrapping (images go as images, §10; PDFs are left for
  `read`). It goes through the agent's own `read`, so the model has seen the file, and a nested
  AGENTS.md comes with it the first time.
- **`!command`** runs a command in the workspace, in a bash of its own (its `cd` and exports do
  not carry over, to the agent's shell or the next `!`), for up to ten minutes; Esc or Ctrl+C
  stops it and everything it started, and messages queued behind it go back to the input box.
  The scrollback shows its first 10 and last 30 lines and how
  it ended. The agent hears of it with the next message, which starts with the command, how it
  ended and its output, cut to `[tools] output_limit` (`[prompts] user_command`). It does not
  start a turn: it is context for the next message. It can run while the agent works; messages
  sent while it runs wait for it.
- Notes come first in the message, in order: `interrupted`, then what `/undo` put back and what
  `!` commands printed, then background reports, then the user's text, then attached files.

**Interrupts.** The agent runs in a future that owns it and hands it back when the turn ends; it
sends events and approval requests to the UI loop through a channel. Esc stops the run:
- A reply cut off mid-stream leaves nothing in the log. The next message starts with the profile's
  `interrupted` note. Without it the model saw its last request still unanswered and took it up
  again: in the first smoke test it finished an interrupted count to 400 before the next task.
- A tool call cut off gets an "interrupted" result, as do the calls after it, and a foreground
  command's whole process group is stopped.
- Messages queued during the turn go back to the input box, unless Ctrl+Enter stopped it: then
  they are sent at once.

**Checks.** Beyond the unit tests, two scripts drive the binary in a pseudo-terminal and read its
screen with a small terminal emulator:
- `scripts/tui/offline.py` runs it against a fake server that streams scripted replies: typing and
  commands, a reply with the status line's numbers, tool calls, an approval, an interrupt, a queued
  message and one sent at once with Ctrl+Enter, a background task worked on during the chat, attachments, PDF pages, `/undo` and
  `/diff`, `--continue`, `!` commands and `@` mentions, and SIGTERM. About 25 s, no GPU; run it
  after any change to the TUI. `--docker IMAGE` runs the same in the image (§13).
- `scripts/tui/smoke.py` does much the same against radiance, and checks that the model hears a
  `!` command and an `@` file, and that `--continue` goes on with the conversation: about 25 s of
  GPU (21 s on 2026-10-27). It refuses to run unless qwen-assistant is already loaded, since
  asking for it would unload whatever else is.


Reference: codex-rs `tui`.

## 13. Container

- **Build:** the `Dockerfile` builds a static musl binary on `rust:1.99.1-alpine`.
  `docker build --target export --output dist .` writes it to `dist/`. For development,
  `scripts/cargo` runs cargo in the same image, so the host needs only Docker.
- **Runtime image (M7, done 2026-10-27):** the `Dockerfile`'s last stage.
  - Python 3.13 on Debian slim, with bash, git, ripgrep, fd, jq, curl, poppler-utils and tini, uv,
    ruff and ty at pinned versions (build arguments, raised together on purpose), and the binary.
  - It runs as the host's user (`--user` with the host's UID and GID), so the agent's files are
    the user's; HOME is a directory anyone can write to. tini is PID 1, so signals reach eidra and
    orphaned processes are reaped.
  - `--network host`, because radiance listens on the host's localhost and the ufw rule admits
    only the LAN to port 8000. Or a vacuum (below).
  - `RADIANCE_API_KEY` is passed at run time only, never stored in the image.
- **Host wrapper:** `scripts/eidra-docker` runs the image in the current directory, mounted at the
  same path so that paths read the same inside and out: the TUI with no arguments, or any eidra
  command. It mounts `~/.eidra` (the user's AGENTS.md, sessions and checkpoints), the uv cache,
  `~/.gitconfig` read-only, `/etc/localtime`, and `$EIDRA_PROFILE` if set, and passes `TERM` and
  `COLORTERM`. Docker's detach keys become ctrl+^ ctrl+q: its default, ctrl+p ctrl+q, holds back
  every ctrl+p, which the TUI uses for the history. `scripts/tui/offline.py --docker eidra` runs
  the TUI's scenarios in the image through the wrapper; all eleven pass.
- **Vacuum:** the agent gets no network at all except the model. Its container runs with
  `--network none`, and eidra reaches the model through a Unix socket mounted into it
  (`EIDRA_MODEL_SOCKET`, or `--model-socket`). Outside the container, `eidra relay` forwards that
  socket to the profile's host and port, and to nothing else. Commands the model runs find no
  DNS, no route and nothing on loopback, so they cannot look things up, fetch a fix or install
  packages. Anything inside can still talk to the server through the socket, which is the one
  thing allowed. Evals run this way by default (`eidra eval run --network host` opts out).
  - The vacuum is the container's network namespace, not a setting the agent can reach: there is
    no flag, variable or file inside that turns it back on, because the network is simply not
    there to turn on. `eidra eval vacuum-check` tests that claim. It runs a probe
    (`eval/vacuum_probe.py`) in the very container an attempt's agent gets, and the probe tries
    every way out we know of — DNS, the server's own port, the internet over TCP and UDP, making
    an interface or a route, `unshare`/`nsenter` into another namespace, a Docker socket or CLI,
    pointing eidra's own `--base-url` at the server over the network — and exits non-zero if any
    worked. It checks both shapes of vacuum (`--network none`, and a Harbor container
    disconnected from the bridge) and a `control` on the host's network, which must report a
    leak, so that a probe passing everywhere cannot pass for a sealed vacuum. The relay stays a
    pipe to one server, never a proxy: a request naming another host is still answered by the
    model server (tested in `relay.rs`).
- **TLS:** none yet, because radiance is plain http on localhost. The first https server brings
  rustls with the ring provider.

## 14. Sessions, checkpoints, observability

- **eidra's own directory** (2026-10-27) is `~/.eidra`, or `$EIDRA_HOME`, and `.eidra` in the
  workspace for a user without a home. It holds the user's `AGENTS.md` (§11), `sessions/` and
  `checkpoints/`: one directory rather than the XDG split of `~/.config` and `~/.local/share`, so
  that a container keeps all of it with one mount or one volume (the user's choice).
- **Sessions** (M8, 2026-10-27): `eidra --continue` (`-c`) goes on with this directory's latest
  interactive session, `eidra --resume ID` (`-r`) with any session by its id or the start of it,
  and `eidra exec` takes both for unattended sessions; `eidra sessions` lists this directory's
  (`--all`, every directory's), the most recently active first, with the first thing the user
  said.
  - A resumed session goes on in its own log and its own workspace, with its system prompt and
    tools as they were: they are the start of every request, so the server's cache can still hold
    the conversation (radiance keeps it for four hours). The rest of the profile is the current
    one, and the TUI says so if it changed. The log gets a `resumed` record.
  - The conversation goes on from its last compaction. Calls left without results, by a crash or
    a kill, get results that say eidra stopped before them, and if the last turn did not end, the
    next message starts with the `interrupted` note. Request numbers, and so seeds, go on from
    where they were, and the shell starts in the directory, with the exports, its last command
    left. A log line cut short by a crash is cut off.
  - The TUI shows the conversation again, its last 80 messages, as they were shown at the time,
    and finds the session's checkpoints, so `/undo` still works.
  - A subagent's session cannot be resumed, nor an unattended one in the TUI or the other way
    round, since the tools and the system prompt differ. A log cut off in the middle of a line by
    a crash is read up to that line.
  - Not done: fork (a new session from a point in an old one).
- **Checkpoints** (M6, 2026-10-27): in an interactive session, a git repository of eidra's own
  records the workspace before and after each turn. It is bare, one per workspace, under
  `~/.eidra/checkpoints`, and runs with the workspace as its work tree and an index
  per session, so it never touches the project's repository, index or branches, and sees what
  commands changed as well as what the file tools did. Each snapshot is a commit on
  `refs/eidra/<session>` whose subject names its turn (`before 3: <message>`, `after 3`,
  `undid 3`), so a resumed session finds its turns again.
  - **What it leaves out:** whatever the project's ignore files and the user's global one ignore;
    `.venv`, `node_modules`, `__pycache__` and other caches, and a root `target/`; and any new file
    over 8 MiB, for good. The home and root directories get no checkpoints, and neither does a
    workspace with over 20,000 new files at once. A nested git repository is recorded as its
    commit, not its files.
  - **`/undo`** puts back what the latest turn that changed files changed: files it modified or
    deleted come back as they were before it, and files it made are removed, with any directories
    it made for them. A file changed again after the turn, by the user or anything else, is left
    as it is and listed, and so is one with something changed above or below it (a file that
    became a directory, or the other way), which putting it back would clear. A file new to the
    snapshots is removed only if the ignore rules put back do not ignore it (a turn that replaced
    a `.gitignore` makes old ignored files look new), and never through a symbolic link, which
    could lead out of the workspace. Undo again for the turn before. The agent's next message starts with a
    note of what was undone (`[prompts] undone`); without it the model believes its changes are
    still there. The conversation itself stays as it was: the model keeps what it tried, and the
    prompt stays append-only.
  - **`/diff`** shows what changed since the last turn began, a summary of files and then the
    patch, coloured; `/diff all`, since the session's first turn began. During a turn it shows
    the turn's changes so far.
  - A git command that fails or takes over 20 s turns checkpoints off for the session, with a
    notice; the turn goes on. Background subagents' changes count towards whichever turn they
    happen in.
  - Snapshots cost a few git commands per turn, about 10 ms each on a small project. A session's
    snapshots are let go 30 days after its last one, when the next session in that workspace
    starts, and `git gc --auto` collects them in the background.
- **Observability:** `tracing` writes to a file. Optionally, OTLP traces go to the local
  Langfuse.

## 15. Evals

- **`eidra exec`:** headless, emits JSON events, sets an exit code, and uses
  `tool_choice: "required"` with `finish`.
- **`eidra eval run`** runs each task of a suite N times. Each attempt runs in its own container,
  with the static binary mounted, a fresh copy of the task's workspace, and no network but the
  model (the vacuum, §13). Checks then run with no network, on a copy of what the agent left,
  with the task's hidden `check/` directory mounted. Each attempt records:
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
- **Harbor tasks.** `eidra eval` also runs tasks in Harbor's format (`instruction.md`,
  `task.toml`, `environment/`, `tests/test.sh` writing a reward to `/logs/verifier/reward.txt`),
  the format of Terminal-Bench, SWE-bench and SWE-rebench. The agent and then the verifier run in
  one container of the task's image, the way Harbor verifies a task in the agent's environment;
  the details are in evals/README.md. `eidra eval select` makes a suite from a downloaded
  dataset, by language and date, and `--remove-images` bounds the disk a run of large images
  needs.
- **To add next:** harder tasks, since this suite saturates (§16.2).
  - SWE-rebench's monthly splits, newer than the model. Qwen3.8 recalls SWE-bench and
    Terminal-Bench environments (§16.2), so tasks it cannot have seen measure the harness, not
    memory. The July 2026 snapshot has 20 Python tasks from issues of 2026-05-15 to 2026-06-15.
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
| M5 | uv, ty, ruff, post-edit diagnostics | the agent hears of the problems its edits introduce (done, §9) |
| M6 | images, PDFs, nested AGENTS.md, compaction, checkpoints | the agent reads images and PDFs, a long session goes on past its context, and a turn can be undone (done, §8, §10, §11, §14) |
| M7 | runtime image and wrapper | the TUI and `exec` run in the image on any project, as the user (done, §13) |
| M8 | sessions resumed, `@` files and Tab, `!` commands; scope chosen by the agent on 2026-10-27, for the user to change | a session can be taken up again after quitting, and a message can bring files and command output (done, §12, §14) |

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

### 16.3 M3: first subagent run (2026-10-26)

The ledger task, with `task` enabled and the prompt asking for one subagent per module, in a
vacuum. Five subagents started from one reply, three at a time. All five finished, and the parent
then ran the tests: 36 of 36 pass.

| | requests | tokens generated | wall |
|---|---|---|---|
| one agent (run file-tools) | 19 | 16K | 1.6 min |
| parent and 5 subagents | 13 + 82 | 69K | 4.8 min |

Each subagent read the spec and its tests again and checked its own module, so the work was four
times larger. Three sequences side by side decoded about 240 tokens/s together, against 150–170
for one, which did not make up for it. On a task this size, delegating costs more than it saves,
as radiance's own five-agent demo found. The harder suite will show where it pays, if anywhere:
larger codebases, and searches whose details the parent does not need.

### 16.4 A 262,144-token context (2026-10-26)

qwen-assistant's window went from 225,280 to 262,144 tokens, the model's trained length, with no
RoPE scaling. Radiance's startup report, before and after:
- The experts keep 22.36 GiB of each card either way.
- The KV cache gets 3.34 GiB a card: 417,360 tokens of context, 1.6 full-length sessions at once
  (1.8 at 225,280).
- The snapshot store stays at 40 slots, 4.40 GiB pinned: geometric backoff keeps 9 a session at
  either length.
- The container uses 31.9 of its 37 GiB.

A near-full retrieval test, as in radiance's October context tests: a 261,064-token archive with
three access codes at 5%, 50% and 95%, sent cold. All three came back right. Prefill ran at
4,218 tok/s, and the answer started after 62 s. (October's 224K test took 5 min 18 s, with
512-token chunks on an older radiance.)

### 16.5 The July 2026 SWE-rebench suite (2026-10-27)

Sixteen Python tasks from SWE-rebench's July 2026 snapshot, from issues of 2026-05-15 to
2026-06-15, in Harbor's format, run in the vacuum one attempt each, two at a time, with the
binary of e659b8a (M4: no diagnostics, compaction or the changes below).

**Baseline: 9 of 16 passed.** Attempts took 6 to 50 minutes and 50 to 200 requests, with 97–99%
of prompt tokens from the cache. Rapid-MLX-426 ran out of context, before compaction existed. The
other six failures ran into no limit, and every one ended with `finish` after long, lint-clean
testing, so the diagnostics of §9 would not have changed them. Why they failed, from their logs
(a background agent read every session):
- three changed existing tests until their own design passed them, where the hidden tests keep
  the old ones: Soju06/codex-lb-744 rewrote a test file, HKUDS/nanobot-4274 changed four tests'
  setup, marimo-9754 changed a public method's return type and two tests;
- three made a choice the instruction does not state and the hidden tests pin (agno-8148,
  livekit-5944, apm-1553), which nothing in the harness can know.

The model also passed `timeout` for `timeout_s` 23 times, asked for more than 600 s 27 times,
spent up to 971 s of an attempt in `sleep` waiting on background jobs, hit 14 network failures in
10 sessions, and was never told its request or time budget (marimo-9766 passed while running
into both).

**Changes made for the next run (v2):** the system prompt says to fix the code and keep the
project's tests and signatures as they are unless the task asks otherwise, and an edit that
changes a committed test says so (§9); `timeout` is taken for `timeout_s`, and capped or default
timeouts are explained (§6); `bash_output` shows the job's command and run time and can wait
(§6); an unattended run hears once when 15% of its requests or of the time the eval gives it
(`--time-limit`) are left; and an agent in the vacuum is told there is no network.

**v2: 19 of 32 passed (59%)**, two attempts per task with the binary of a5d4a7c, which also has
the diagnostics (§9) and compaction (§8). Counting each task by the share of its attempts that
passed, that is 9.5 of 16 against the baseline's 9: no difference that one baseline attempt per
task can show. Underneath:
- **Rewritten tests, mostly gone.** codex-lb-744 and nanobot-4274 passed both attempts,
  marimo-9754 one. The changed-tests note fired once in 32 attempts (deepagents-3866, which
  passed), so the system prompt's line, not the note, is what changed this, if anything did. The
  one such failure left changed a function's return type and then two existing assertions with a
  Python script run through bash, which the note does not see; a check against the commit at
  `finish` would.
- **Unstated choices still fail:** agno-8148 0/2, apm-1553 0/2, livekit-5944 1/2.
- **Compaction works:** both Rapid-MLX-426 attempts compacted once at about 200K tokens and
  finished, where the baseline hit the context limit, but both failed 7 of the hidden tests.
- **New losses:** notebooklm-py-826 failed both attempts the same way, taking any settled future
  in the registry for a refresh of this cycle, where the hidden test leaves one from an earlier
  cycle there; the baseline's attempt told them apart. nanobot-4129, marimo-9626 and
  hermes-webui-3069 passed one attempt of two. The failing ones left the existing tests alone,
  and hermes's ran out of requests.
- **Cost:** 17.3 minutes and 66K generated tokens per attempt, against 15.4 and 54K. Tool errors
  doubled to 1.0 per attempt: mostly multi-line `old` texts whose later lines did not match, in
  long attempts on very large files (agno's module of over 5,000 lines), and six writes refused
  because the file had changed since it was read. The budget note came in 8 attempts, 2 of which
  passed.

Next: the check at `finish`, measured against v2 itself, two attempts per task, so that a change
is compared with the same harness.

## 17. Open questions

- **Keep or drop reasoning in history?** Both cost the same in cache (§3.1). Keeping it is the
  template's default and what the model was trained on; dropping it saves context. Evals decide.
- **The snapshot store under load:** the 40 slots are shared by every session. Measure them with
  subagents and anvil running, using radiance's `/metrics`
  (`radiance:prefix_hit_cut_tokens_total`, `radiance:prefix_cache_ckpt_evictions_by_reason_total`).
- **Evicting a finished subagent's cache** (asked 2026-10-27). A subagent's conversation is never
  sent again once it reports, but radiance keeps it like any other: its attention blocks (in VRAM,
  copied to the 8 GiB host tier and given back to the experts on demand) and its linear-state
  snapshots (about nine, in the 40-slot store every conversation shares), until the 4-hour TTL or
  until memory is wanted. Radiance has no way to drop one conversation: `POST /reset_prefix_cache`
  drops every one, the parent's included, and `cache_prompt: false` is ignored.
  - What it costs now is probably little: eviction is least-recently-used, and a finished
    subagent's entries are older than those of the conversations still going, so they go first.
    The risk is a burst: several subagents ending together fill the snapshot store, and a parent
    idle for a while (the user reading) can lose its snapshots before they do. Measure it with
    `/sessions` and `prefix_cache_ckpt_evictions_by_reason_total{reason="store_full"}` while
    subagents run.
  - If it matters, the fix is in radiance: `DELETE /sessions/{session_id}`, which drops one
    conversation's blocks, host copy and snapshots. eidra already labels every request with its
    session's id (`session_id`), so it would call it when a subagent reports. A shared prefix (the
    system prompt) stays while any conversation uses it, as radiance does now.
- **Settings to tune with evals:** the compaction threshold, the thinking policy per kind of step,
  and tool-schema variants.
- **ty:** is it good enough on real projects?
