+++
title = "Tools"
weight = 3
[extra]
group = "Reference"
+++

# Tools

Maki ships with 18 built-in tools. This is the full reference.

## File Operations

### `bash`

Execute a bash command.
Commands run in <cwd> by default.

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `command` | string | yes |  | The bash command to execute |
| `description` | string | no |  | Short description (3-5 words) of what the command does |
| `timeout` | integer | no | 120 | Timeout in seconds |
| `workdir` | string | no | cwd | Working directory |

### `read`

Read a file or directory. Returns contents with line numbers (1-indexed).

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `limit` | integer | no | Max number of lines to read |
| `offset` | integer | no | Line number to start from (1-indexed) |
| `path` | string | yes | Absolute path to the file or directory |

### `write`

Write content to a file, replacing existing content.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `content` | string | yes | The complete file content to write |
| `path` | string | yes | Absolute path to the file |

### `edit`

Replace an exact string match in a file.

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `new_string` | string | yes |  | Replacement string |
| `old_string` | string | yes |  | Exact string to find (must match uniquely unless replace_all is true) |
| `path` | string | yes |  | Absolute path to the file |
| `replace_all` | boolean | no | false | Replace all occurrences |

### `multiedit`

Make multiple find-and-replace edits to a single file atomically.
Prefer this over edit when making multiple changes to the same file.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `edits` | array | yes | Array of edit operations to apply sequentially |
| `path` | string | yes | Absolute path to the file |

### `glob`

Find files by glob pattern.

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `path` | string | no | cwd | Directory to search in |
| `pattern` | string | yes |  | Glob pattern (e.g. **/*.rs, src/**/*.ts) |

### `grep`

Search file contents using regex.

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `context_after` | integer | no |  | Context lines after match |
| `context_before` | integer | no |  | Context lines before match |
| `include` | string | no |  | File glob filter (e.g. *.c) |
| `limit` | integer | no |  | Max match groups to return |
| `path` | string | no | cwd | Directory to search in |
| `pattern` | string | yes |  | Regex pattern |

### `index`

Return a compact skeleton of a source file: imports, type definitions, function signatures, and structure with their line numbers sorrounded by []. ~70-90% fewer tokens than reading the full file.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `path` | string | yes | Absolute path to the file |

## Execution & Control

### `batch`

Executes multiple independent tool calls concurrently to reduce round-trips.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `tool_calls` | array | yes | Array of tool calls to execute in parallel |

### `code_execution`

Execute Python code in a sandboxed interpreter. Tools are available as callable functions.

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `code` | string | yes |  | Python code to execute. Tools are async functions that return strings (not objects). You MUST await every call: `result = await read(path='/file')`. Use `await asyncio.gather(...)` for concurrency. |
| `timeout` | integer | no | 30, max 300 | Timeout in seconds |

### `question`

Use this tool when you need to ask the user questions during execution. This allows you to:
- Gather user preferences or requirements
- Clarify ambiguous instructions
- Get decisions on implementation choices as you work
- Offer choices to the user about what direction to take

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `questions` | array | yes | List of questions to ask the user |

## External

### `webfetch`

Fetch a URL and return its contents.

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `format` | string | no |  | Output format: markdown (default), text, or html |
| `timeout` | integer | no | 30, max 120 | Timeout in seconds |
| `url` | string | yes |  | URL to fetch (http:// or https://) |

### `websearch`

Search the web for real-time information using Exa AI.

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `num_results` | integer | no | 8 | Number of results to return |
| `query` | string | yes |  | Search query |

## Agent & Knowledge

### `task`

Launch an autonomous subagent to perform tasks independently. Best combined with batch.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `description` | string | yes | Short (3-5 words) description of the task |
| `model_tier` | string | no | Model tier (optional, omit to use current model, capped at current tier):<br>- "strong" (e.g. Opus): Deep reasoning, complex architecture, subtle bugs. ~5x cost of medium.<br>- "medium" (e.g. Sonnet): Balanced. Refactors, features, multi-file changes.<br>- "weak" (e.g. Haiku): Fast/cheap. Search, summarize, boilerplate, simple edits. |
| `prompt` | string | yes | Detailed task prompt for the agent |
| `subagent_type` | string | no | Subagent type: "research" (read-only, default) or "general" (can modify files) |

### `todowrite`

Create or update a structured todo list to track tasks.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `todos` | array | yes | The updated todo list |

### `memory`

Persistent, project-scoped scratchpad for learnings, patterns, decisions, and gotchas across sessions.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `command` | string | yes | Command: view, write, delete |
| `content` | string | no | File content for 'write' |
| `path` | string | no | Relative path (e.g. 'architecture.md'). Omit to list all. |

### `skill`

Load a skill by name to get detailed instructions.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `name` | string | yes | Name of the skill to load |