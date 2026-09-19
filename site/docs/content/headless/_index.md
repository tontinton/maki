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
run can resume it with `-c` or `-s <id>`. `text` output prints `session: <id>`
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
| Continue it in place | `-s <id>` |
| Copy its history under a generated id | `-s <id> --fork-session` |
| Copy its history under an id you choose | `-s <old> --session-id <new>` |

A copy belongs to the directory you run it in, so `maki -c` there continues the
copy rather than the session it came from.

The `session_id` in output messages is the id string you passed, so a hex uuid
comes back as that hex uuid rather than the base58 form Maki generates.

### One process per session

Only one Maki process can have a session open, so two runs cannot overwrite
each other's turns. A second process that resumes a busy session with `-c` or
`-s` exits before it sends any request:

```
session <id> is open in another maki process
  --fork-session  work on a copy of it
  or drop -c/-s to start a new session here
```

The lock is released when the process exits, even after a crash. On a filesystem
without file locks, such as some NFS mounts, Maki logs a warning and runs
unlocked.

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

SDK-only flags (`--system-prompt`, `--max-turns`, `--permission-mode`, `--include-partial-messages`, ...) are listed in the [CLI flag matrix](/docs/cli/#flags-by-run-path).

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
