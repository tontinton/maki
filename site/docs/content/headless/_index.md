+++
title = "Headless Mode"
weight = 8
[extra]
group = "Reference"
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

Add `--verbose` to include full turn-by-turn messages in the output.

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

This enters a bidirectional NDJSON loop over stdio instead of the one-shot print path. Inbound messages (`user`, `control_request`, `control_response`) drive the agent; outbound messages (`system`, `assistant`, `result`, `stream_event`, `control_request`) match the Claude Code SDK shape.

Under the hood it reuses the same `spawn_interactive` driver as the TUI and ACP server, so sessions, tools, and permissions all work the same way.

### Flags

| Flag | Description |
|------|-------------|
| `--system-prompt` | Override the system prompt entirely |
| `--append-system-prompt` | Append text to the built-in system prompt |
| `--max-turns` | Cap the number of agent turns |
| `--session-id <id>` | Set a specific session ID |
| `--resume <id>` / `-s <id>` | Resume an existing session |
| `--fork-session` | Load a session's history under a new ID |
| `--continue` | Resume the most recent session in the current directory |
| `--permission-mode <mode>` | `default`, `acceptEdits`, `plan`, or `bypassPermissions` |
| `--include-partial-messages` | Stream Anthropic-shaped deltas (`message_start`, `content_block_delta`, ...) |
| `--allowed-tools` / `--disallowed-tools` | Comma-separated tool allow/deny lists (PascalCase or snake_case) |

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
