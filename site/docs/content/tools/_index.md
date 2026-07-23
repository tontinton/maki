+++
title = "Tools"
weight = 3
[extra]
group = "Reference"
+++

# Tools

Maki ships with 20 built-in tools. This is the full reference.

## File Operations

### `bash` *(lua plugin)*

Execute a bash command. Default dir: <cwd>. DO NOT use for file ops - only git, builds, tests, system commands. Use `workdir` instead of `cd && cmd`. Chain dependent commands with `&&`. Use batch for independent ones. Provide short `description` (3-5 words). Output truncated beyond 2000 lines or 50KB. Interactive commands (sudo, ssh) fail immediately.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `command` | string | yes |  |
| `description` | string | no |  |
| `timeout` | integer | no |  |
| `workdir` | string | no |  |

### `read` *(lua plugin)*

Read a file or directory. Returns contents with line numbers (1-indexed).

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `limit` | integer | no |  |
| `offset` | integer | no |  |
| `path` | string | yes |  |

### `write` *(lua plugin)*

Write content to a file, replacing existing content. Creates parent directories. Always read first. Never create files unless necessary. Never proactively create docs (*.md, README) unless requested.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `content` | string | yes |  |
| `path` | string | yes |  |

### `edit` *(lua plugin)*

Replace an exact string match in a file. old_string must appear exactly once unless replace_all is true. Read file first. When copying from read output, exclude line number prefix (e.g. `42: `). Prefer over write for targeted changes. Use replace_all for renaming.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `new_string` | string | yes |  |
| `old_string` | string | yes |  |
| `path` | string | yes |  |
| `replace_all` | boolean | no |  |

### `multiedit` *(lua plugin)*

Make multiple find-and-replace edits to a single file atomically. Prefer over edit for multiple changes. Read file first. old_string must match exactly, including whitespace. Each edit must match exactly once unless replace_all. Edits applied in sequence. If any edit fails, none are written. Ensure earlier edits don't affect later edits.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `edits` | array | yes |  |
| `path` | string | yes |  |

### `edit_lines` *(lua plugin, opt-in)*

Edit lines by number. Replaces lines from `start` to `end` (inclusive) with `new_string`. Use empty `new_string` to delete. Do not use with batch.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `end` | integer | yes |  |
| `new_string` | string | yes |  |
| `path` | string | yes |  |
| `start` | integer | yes |  |

### `insert_lines` *(lua plugin, opt-in)*

Insert lines before a given line number. Lines at `line` and below shift down. Existing lines preserved. Do not use with batch.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `line` | integer | yes |  |
| `new_string` | string | yes |  |
| `path` | string | yes |  |

### `glob` *(lua plugin)*

Find files by glob pattern. Respects .gitignore. Returns absolute paths sorted by modification time (newest first). Prefer speculative parallel searches over sequential glob+grep.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `path` | string | no |  |
| `pattern` | string | yes |  |

### `grep` *(lua plugin)*

Search file contents using regex. Respects .gitignore. Results grouped by file, sorted by modification time. Prefer speculative parallel searches over sequential glob+grep. Do NOT wrap pattern in quotes or double-escape (e.g. `\[` not `\\[`). Multi-line matching auto-enabled when pattern contains `\n`, `(?s)`, or `(?m)`.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `context_after` | integer | no |  |
| `context_before` | integer | no |  |
| `include` | string | no |  |
| `limit` | integer | no |  |
| `path` | string | no |  |
| `pattern` | string | yes |  |

### `index` *(lua plugin)*

Return a compact overview of a source file: imports, types, function signatures, and structure with line numbers in []. ~70-90% more efficient than reading full file. Use FIRST to understand structure before read with offset/limit. Supports source files and markdown. Falls back with error on unsupported languages.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `path` | string | yes |  |

### `view_image` *(lua plugin)*

View an image file (png, jpeg, gif, webp) as vision input. Use instead of `read` for images. Paths: absolute, relative, or ~/. Oversized images downscaled automatically (animated gif/webp keep only first frame).

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `path` | string | yes |  |

## Execution & Control

### `batch` *(lua plugin)*

Execute multiple independent tool calls concurrently. ALWAYS use batch for multiple independent calls. 1-25 tools per batch. Parallel execution, order not guaranteed. Partial failures don't stop others. Do NOT nest batch. Use code_execution for dependent operations.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `tool_calls` | array | yes | Array of tool calls to execute in parallel |

### `code_execution` *(lua plugin)*

Execute Python code in a sandboxed interpreter with tools as callable functions. Use for chained/dependent tool calls and filtering/processing results. Faster than sequential tool calls. Tools are async: `result = await read(path='file.txt')`. Use `asyncio.gather()` for concurrency. Available libs: re, asyncio, sys, os, json. Fresh sandbox each run. 30s script timeout (`timeout` param); time awaiting tool calls doesn't count.

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `code` | string | yes |  | Python code to execute. Tools are async functions that return strings (not objects). You MUST await every call: `result = await read(path='/file')`. Use `await asyncio.gather(...)` for concurrency. |
| `timeout` | integer | no | 30 | Script execution timeout in seconds |

### `question` *(lua plugin)*

Ask the user questions during execution. Supports single/multi-select, custom answers, and tabbed multi-question forms. Put recommended options first with "(Recommended)" suffix.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `questions` | array | yes | List of questions to ask the user |

## Agent & Knowledge

### `task` *(lua plugin)*

Launch an autonomous subagent. Types: research (read-only, default) or general (full access). Best combined with batch. Each invocation starts fresh - inline context. Summarize results in your response.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `description` | string | yes | Short (3-5 words) description of the task |
| `model_tier` | string | no | Model tier (optional, omit to use current model, capped at current tier): "strong" (deep reasoning, ~5x cost), "medium" (balanced), "weak" (fast/cheap). |
| `output_schema` | string | no | JSON Schema (object) the subagent's final result must match. When set, the result is returned as a validated JSON string. |
| `prompt` | string | yes | Detailed task prompt for the agent |
| `subagent_type` | string | no | Subagent type: "research" (read-only, default) or "general" (can modify files) |

### `todo_write` *(lua plugin)*

Create or update a structured todo list to track tasks. Use after EACH completed step. Send complete list each time (replace-all semantics). Use ONLY for multi-step work (3+ steps). Skip for trivial tasks.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `todos` | array | yes | The updated todo list |

### `memory` *(lua plugin)*

Persistent, project-scoped scratchpad for learnings, patterns, decisions, and gotchas across sessions. Save important context before compaction or to build project knowledge. Keep entries concise and current. Delete outdated information.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `command` | string | yes | Command: view, write, delete |
| `content` | string | no | File content for 'write' |
| `path` | string | no | Relative path (e.g. 'architecture.md'). Omit to list all. |

### `skill` *(lua plugin)*

Load a skill that provides instructions and workflows for specific tasks.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `name` | string | yes | Name of the skill to load |

## Web

### `webfetch` *(lua plugin)*

Fetch a URL and return its contents. Supports markdown (default), text, or html. HTTP auto-upgraded to HTTPS. Max 5MB response, 120s timeout. Best used inside code_execution to avoid context bloat.

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `format` | string | no |  | Output format: markdown (default), text, or html |
| `timeout` | integer | no | 30, max 120 | Timeout in seconds |
| `url` | string | yes |  | URL to fetch (http:// or https://) |

### `websearch` *(lua plugin)*

Search the web for real-time information using Exa AI.

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `num_results` | integer | no | 8 | Number of results to return |
| `query` | string | yes |  | Search query |