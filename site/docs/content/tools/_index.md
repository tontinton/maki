+++
title = "Tools"
weight = 3
[extra]
group = "Reference"
+++

# Tools

Maki ships with 22 built-in tools. This is the full reference.

## File Operations

### `bash` *(lua plugin)*

Execute a bash command (runs in <cwd> by default). Use only for git, builds, tests, and system commands; do not use for file operations. Use `workdir` instead of `cd`. Chain dependent commands with `&&` and batch for independent ones. Output truncated beyond 2000 lines or 50KB. Interactive commands fail immediately.

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `command` | string | yes |  | The bash command to execute |
| `description` | string | no |  | Short description (3-5 words) of what the command does |
| `timeout` | integer | no | 120 | Timeout in seconds |
| `workdir` | string | no | cwd | Working directory |

### `read` *(lua plugin)*

Read a file or directory with line numbers (1-indexed).

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `limit` | integer | no | Max number of lines to read. Omitting the limit reads up to 2000 lines. |
| `offset` | integer | no | Line number to start from (1-indexed) |
| `path` | string | yes | Absolute path to the file or directory |

### `write` *(lua plugin)*

Write content to a file, replacing existing content. Creates parent directories if needed. Read the file first; prefer editing existing files. Only create documentation files when explicitly requested.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `content` | string | yes | The complete file content to write |
| `path` | string | yes | Absolute path to the file |

### `edit` *(lua plugin)*

Replace an exact string match in a file. The old_string must appear exactly once unless replace_all is true. Read the file first; do NOT include line-number prefixes from read output. Prefer this over write for targeted changes; use replace_all for renaming.

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `new_string` | string | yes |  | Replacement string |
| `old_string` | string | yes |  | Exact string to find (must match uniquely unless replace_all is true) |
| `path` | string | yes |  | Absolute path to the file |
| `replace_all` | boolean | no | false | Replace all occurrences |

### `multiedit` *(lua plugin)*

Apply multiple find-and-replace edits to a single file atomically. Read the file first; each old_string must match exactly once unless replace_all is true. Edits apply in sequence; if any fails, none are written. Ensure earlier edits do not alter text later edits need.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `edits` | array | yes | Array of edit operations to apply sequentially |
| `path` | string | yes | Absolute path to the file |

### `edit_lines` *(lua plugin, opt-in)*

Replace a line range (start to end, inclusive) with `new_string`; empty `new_string` deletes the range.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `end` | integer | yes | Last line, inclusive |
| `new_string` | string | yes | Replacement text |
| `path` | string | yes | Absolute path to the file |
| `start` | integer | yes | First line (1-indexed) |

### `insert_lines` *(lua plugin, opt-in)*

Insert `new_string` before the given 1-indexed line number. Existing lines shift down.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `line` | integer | yes | Line number to insert before (1-indexed). Use 1 to insert at the top. |
| `new_string` | string | yes | Text to insert |
| `path` | string | yes | Absolute path to the file |

### `glob` *(lua plugin)*

Find files by glob pattern. Respects .gitignore and returns absolute paths sorted by modification time (newest first). Prefer parallel searches.

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `path` | string | no | cwd | Directory to search in |
| `pattern` | string | yes |  | Glob pattern (e.g. **/*.rs, src/**/*.ts) |

### `grep` *(lua plugin)*

Search file contents with regex. Respects .gitignore, returns results grouped by file. Prefer parallel searches. Do not quote or double-escape the pattern. Multi-line matching auto-enables for `\n`, `(?s)`, or `(?m)`.

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `context_after` | integer | no |  | Context lines after match |
| `context_before` | integer | no |  | Context lines before match |
| `include` | string | no |  | File glob filter (e.g. *.c) |
| `limit` | integer | no |  | Max match groups to return |
| `path` | string | no | cwd | Directory to search in |
| `pattern` | string | yes |  | Regex pattern |

### `index` *(lua plugin)*

Return a compact overview of a source file: imports, types, function signatures, and structure with line numbers. Use before read to understand file structure; falls back with an error on unsupported languages.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `path` | string | yes | Absolute path to the file |

### `view_image` *(lua plugin)*

View an image file (png, jpeg, gif, webp) so you can actually see it; it is returned as vision input alongside the tool result. Use instead of `read` for images.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `path` | string | yes | Path to the image file |

## Execution & Control

### `batch` *(lua plugin)*

Executes multiple independent tool calls concurrently to reduce round-trips.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `tool_calls` | array | yes | Array of tool calls to execute in parallel |

### `code_execution` *(lua plugin)*

Execute Python code in a sandboxed interpreter with tools as callable functions.

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `code` | string | yes |  | Python code to execute. Tools are async functions that return strings. You MUST await every call: `result = await read(path='/file')`. Use `await asyncio.gather(...)` for concurrency. |
| `timeout` | integer | no | 30, max 300 | Timeout in seconds |

### `question` *(lua plugin)*

Ask the user questions during execution to gather preferences, clarify instructions, or get decisions. `custom` is enabled by default; don't add catch-all options. Answers are arrays of labels; use `multiSelect` for multi-select. Put the recommended option first.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `questions` | array | yes | List of questions to ask the user |

### `tool_search` *(lua plugin)*

Search for deferred tools by name or description. Returns a list of tools that can be loaded on demand.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `namespace` | string | no | Optional namespace filter |
| `query` | string | yes | Search query to match tool names or descriptions |

### `load_namespace` *(lua plugin)*

Load all tools from a namespace. Returns the list of tools that were loaded.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `namespace` | string | yes | Namespace to load |

## Agent & Knowledge

### `task` *(lua plugin)*

Launch an autonomous subagent for independent tasks. Use `subagent_type` "research" for read-only exploration or "general" for implementation work. Launch multiple tasks concurrently when possible; inline needed context and ask for concise, file:line summaries.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `description` | string | yes | Short (3-5 words) description of the task |
| `model_tier` | string | no | Optional capped model tier: "weak", "medium", or "strong". |
| `output_schema` | string | no | JSON Schema the subagent's final result must match. Returned as a validated JSON string. |
| `prompt` | string | yes | Detailed task prompt for the agent |
| `subagent_type` | string | no | Subagent type: "research" (read-only, default) or "general" (can modify files) |

### `todo_write` *(lua plugin)*

Create or update a structured todo list for multi-step work (3+ steps). Send the complete list each time (replace-all) and update after each completed step. Skip for trivial tasks.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `todos` | array | yes | The updated todo list |

### `memory` *(lua plugin)*

Persistent, project-scoped scratchpad for learnings, patterns, decisions, and gotchas across sessions.

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

Fetch a URL and return its contents. Supports markdown (default), text, and html. HTTP auto-upgrades to HTTPS. Max 5MB response, 120s timeout. Use inside code_execution with truncation to avoid bloat.

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `format` | string | no |  | Output format: markdown (default), text, or html |
| `timeout` | integer | no | 30, max 120 | Timeout in seconds |
| `url` | string | yes |  | URL to fetch (http:// or https://) |

### `websearch` *(lua plugin)*

Search the web using Exa AI (today is YYYY-MM-DD). Use for current events, docs, APIs, or anything not in local files. Prefer targeted queries.

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `num_results` | integer | no | 8 | Number of results to return |
| `query` | string | yes |  | Search query |