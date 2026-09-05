+++
title = "Tools"
weight = 4
[extra]
group = "Reference"
+++

# Tools

Maki ships with 23 built-in tools in this reference (22 on by default, 1 opt-in via plugin options). Tools marked **opt-in** are off until you enable them under `plugins` in [Configuration](/docs/configuration/).

## File Operations

### `bash` {#bash}

Execute a bash command.
Commands run in <cwd> by default.

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `command` | string | yes |  | The bash command to execute |
| `description` | string | no |  | Short description (3-5 words) of what the command does |
| `timeout` | integer | no | 120 | Timeout in seconds |
| `workdir` | string | no | cwd | Working directory |

### `list` {#list}

List directory contents. Returns entry names sorted alphabetically, directories first with a trailing /.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `path` | string | yes | Absolute path to the directory |

### `read` {#read}

Read a file. Returns contents with line numbers (1-indexed).

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `limit` | integer | yes | Max number of lines to read. Use 0 to read until end of file (capped at 2000 lines). |
| `offset` | integer | yes | Line number to start from (1-indexed). Use 1 for the first line. |
| `path` | string | yes | Absolute path to the file |

### `write` {#write}

Write content to a file, replacing existing content.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `content` | string | yes | The complete file content to write |
| `path` | string | yes | Absolute path to the file |

### `edit` {#edit}

Replace an exact string match in a file.

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `new_string` | string | yes |  | Replacement string |
| `old_string` | string | yes |  | Exact string to find (must match uniquely unless replace_all is true) |
| `path` | string | yes |  | Absolute path to the file |
| `replace_all` | boolean | no | false | Replace all occurrences |

### `multiedit` {#multiedit}

Make multiple find-and-replace edits to a single file atomically.
Prefer this over edit when making multiple changes to the same file.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `edits` | array | yes | Array of edit operations to apply sequentially |
| `path` | string | yes | Absolute path to the file |

### `edit_lines` {#edit_lines}

Edit lines by number. Replaces lines from `start` to `end` (inclusive) with `new_string`. Use empty `new_string` to delete a range. Do not use with the batch tool.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `end` | integer | yes | Last line, inclusive |
| `new_string` | string | yes | Replacement text |
| `path` | string | yes | Absolute path to the file |
| `start` | integer | yes | First line (1-indexed) |

### `insert_lines` <span class="badge badge-optin">opt-in</span> {#insert_lines}

Insert `new_string` after line `line`, or at the top with 0. Only include new lines, never lines already in the file. Do not use with the batch tool.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `line` | integer | yes | Line number to insert after (1-indexed). Use 0 to insert at the top. |
| `new_string` | string | yes | Text to insert |
| `path` | string | yes | Absolute path to the file |

### `glob` {#glob}

Find files by glob pattern.

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `path` | string | no | cwd | Directory to search in |
| `pattern` | string | yes |  | Glob pattern (e.g. **/*.rs, src/**/*.ts) |

### `grep` {#grep}

Search file contents using regex.

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `context_after` | integer | no |  | Context lines after match |
| `context_before` | integer | no |  | Context lines before match |
| `include` | string | no |  | File glob filter (e.g. *.c) |
| `limit` | integer | no |  | Max match groups to return |
| `path` | string | no | cwd | Directory to search in |
| `pattern` | string | yes |  | Regex pattern |

### `index` {#index}

Return a compact overview of a source file: imports, type definitions, function signatures, and structure with their line numbers surrounded by []. ~70-90% more efficient than reading the full file.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `path` | string | yes | Absolute path to the file |

### `view_image` {#view_image}

View an image file (png, jpeg, gif, webp) so you can actually see it; it is returned as vision input alongside the tool result. Use instead of `read` for images.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `path` | string | yes | Path to the image file |

## Execution & Control

### `batch` {#batch}

Executes multiple independent tool calls concurrently to reduce round-trips.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `tool_calls` | array | yes | Array of tool calls to execute in parallel |

### `code_execution` {#code_execution}

Execute Python in a sandbox where every tool is an async function.

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `code` | string | yes |  | Python code. Tools return strings, not objects, and you MUST await every call: `result = await read(path='/file', offset=1, limit=0)`. |
| `timeout` | integer | no | 30 | Script execution timeout in seconds |

### `question` {#question}

Use this tool when you need to ask the user questions during execution. This allows you to:
- Gather user preferences or requirements
- Clarify ambiguous instructions
- Get decisions on implementation choices as you work
- Offer choices to the user about what direction to take

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `questions` | array | yes | List of questions to ask the user |

## Agent & Knowledge

### `task` {#task}

Launch an autonomous subagent to perform tasks independently. Best combined with batch.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `background` | boolean | no | Run in the background. The call returns a receipt at once, the outcome arrives as a message when the subagent finishes, and task_result or task_wait can fetch it. |
| `description` | string | yes | Short (3-5 words) description of the task |
| `model_tier` | string | no | Model tier (optional, omit to use current model, capped at current tier):<br>- "strong" (e.g. Opus): Deep reasoning, complex architecture, subtle bugs, most critical sections. ~5x cost of medium.<br>- "medium" (e.g. Sonnet): Balanced. Refactors, features, multi-file changes.<br>- "weak" (e.g. Haiku): Fast/cheap. Search, summarize, boilerplate, simple edits. |
| `output_schema` | string | no | JSON Schema (object) the subagent's final result must match. When set, the result is returned as a validated JSON string. |
| `prompt` | string | yes | Detailed task prompt for the agent |
| `subagent_type` | string | no | Subagent type: "research" (read-only, default) or "general" (can modify files) |

### `task_result` {#task_result}

Fetch the status or final output of a background task launched with the task tool (`background = true`).

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `task_id` | string | yes | Task id from the background receipt. |

### `task_wait` {#task_wait}

Block until a background task finishes or the timeout passes. Returns the result when done, a still-working note on timeout.

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `task_id` | string | yes |  | Task id from the background receipt. |
| `timeout_ms` | integer | no | 60000, cap 600000 | Maximum wait in milliseconds. |

### `todo_write` {#todo_write}

Create or update a structured todo list to track tasks.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `todos` | array | yes | The updated todo list |

### `memory` {#memory}

Persistent, project-scoped scratchpad for learnings, patterns, decisions, and gotchas across sessions.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `command` | string | yes | - `list [tags]`: tag-grouped index, no bodies.<br>- `read path\|tags`: one body (path) or collated bodies (tags).<br>- `write path tags content`: create or overwrite a note.<br>- `delete path` |
| `content` | string | no | Body for write (frontmatter added automatically). |
| `path` | string | no | Relative path, e.g. 'architecture.md'. |
| `tags` | array | no | snake_case tags. Filter for list/read; assigned on write (defaults to filename stem). |

### `skill` {#skill}

Load a skill that provides instructions and workflows for specific tasks.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `name` | string | yes | Name of the skill to load |

## Web

### `webfetch` {#webfetch}

Fetch a URL and return its contents.

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `format` | string | no |  | Output format: markdown (default), text, or html |
| `timeout` | integer | no | 30, max 120 | Timeout in seconds |
| `url` | string | yes |  | URL to fetch (http:// or https://) |

### `websearch` {#websearch}

Search the web for real-time information using Exa AI.

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `num_results` | integer | no | 8 | Number of results to return |
| `query` | string | yes |  | Search query |