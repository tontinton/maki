+++
title = "Tools"
weight = 3
+++

# Tools

Maki ships with 17 built-in tools. This is the full reference.

## File Operations

### `bash`

Runs shell commands. Use it for git, builds, tests, and anything else you'd type in a terminal. Not for reading or writing files.

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `command` | string | yes | | The bash command to execute |
| `description` | string | no | | Short description (3-5 words) |
| `timeout` | u64 | no | 120 | Timeout in seconds |
| `workdir` | string | no | cwd | Working directory |

### `read`

Reads a file or directory listing. Output includes 1-indexed line numbers for precise references.

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `path` | string | yes | | Absolute path to file or directory |
| `offset` | usize | no | | Start line (1-indexed) |
| `limit` | usize | no | 2000 | Max lines to read |

### `write`

Overwrites a file with new content. Creates parent directories if needed.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `path` | string | yes | Absolute path |
| `content` | string | yes | Complete file content |

### `edit`

Finds an exact string in a file and replaces it. The match must be unique unless `replace_all` is set.

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `path` | string | yes | | Absolute path |
| `old_string` | string | yes | | Exact string to find |
| `new_string` | string | yes | | Replacement string |
| `replace_all` | bool | no | false | Replace all occurrences |

### `multiedit`

Applies multiple replacements to the same file atomically. Edits run in sequence, each seeing the result of the previous. If any edit fails, nothing is written.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `path` | string | yes | Absolute path |
| `edits` | array | yes | Array of `{old_string, new_string, replace_all?}` |

### `glob`

Finds files matching a glob pattern. Respects `.gitignore` and returns results sorted by modification time (newest first).

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `pattern` | string | yes | | Glob pattern (e.g. `**/*.rs`) |
| `path` | string | no | cwd | Directory to search |

### `grep`

Searches file contents with regex. Respects `.gitignore`. Results are grouped by file and sorted by modification time.

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `pattern` | string | yes | | Regex pattern |
| `path` | string | no | cwd | Directory to search |
| `include` | string | no | | File glob filter (e.g. `*.rs`) |

### `index`

Returns a compact skeleton of a source file: imports, type definitions, and function signatures, all with line numbers. Uses 70-90% fewer tokens than reading the full file. Powered by tree-sitter, supports 15+ languages.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `path` | string | yes | Absolute path |

## Execution & Control

### `batch`

Runs 1 to 25 independent tool calls in parallel. Partial failures don't block the rest. Useful when reading multiple files or running unrelated lookups at once.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `tool_calls` | array | yes | Array of `{tool, parameters}` |

### `code_execution`

Runs Python in a sandboxed interpreter. All tools are available as async functions inside the sandbox, so the agent can chain calls, filter results, or do light data processing in one round trip.

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `code` | string | yes | | Python code (must `await` tool calls) |
| `timeout` | u64 | no | 30 | Timeout in seconds (max 300) |

### `question`

Asks the user a question. Can present predefined options for single or multi-select answers.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `questions` | array | yes | Array of question objects with `question`, `options`, `multiple` |

## External

### `webfetch`

Fetches the contents of a URL. HTTP is upgraded to HTTPS automatically. Max response size is 5MB.

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `url` | string | yes | | URL to fetch |
| `format` | string | no | markdown | Output format: markdown, text, html |
| `timeout` | u64 | no | 30 | Timeout in seconds |

### `websearch`

Searches the web via Exa AI. Useful for real-time information not available in the codebase.

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `query` | string | yes | | Search query |
| `num_results` | u64 | no | 8 | Number of results |

## Agent & Knowledge

### `task`

Runs a sub-agent on an independent task. Research agents are read-only; general agents get full tool access. Model tier is configurable, so lightweight tasks don't use expensive models.

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `description` | string | yes | | Short task description (3-5 words) |
| `prompt` | string | yes | | Detailed prompt |
| `subagent_type` | string | no | research | `research` (read-only) or `general` |
| `model_tier` | string | no | current | `strong`, `medium`, or `weak` |

### `todowrite`

Creates or updates a todo list for tracking multi-step work. Each item has a content string, a status, and a priority.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `todos` | array | yes | Array of `{content, status, priority}` |

### `memory`

A persistent, project-scoped scratchpad. Used to save learnings, patterns, and decisions that should survive across sessions.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `command` | string | yes | `view`, `write`, or `delete` |
| `path` | string | no | Relative path (omit to list all) |
| `content` | string | no | Content for `write` |

### `skill`

Loads a named skill, giving the agent detailed instructions for a specific kind of task. Skills are focused, reusable, and opinionated.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `name` | string | yes | Skill name |
