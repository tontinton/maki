+++
title = "Headless Mode"
weight = 21
[extra]
group = "Guides"
+++

# Headless Mode

Run Maki non-interactively with `--print` / `-p`. Useful for scripts, CI, and automation.

```bash
maki "explain this codebase" --print
```

Pipe via stdin:

```bash
echo "list all TODO comments" | maki -p
```

A headless run never asks about [folder trust](/docs/folder-trust/), so a
project `.maki` directory it has no stored answer for is skipped and reported on
standard error. In a container, add `--trust` to load it for that run.

## Output Formats

| Format | Description |
|--------|-------------|
| `text` | Raw response only (default) |
| `json` | Single JSON object with metadata |
| `stream-json` | JSONL stream, one event per line |

```bash
maki "fix the tests" --print --output-format json
```

JSON output includes `type`, `subtype`, `is_error`, `duration_ms`, `num_turns`, `result`, `stop_reason`, `session_id`, `total_cost_usd`, and `usage`.

`usage` totals the whole run rather than the last turn. On a multi-turn run
`input_tokens` is near zero and `cache_read_input_tokens` carries the volume,
because every turn after the first reads the prompt from cache.

Add `--verbose` to include full turn-by-turn messages in the output.

## Sessions

A `--print` run stores its session, so `maki session list` shows it and the next
run can resume it with `-c` or `-r <id>`. `text` output prints `session: <id>`
on stderr to keep stdout clean for pipes. `json` and `stream-json` return it as
`session_id`.

Maki saves at the end of each turn, so a run that dies before its first turn
stores nothing. If a save fails, for example on a full disk, Maki prints
`maki: failed to save session <id>` on stderr. The exit code ignores save
failures, so check stderr before a script resumes with `-c`.

`--session-id <ID>` picks the id up front. It fails if a session already exists
under that id. To build on a session that exists:

| Goal | Flags |
|------|-------|
| Continue it in place | `-r <id>` |
| Copy its history under a generated id | `-r <id> --fork-session` |
| Copy its history under an id you choose | `-r <old> --session-id <new>` |

A copy belongs to the directory you run it in, so `maki -c` there continues the
copy rather than the session it came from.

The `session_id` in output messages is the id string you passed, so a hex uuid
comes back as that hex uuid rather than the base58 form Maki generates.

### One process per session

Only one Maki process can have a session open, so two runs cannot overwrite
each other's turns. A second process that resumes a busy session with `-c` or
`-r` exits before it sends any request:

```
session <id> is open in another maki process
  --fork-session  work on a copy of it
  or drop -c/-r to start a new session here
```

The lock is released when the process exits, even after a crash. On a filesystem
without file locks, such as some NFS mounts, Maki logs a warning and runs
unlocked.

## Background Tasks

`--print` and SDK mode keep background moves off unless you pass `--background-timeout <SECS>`. Every call blocks until it finishes, and `background = true` runs in the foreground. With moves on, a dev server the model moved would hold a CI job for the full timeout, and a turn could end while its subagents still run, which an SDK client would read as finished work.

With the flag set, work moves as in the [TUI](/docs/background-tasks/), and Maki waits for it after the run ends:

| Mode | While background work runs |
|------|----------------------------|
| `--print` | The process stays alive and each result starts a new turn. The final result covers those turns, and `text` output prints the last turn's text. |
| SDK, stdin open | Results ride along with the next user message. No turn starts on its own, so the client gets exactly one result per message. |
| SDK, stdin closed | As `--print`, but each woken turn sends a result of its own. |

`--print` counts spend after the last turn, such as a subagent stopped mid-run, in the final `total_cost_usd` and `usage`, and in the stored session. An SDK result cannot include spend that came after it, so Maki sends that spend in one extra message before it exits, only when there is some. Add its `total_cost_usd` to the sum of the results for the full bill:

```json
{"type":"system","subtype":"background_usage","total_cost_usd":0.12,"usage":{"input_tokens":5300,"output_tokens":800,...},"session_id":"..."}
```

With `--verbose` and in SDK mode, a notice comes before each woken turn, so its messages do not run on from the last turn's. Claude Code has no such message.

```json
{"type":"system","subtype":"notice","text":"Background bash finished: cargo test","session_id":"..."}
```

The process exits once nothing is running. If `--background-timeout` runs out first, Maki stops the tasks and warns on stderr:

```
N background task result(s) never reached the model before --background-timeout ran out ...
```

No turn starts past the deadline, so the count includes results that came in during the last turn. Resuming the session reports all of them as lost.

## Subagents

Parallel subagents interleave their messages in `--verbose`, `stream-json` and SDK output. Maki tells them apart with Claude Code's task messages:

| `subtype` | Sent |
|-----------|------|
| `task_started` | Right before the subagent's first message |
| `task_progress` | After each of its turns that called tools |
| `task_notification` | Once, when it ends. It comes right before the result of the call that opened the subagent, or for a call moved to the [background](/docs/background-tasks/), before the first result after the subagent ended |

```json
{"type":"system","subtype":"task_started","task_id":"toolu_01...","tool_use_id":"toolu_01...","description":"Find the auth middleware","task_type":"local_agent","prompt":"...","session_id":"..."}
{"type":"system","subtype":"task_progress","task_id":"toolu_01...","tool_use_id":"toolu_01...","description":"Find the auth middleware","usage":{"total_tokens":52470,"tool_uses":2,"duration_ms":4314},"last_tool_name":"Read","session_id":"..."}
{"type":"system","subtype":"task_notification","task_id":"toolu_01...","tool_use_id":"toolu_01...","status":"completed","output_file":"","summary":"Auth is checked in src/auth.rs","usage":{...},"session_id":"..."}
```

`task_id` and `tool_use_id` hold the same id: the `task` call's id, or a separate id when another call such as `batch` opened the subagent. Each message of the subagent has `parent_tool_use_id` set to it. A nested subagent counts as part of the top-level task above it, for its messages and its `usage`.

`status` and `summary` come from the result of the call that ran the subagent:

| `status` | When | `summary` |
|----------|------|-----------|
| `completed` | The call succeeded | Its result |
| `failed` | The call failed | The error |
| `stopped` | A stop or cancel cut it, or the output ended first | The reason |

A plugin that runs its own subagents sets these by passing its tool result to [`Session:close()`](/docs/lua-api/#Session-close). Without one, the subagent reads as `completed` with an empty summary.

`output_file` is always empty. Maki keeps subagent transcripts in the session.

## Stopping a Run

`Ctrl+C` (SIGINT) or SIGTERM stops a `--print` or SDK run without leaving work behind. Maki cancels the turn in flight, stops the background tasks, and kills the processes its tools started. It then sends the result as usual. If a turn was running, the result holds what that turn produced so far, with `"stop_reason":"cancelled"` and `"is_error":true`. Results that never reached the model get the same warning as a timeout, ending in `before maki was interrupted`.

The exit code is 130 after SIGINT and 143 after SIGTERM, as a shell reports a process the signal killed. A second signal exits at once, without stopping the work.

## Claude Code Compatibility

Maki's `--print` is a drop-in replacement for Claude Code:

```bash
# Before
claude "fix the bug" --print --output-format json

# After
maki "fix the bug" --print --output-format json
```

Same JSON fields, same `--output-format` options, same `--verbose` behavior. Scripts that parse Claude Code output work unchanged.

## SDK / Stream Mode

For tools like Conductor, Windsurf, or custom orchestrators that speak the Claude Code SDK wire protocol, use `--input-format stream-json`:

```bash
maki --print --input-format stream-json
```

This enters a bidirectional NDJSON loop over stdio instead of the one-shot print path:

```
your orchestrator                     maki --print --input-format stream-json
        │                                             │
        │  {"type":"user",...}            (stdin)     │
        ├─────────────────────────────────────────────►
        │                                             │
        ◄─────────────────────────────────────────────┤
        │  system / assistant / stream_event / result │
        │  one JSON object per line       (stdout)    │
```

Inbound messages (`user`, `control_request`, `control_response`, `control_cancel_request`) drive the agent; outbound messages match the Claude Code SDK shape. Under the hood it reuses the same driver as the TUI and ACP server, so sessions, tools, and permissions all work the same way.

SDK-only flags (`--permission-mode`, `--include-partial-messages`, ...) are listed in the [CLI flag matrix](/docs/cli/#flags-by-run-path).

Two caveats:

- One-shot `--print` always runs in **build** mode. Plan mode needs the SDK path (or the TUI).
- The plan file for SDK `--permission-mode plan` is `./plan.md` under cwd, not the state-dir `plans/<slug>.md` files the TUI uses.

### Quick example

```bash
echo '{"type":"user","message":{"content":"explain this repo"}}' \
  | maki --print --input-format stream-json --max-turns 3
```

## Examples

Pipe compiler errors back for a fix:

```bash
cargo build 2>&1 | maki "Fix these compiler errors." --print --yolo
```

Generate a changelog from recent commits:

```bash
git log --oneline v1.2.0..HEAD | maki "Write a user-facing \
  changelog grouped by: Added, Changed, Fixed. Skip chores." --print
```

Automated PR summaries in CI:

```bash
SUMMARY=$(git diff main..HEAD | maki "Write a 2-3 sentence \
  summary of this change for a PR description." --print)
gh pr edit --body "$SUMMARY"
```

Migrate an API across many files:

```bash
grep -rl 'old_api_call' src/ | while read file; do
  maki "In $file, migrate old_api_call() to new_api_call(). \
    Keep behavior identical." -p --yolo --allowed-tools Read,Edit
done
```

Cost tracking:

```bash
maki "refactor the database layer" -p --output-format json | jq '.total_cost_usd'
```
