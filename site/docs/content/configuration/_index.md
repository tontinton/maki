+++
title = "Configuration"
weight = 2
+++

# Configuration

Maki uses TOML config files in two places:

- **Global**: `~/.config/maki/config.toml`
- **Project**: `.maki/config.toml` (relative to your working directory)

Project settings win over global ones, field by field. Neither file needs to exist; everything has a default.

## Example

```toml
[ui]
splash_animation = true
mouse_scroll_lines = 5

[ui.tool_output_lines]
bash = 8
read = 5

[agent]
bash_timeout_secs = 180
max_output_lines = 3000

[provider]
default_model = "anthropic/claude-sonnet-4-6"

[storage]
max_log_files = 5

[index]
max_file_size_mb = 4
```

## Full Reference

### Top-level

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `always_yolo` | bool | `false` | Start every session with YOLO mode (skip permission prompts, deny rules still apply) |

### `[ui]`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `splash_animation` | bool | `true` | Show splash animation on startup |
| `flash_duration_ms` | u64 | `1500` | Duration of flash messages (ms) |
| `typewriter_ms_per_char` | u64 | `4` | Typewriter effect speed (ms/char) |
| `mouse_scroll_lines` | u32 | `3` | Lines per mouse wheel scroll (min: 1) |

### `[ui.tool_output_lines]`

How many lines of output to show per tool in the UI. All values are `usize` with a minimum of 1.

| Field | Default |
|-------|---------|
| `bash` | 5 |
| `code_execution` | 5 |
| `task` | 5 |
| `index` | 3 |
| `grep` | 3 |
| `read` | 3 |
| `write` | 7 |
| `web` | 3 |
| `other` | 3 |

### `[agent]`

| Field | Type | Default | Min | Description |
|-------|------|---------|-----|-------------|
| `max_output_bytes` | usize | `51200` | 1024 | Max tool output size (bytes) |
| `max_output_lines` | usize | `2000` | 10 | Max tool output lines |
| `max_response_bytes` | usize | `5242880` | 1024 | Max LLM response size (bytes) |
| `max_line_bytes` | usize | `500` | 80 | Max bytes per output line |
| `bash_timeout_secs` | u64 | `120` | 5 | Bash command timeout |
| `code_execution_timeout_secs` | u64 | `30` | 5 | Python sandbox timeout |
| `max_continuation_turns` | u32 | `3` | 1 | Max continuation turns |
| `compaction_buffer` | u32 | `30000` | 1000 | Context compaction buffer |
| `search_result_limit` | usize | `100` | 10 | Max search results |
| `interpreter_max_memory_mb` | usize | `50` | 10 | Python interpreter memory limit (MB) |

### `[provider]`

| Field | Type | Default | Min | Description |
|-------|------|---------|-----|-------------|
| `default_model` | string | none | n/a | Default LLM model (e.g. `anthropic/claude-sonnet-4-6`) |
| `connect_timeout_secs` | u64 | `10` | 1 | API connection timeout |
| `stream_timeout_secs` | u64 | `300` | 10 | Streaming response timeout |

### `[storage]`

| Field | Type | Default | Min | Description |
|-------|------|---------|-----|-------------|
| `max_log_bytes_mb` | u64 | `200` | 1 | Max log file size (MB) |
| `max_log_files` | u32 | `10` | 1 | Max number of log files |
| `input_history_size` | usize | `100` | 10 | REPL input history entries |

### `[index]`

| Field | Type | Default | Min | Description |
|-------|------|---------|-----|-------------|
| `max_file_size_mb` | u64 | `2` | 1 | Max file size for tree-sitter indexing (MB) |

## Validation

Numeric fields are validated against their minimums on load. A value below the minimum raises a `ConfigError` with the section, field, value, and minimum. Invalid TOML logs a warning and falls back to defaults.
