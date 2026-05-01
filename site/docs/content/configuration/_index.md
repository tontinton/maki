+++
title = "Configuration"
weight = 2
[extra]
group = "Getting Started"
+++

# Configuration

Settings go in `init.lua`, a Lua script that calls `maki.setup()`. Same language as plugins.

Two places, both optional:

- **Global**: `~/.config/maki/init.lua`
- **Project**: `.maki/init.lua` (relative to your working directory)

When both exist, project settings override global ones. Neither file is required.

## Example

```lua
maki.setup({
    ui = {
        splash_animation = true,
        mouse_scroll_lines = 5,
        tool_output_lines = {
            bash = 8,
            read = 5,
        },
    },
    agent = {
        bash_timeout_secs = 180,
        max_output_lines = 3000,
    },
    provider = {
        default_model = "anthropic/claude-sonnet-4-6",
    },
    storage = {
        max_log_files = 5,
    },
    index = {
        max_file_size_mb = 4,
    },
})
```

All fields are optional. Typos in field names cause an error right away.

`maki.setup()` can only be called once per init.lua.

## Full Reference

### Top-level

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `always_yolo` | bool | `false` | Start every session with YOLO mode (skip permission prompts, deny rules still apply) |

### `ui`

| Field | Type | Default | Min | Description |
|-------|------|---------|-----|-------------|
| `splash_animation` | bool | `true` | - | Show splash animation on startup |
| `flash_duration_ms` | u64 | `1500` | - | Duration of flash messages (ms) |
| `typewriter_ms_per_char` | u64 | `4` | - | Typewriter effect speed (ms/char) |
| `mouse_scroll_lines` | u32 | `3` | 1 | Lines per mouse wheel scroll |

### `ui.tool_output_lines`

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

### `agent`

| Field | Type | Default | Min | Description |
|-------|------|---------|-----|-------------|
| `max_output_bytes` | usize | `51200` | 1024 | Max tool output size (bytes) |
| `max_output_lines` | usize | `2000` | 10 | Max tool output lines |
| `max_response_bytes` | usize | `5242880` | 1024 | Max LLM response size (bytes) |
| `max_line_bytes` | usize | `500` | 80 | Max bytes per line before truncation |
| `bash_timeout_secs` | u64 | `120` | 5 | Bash command timeout (seconds) |
| `code_execution_timeout_secs` | u64 | `30` | 5 | Code execution timeout (seconds) |
| `max_continuation_turns` | u32 | `3` | 1 | Max automatic continuation turns |
| `compaction_buffer` | u32 | `40000` | 1000 | Token buffer reserved during compaction |
| `search_result_limit` | usize | `100` | 10 | Max results from grep/glob searches |
| `interpreter_max_memory_mb` | usize | `50` | 10 | Memory limit for code interpreter (MB) |

### `provider`

| Field | Type | Default | Min | Description |
|-------|------|---------|-----|-------------|
| `default_model` | String | `none` | - | Default model identifier (e.g. `anthropic/claude-sonnet-4-6`) |
| `connect_timeout_secs` | u64 | `10` | 1 | HTTP connect timeout (seconds) |
| `low_speed_timeout_secs` | u64 | `30` | 1 | Low speed timeout (seconds with less than 1 byte received) |
| `stream_timeout_secs` | u64 | `300` | 10 | Streaming response timeout (seconds) |

### `storage`

| Field | Type | Default | Min | Description |
|-------|------|---------|-----|-------------|
| `max_log_bytes_mb` | u64 | `200` | 1 | Max total log size (MB) |
| `max_log_files` | u32 | `10` | 1 | Max number of log files to keep |
| `input_history_size` | usize | `100` | 10 | Number of input history entries to retain |

### `index`

| Field | Type | Default | Min | Description |
|-------|------|---------|-----|-------------|
| `max_file_size_mb` | u64 | `2` | 1 | Max file size for indexing (MB) |

## Tools

The `tools` table lets you turn tools on or off. By default `index`, `webfetch`, and `websearch` are on. `bash` is off by default.

```lua
maki.setup({
    tools = {
        bash = { enabled = true },
        websearch = { enabled = false },
    },
})
```

## Validation

If a value is below its minimum, Maki shows a `ConfigError` with the field name, value, and minimum.

## Directory layout

Maki uses XDG directories on Linux and macOS:

| Purpose | Path |
|---------|------|
| Config | `~/.config/maki/` (init.lua, permissions.toml, mcp.toml) |
| Data | `~/.local/share/maki/` |
| Logs | `~/.local/logs/maki/` |
| State | `~/.local/state/maki/` |

`~/.maki/` is checked as a legacy fallback.

## Personal Instructions

On top of `AGENTS.md`, you can add your own instructions in two places:

- `AGENTS.local.md` at project root for per-project preferences (gitignored)
- `~/.config/maki/AGENTS.md` for preferences that apply to all projects

Both are added to the system prompt at the start of every session.

## Migrating from config.toml

Still have a `config.toml`? Here is how to switch over.

**Rename your config files:**

```
~/.config/maki/config.toml  ->  ~/.config/maki/init.lua
.maki/config.toml           ->  .maki/init.lua
```

**Wrap the content in `maki.setup()`:**

Before:

```toml
[agent]
bash_timeout_secs = 180
```

After:

```lua
maki.setup({
    agent = { bash_timeout_secs = 180 },
})
```

Same field names, just Lua syntax instead of TOML.

**Move MCP sections to `mcp.toml`.**

- `~/.config/maki/mcp.toml` (global)
- `.maki/mcp.toml` (per-project)

Same format, just a different file. See [MCP](/docs/mcp/).

**Permissions stay in `permissions.toml`.**
